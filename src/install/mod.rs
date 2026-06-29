//! System-module installation and verification for `nix-agent apply`.
//!
//! `plan` produces a validated module in an isolated sandbox. `apply` must put
//! that module where the *running* NixOS system will actually evaluate it:
//!
//!   * generated module → `/etc/nixos/modules/ai-generated/<plan-id>.nix`
//!   * aggregator       → `/etc/nixos/modules/ai-generated/default.nix`
//!   * root config      → `/etc/nixos/configuration.nix` (must import the dir)
//!
//! This module owns four concerns, each independently testable against temp
//! directories (no `/etc/nixos`, no real `nixos-rebuild`):
//!   1. normalizing model output into a valid module function (`{ config, pkgs,
//!      lib, ... }:`),
//!   2. registering the module in the aggregator's `imports` (idempotent, backed
//!      up, atomic),
//!   3. verifying the root config already imports the aggregator directory,
//!   4. post-rebuild verification that expected binaries appeared.
//!
//! The actual rebuild is delegated to the existing [`SystemBuilder`] seam so it
//! stays mockable; the binary check is delegated to [`BinaryProbe`].

use std::io;
use std::path::{Path, PathBuf};

use rnix::ast::{Expr, Param};
use rowan::ast::AstNode;

use crate::execution::{
    parse_build_stderr, BuildMode, BuildOutput, EngineError, NixBuildError, SystemBuilder,
};
use crate::plan::Plan;

/// Default system configuration directory for non-flake NixOS.
pub const DEFAULT_CONFIG_DIR: &str = "/etc/nixos";
/// Import path the root config must contain, relative to `config_dir`.
pub const AGGREGATOR_IMPORT: &str = "./modules/ai-generated";
/// The canonical module function header injected when wrapping bare attrsets.
pub const MODULE_HEADER: &str = "{ config, pkgs, lib, ... }:";
/// How many autonomous repair cycles the self-healing loop attempts before it
/// gives up and rolls back.
pub const MAX_REPAIR_ATTEMPTS: usize = 3;

// ── System apply configuration ──────────────────────────────────────────────

/// Where, on the real system, an applied module is installed and activated.
/// Every path is logged so the operator can see exactly what `apply` touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NixosApplyConfig {
    pub config_dir: PathBuf,
    pub generated_modules_dir: PathBuf,
    pub aggregator_path: PathBuf,
    pub root_config_path: PathBuf,
    pub rebuild_mode: BuildMode,
}

impl Default for NixosApplyConfig {
    fn default() -> Self {
        Self::for_config_dir(PathBuf::from(DEFAULT_CONFIG_DIR), None)
    }
}

impl NixosApplyConfig {
    /// Derive all paths from a single `config_dir`, optionally overriding the
    /// root config file. Used directly by tests with a temp dir.
    pub fn for_config_dir(config_dir: PathBuf, root_config: Option<PathBuf>) -> Self {
        let generated_modules_dir = config_dir.join("modules").join("ai-generated");
        let aggregator_path = generated_modules_dir.join("default.nix");
        let root_config_path = root_config.unwrap_or_else(|| config_dir.join("configuration.nix"));
        Self {
            config_dir,
            generated_modules_dir,
            aggregator_path,
            root_config_path,
            rebuild_mode: BuildMode::Test,
        }
    }

    /// Resolve from the environment: `NIX_AGENT_CONFIG_DIR` (else `/etc/nixos`)
    /// and `NIX_AGENT_CONFIG` (else `<config_dir>/configuration.nix`). Unlike the
    /// sandbox `AppConfig` (which defaults `config_dir` to `.`), `apply` targets
    /// the real system, so the default here is `/etc/nixos`.
    pub fn resolve() -> Self {
        let config_dir =
            env_path("NIX_AGENT_CONFIG_DIR").unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_DIR));
        Self::for_config_dir(config_dir, env_path("NIX_AGENT_CONFIG"))
    }

    /// Installed path for a module with the given plan id.
    pub fn module_path_for(&self, plan_id: &str) -> PathBuf {
        self.generated_modules_dir.join(format!("{plan_id}.nix"))
    }

    /// Human string for the rebuild command that will run.
    pub fn rebuild_command(&self, with_sudo: bool) -> String {
        let prefix = if with_sudo { "sudo " } else { "" };
        format!("{prefix}nixos-rebuild {}", self.rebuild_mode.subcommand())
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum InstallError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    /// The model output could not be turned into a valid module function.
    Normalize(String),
    /// `/etc/nixos/configuration.nix` is missing entirely.
    RootConfigMissing(PathBuf),
    /// The root config does not import the aggregator directory.
    RootNotImporting {
        root_config: PathBuf,
        import: String,
        rebuild: String,
    },
    /// `nixos-rebuild` ran but rejected the configuration. The parsed diagnostic
    /// is boxed to keep `InstallError` small (it is returned by value widely).
    Rebuild {
        parsed: Option<Box<NixBuildError>>,
        stderr: String,
    },
    /// Infrastructure failure spawning/awaiting the rebuild.
    Engine(EngineError),
    /// The self-healing backend failed to produce a replacement module.
    Repair(anyhow::Error),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "I/O error on {}: {}", path.display(), source),
            Self::Normalize(m) => write!(f, "could not normalize generated module: {m}"),
            Self::RootConfigMissing(p) => write!(
                f,
                "root NixOS config not found at {} — cannot verify it imports the generated modules",
                p.display()
            ),
            Self::RootNotImporting {
                root_config,
                import,
                rebuild,
            } => write!(
                f,
                "generated modules directory is not imported by {root}\n\n\
                 Add this once to your imports:\n\n  {import}\n\n\
                 Then rerun:\n\n  {rebuild}",
                root = root_config.display(),
            ),
            Self::Rebuild { parsed, stderr } => {
                writeln!(f, "nixos-rebuild rejected the configuration")?;
                if let Some(e) = parsed {
                    write!(f, "  {:?}: {}", e.kind, e.message)?;
                    if let Some(loc) = &e.location {
                        write!(f, " ({}:{}:{})", loc.file.display(), loc.line, loc.column)?;
                    }
                } else {
                    let lines: Vec<&str> = stderr.lines().collect();
                    let start = lines.len().saturating_sub(8);
                    write!(f, "{}", lines[start..].join("\n"))?;
                }
                Ok(())
            }
            Self::Engine(e) => write!(f, "{e}"),
            Self::Repair(e) => write!(f, "self-healing regeneration failed: {e}"),
        }
    }
}

impl std::error::Error for InstallError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Engine(e) => Some(e),
            _ => None,
        }
    }
}

// ── Self-healing seam ───────────────────────────────────────────────────────

/// Regenerates a corrected module from the failed source and the build error.
/// Implemented by an adapter over the local LLM backend; mocked in tests so the
/// healing loop is exercised without inference.
#[allow(async_fn_in_trait)]
pub trait ModuleRepairer {
    async fn repair(&mut self, failed_module: &str, stderr: &str) -> anyhow::Result<String>;
}

/// Build the strict, prose-free repair prompt fed to the inference backend. The
/// backend wraps this in its own ChatML template before generation.
pub fn build_repair_prompt(failed_module: &str, stderr: &str) -> String {
    format!(
        "[SYSTEM] You are an expert NixOS fixer. The module you just generated failed to compile.\n\
         [ERROR] {stderr}\n\
         [SOURCE] {failed_module}\n\
         Fix the error by regenerating the valid Nix module code. Maintain the exact required \
         function header structure. Do not include prose."
    )
}

/// Progress emitted by the self-healing loop, so a front-end can show the
/// `[Self-Healing]` lines without the loop owning any UI.
#[derive(Debug)]
pub enum RepairEvent {
    /// A rebuild failed; repair `attempt`/`max` is about to start.
    Attempt {
        attempt: usize,
        max: usize,
        error: Option<Box<NixBuildError>>,
    },
    /// A regenerated module passed the AST gate and is being re-built.
    Rebuilding { attempt: usize },
}

/// What [`activate`] did on success.
#[derive(Debug, Clone, Default)]
pub struct ActivateReport {
    /// Number of autonomous repair cycles it took (0 = built first try).
    pub healing_attempts: usize,
}

// ── Module normalization ────────────────────────────────────────────────────

/// Turn raw model output into a valid NixOS module *function*.
///
///   * a bare attrset body (`{ environment.systemPackages = ...; }`) is wrapped
///     with [`MODULE_HEADER`];
///   * an existing module function is preserved as-is;
///   * a module function that references `pkgs` but does not bind it has `pkgs`
///     injected into its argument pattern, preventing `undefined variable 'pkgs'`.
///
/// The result is always re-parsed; if it is not valid Nix an error is returned
/// rather than installing something that would fail to evaluate.
pub fn normalize_module(raw: &str) -> Result<String, InstallError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(InstallError::Normalize("module is empty".to_owned()));
    }

    let needs_pkgs = references_pkgs(trimmed);
    let parse = rnix::Root::parse(trimmed);

    match parse.tree().expr() {
        Some(Expr::Lambda(lambda)) => match lambda.param() {
            Some(Param::Pattern(pat)) => {
                let binds_pkgs = pattern_binds(&pat, "pkgs");
                let result = if needs_pkgs && !binds_pkgs {
                    inject_pkgs(trimmed, &pat)
                } else {
                    trimmed.to_owned()
                };
                ensure_parses(&result)?;
                Ok(ensure_trailing_nl(result))
            }
            // `x: <body>` style: no attrset pattern to expose `pkgs` through.
            _ => {
                if needs_pkgs {
                    return Err(InstallError::Normalize(
                        "module references `pkgs` but its argument is not an attrset pattern that \
                         can receive it"
                            .to_owned(),
                    ));
                }
                ensure_parses(trimmed)?;
                Ok(ensure_trailing_nl(trimmed.to_owned()))
            }
        },
        // Bare attrset, `with ... ;`, `let ... in`, etc. → wrap as a function.
        _ => {
            let wrapped = format!("{MODULE_HEADER}\n\n{trimmed}\n");
            ensure_parses(&wrapped)?;
            Ok(wrapped)
        }
    }
}

/// Whether `src` uses `pkgs` (as `with pkgs` or `pkgs.<x>`), excluding longer
/// identifiers like `nixpkgs` or `pkgsCross`.
pub fn references_pkgs(src: &str) -> bool {
    let bytes = src.as_bytes();
    for (i, _) in src.match_indices("pkgs") {
        let before_ok = i == 0 || !is_ident_char(bytes[i - 1]);
        let after = bytes.get(i + 4).copied();
        let after_is_pkgs_use = match after {
            Some(b'.') => true,                       // pkgs.foo
            Some(c) => !is_ident_char(c),             // standalone token: `with pkgs;`
            None => true,
        };
        if before_ok && after_is_pkgs_use {
            return true;
        }
    }
    false
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'\'')
}

fn pattern_binds(pat: &rnix::ast::Pattern, name: &str) -> bool {
    pat.pat_entries().any(|e| {
        e.ident()
            .and_then(|i| i.ident_token())
            .map(|t| t.text() == name)
            .unwrap_or(false)
    })
}

/// Rebuild the lambda pattern with `pkgs` (and an ellipsis) added, preserving the
/// existing entries verbatim, then splice it back into the source.
fn inject_pkgs(src: &str, pat: &rnix::ast::Pattern) -> String {
    let mut entries: Vec<String> = pat
        .pat_entries()
        .map(|e| e.syntax().text().to_string().trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    entries.push("pkgs".to_owned());
    let new_pat = format!("{{ {}, ... }}", entries.join(", "));

    let range = pat.syntax().text_range();
    let start = usize::from(range.start());
    let end = usize::from(range.end());
    format!("{}{}{}", &src[..start], new_pat, &src[end..])
}

fn ensure_parses(src: &str) -> Result<(), InstallError> {
    crate::ast::NixFile::from_source("normalized.nix", src.to_owned())
        .map(|_| ())
        .map_err(|e| InstallError::Normalize(format!("result is not valid Nix: {e}")))
}

fn ensure_trailing_nl(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

// ── Aggregator registration ─────────────────────────────────────────────────

/// Outcome of registering a module in the aggregator, carrying what is needed to
/// roll back on a later rebuild failure.
#[derive(Debug, Clone)]
pub struct AggregatorChange {
    /// `true` if the aggregator was written (false = the import was already there).
    pub changed: bool,
    /// Prior aggregator contents, for in-memory restore. `None` if we created it.
    pub prior: Option<String>,
    /// Path of the `.bak` written before modifying an existing aggregator.
    pub backup: Option<PathBuf>,
}

/// Ensure the aggregator at `aggregator_path` imports `./<plan_id>.nix` exactly
/// once. Creates it if missing, preserves existing imports, de-duplicates, backs
/// up an existing file to `<name>.bak`, and writes atomically.
pub fn register_in_aggregator(
    aggregator_path: &Path,
    plan_id: &str,
) -> Result<AggregatorChange, InstallError> {
    if let Some(parent) = aggregator_path.parent() {
        create_dir_all(parent)?;
    }

    let prior = read_opt(aggregator_path)?;
    let mut imports = prior.as_deref().map(extract_imports).unwrap_or_default();

    let target = format!("./{plan_id}.nix");
    if imports.iter().any(|i| import_eq(i, &target)) {
        return Ok(AggregatorChange {
            changed: false,
            prior,
            backup: None,
        });
    }

    // Back up an existing aggregator before mutating it.
    let backup = match &prior {
        Some(content) => {
            let bak = with_suffix(aggregator_path, ".bak");
            write_file(&bak, content)?;
            Some(bak)
        }
        None => None,
    };

    imports.push(target);
    dedupe_imports(&mut imports);
    atomic_write(aggregator_path, &render_aggregator(&imports))?;

    Ok(AggregatorChange {
        changed: true,
        prior,
        backup,
    })
}

/// Restore the aggregator to its pre-registration state (used on rebuild failure).
pub fn restore_aggregator(
    aggregator_path: &Path,
    change: &AggregatorChange,
) -> Result<(), InstallError> {
    match &change.prior {
        Some(content) => atomic_write(aggregator_path, content),
        // We created it; remove it again.
        None => remove_if_exists(aggregator_path),
    }
}

/// Extract `./*.nix`-style import tokens from an aggregator/config `imports = [ … ]`.
fn extract_imports(content: &str) -> Vec<String> {
    let Some(kpos) = content.find("imports") else {
        return Vec::new();
    };
    let after = &content[kpos..];
    let Some(lb) = after.find('[') else {
        return Vec::new();
    };
    let rest = &after[lb + 1..];
    let Some(rb) = rest.find(']') else {
        return Vec::new();
    };
    rest[..rb]
        .split(|c: char| c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty() && (t.starts_with("./") || t.ends_with(".nix")))
        .map(str::to_owned)
        .collect()
}

fn render_aggregator(imports: &[String]) -> String {
    let mut s = String::from("{ ... }:\n\n{\n  imports = [\n");
    for imp in imports {
        s.push_str("    ");
        s.push_str(imp);
        s.push('\n');
    }
    s.push_str("  ];\n}\n");
    s
}

/// Compare imports ignoring a leading `./`.
fn import_eq(a: &str, b: &str) -> bool {
    a.trim_start_matches("./") == b.trim_start_matches("./")
}

fn dedupe_imports(imports: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    imports.retain(|i| seen.insert(i.trim_start_matches("./").to_owned()));
}

// ── Root config verification ────────────────────────────────────────────────

/// Verify the root config imports the aggregator directory. Returns an
/// actionable error otherwise (and never mutates the file).
pub fn verify_root_config(cfg: &NixosApplyConfig) -> Result<(), InstallError> {
    let content = match std::fs::read_to_string(&cfg.root_config_path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(InstallError::RootConfigMissing(cfg.root_config_path.clone()))
        }
        Err(source) => {
            return Err(InstallError::Io {
                path: cfg.root_config_path.clone(),
                source,
            })
        }
    };

    if root_imports_aggregator(&content) {
        Ok(())
    } else {
        Err(InstallError::RootNotImporting {
            root_config: cfg.root_config_path.clone(),
            import: AGGREGATOR_IMPORT.to_owned(),
            rebuild: cfg.rebuild_command(true),
        })
    }
}

/// `true` if any non-comment line references `modules/ai-generated`.
pub fn root_imports_aggregator(content: &str) -> bool {
    content.lines().any(|line| {
        let t = line.trim();
        !t.starts_with('#') && t.contains("modules/ai-generated")
    })
}

// ── Package / binary verification ───────────────────────────────────────────

/// Extract simple package names from `environment.systemPackages = with pkgs; [ … ];`
/// (or a list of `pkgs.<name>`). Best-effort: declarative names only, never code.
pub fn parse_system_packages(src: &str) -> Vec<String> {
    const KEY: &str = "environment.systemPackages";
    let Some(kpos) = src.find(KEY) else {
        return Vec::new();
    };
    let after = &src[kpos + KEY.len()..];
    let Some(lb) = after.find('[') else {
        return Vec::new();
    };
    let rest = &after[lb + 1..];
    let Some(rb) = rest.find(']') else {
        return Vec::new();
    };

    rest[..rb]
        .split(|c: char| c.is_whitespace())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.strip_prefix("pkgs.").unwrap_or(t))
        .filter(|t| is_package_ident(t))
        .map(str::to_owned)
        .collect()
}

fn is_package_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

/// Map a package name to the binary it is expected to install.
pub fn package_to_binary(package: &str) -> String {
    match package {
        "ripgrep" => "rg",
        "fd" => "fd",
        other => other,
    }
    .to_owned()
}

/// Resolves binaries on the activated system. Abstracted so tests can point at a
/// temp directory instead of `/run/current-system/sw/bin`.
pub trait BinaryProbe {
    /// Return the absolute path if `binary` exists on the activated system.
    fn resolve(&self, binary: &str) -> Option<PathBuf>;
}

/// Default probe: looks under `/run/current-system/sw/bin`.
#[derive(Debug, Clone)]
pub struct FsBinaryProbe {
    pub bin_dir: PathBuf,
}

impl Default for FsBinaryProbe {
    fn default() -> Self {
        Self {
            bin_dir: PathBuf::from("/run/current-system/sw/bin"),
        }
    }
}

impl BinaryProbe for FsBinaryProbe {
    fn resolve(&self, binary: &str) -> Option<PathBuf> {
        let p = self.bin_dir.join(binary);
        p.exists().then_some(p)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBinary {
    pub package: String,
    pub binary: String,
}

#[derive(Debug, Clone, Default)]
pub struct BinaryReport {
    pub found: Vec<PathBuf>,
    pub missing: Vec<MissingBinary>,
}

/// Check each package's expected binary against the probe.
pub fn verify_binaries<P: BinaryProbe>(probe: &P, packages: &[String]) -> BinaryReport {
    let mut report = BinaryReport::default();
    for pkg in packages {
        let binary = package_to_binary(pkg);
        match probe.resolve(&binary) {
            Some(path) => report.found.push(path),
            None => report.missing.push(MissingBinary {
                package: pkg.clone(),
                binary,
            }),
        }
    }
    report
}

// ── Orchestration ───────────────────────────────────────────────────────────

/// What `register_module` produced — paths plus the rollback handles.
#[derive(Debug, Clone)]
pub struct RegisterReport {
    pub module_path: PathBuf,
    pub aggregator_path: PathBuf,
    pub root_config_path: PathBuf,
    pub packages: Vec<String>,
    /// Prior installed module contents, if this id was already applied.
    pub module_prior: Option<String>,
    pub aggregator_change: AggregatorChange,
    /// Plan id + prompt, kept so the self-healing loop can re-render the module's
    /// provenance header after a regeneration.
    pub plan_id: String,
    pub prompt: String,
    /// The normalized module body that was installed (no provenance header) —
    /// the starting point for the first repair prompt.
    pub normalized_source: String,
}

/// Phase 2 of apply: normalize, verify the root config, write the module, and
/// register it in the aggregator. Performs no rebuild. Verifying the root config
/// *before* writing means a misconfigured system is never mutated.
pub fn register_module(
    cfg: &NixosApplyConfig,
    plan: &Plan,
) -> Result<RegisterReport, InstallError> {
    let normalized = normalize_module(&plan.module_source)?;
    let packages = parse_system_packages(&normalized);

    // Fail before touching the filesystem if the root config isn't wired up.
    verify_root_config(cfg)?;

    create_dir_all(&cfg.generated_modules_dir)?;

    let module_path = cfg.module_path_for(&plan.id);
    let module_prior = read_opt(&module_path)?;
    atomic_write(
        &module_path,
        &render_module(&plan.id, &plan.prompt, &normalized),
    )?;

    let aggregator_change = register_in_aggregator(&cfg.aggregator_path, &plan.id)?;

    Ok(RegisterReport {
        module_path,
        aggregator_path: cfg.aggregator_path.clone(),
        root_config_path: cfg.root_config_path.clone(),
        packages,
        module_prior,
        aggregator_change,
        plan_id: plan.id.clone(),
        prompt: plan.prompt.clone(),
        normalized_source: normalized,
    })
}

/// Phase 3 of apply: run the rebuild with an autonomous self-healing loop.
///
/// On a failed `nixos-rebuild test`, instead of rolling back immediately, the
/// build error is fed back to the inference backend (via `repairer`), the
/// regenerated module is re-normalized + AST-gated, rewritten, and re-built —
/// up to [`MAX_REPAIR_ATTEMPTS`] times. Only if every repair fails is the
/// transactional rollback executed (aggregator restored, broken module moved to
/// `failed/`) and a terminal error returned. Never reports success on a failed
/// rebuild.
pub async fn activate<B, R>(
    cfg: &NixosApplyConfig,
    builder: &B,
    repairer: &mut R,
    reg: &RegisterReport,
    mut on_event: impl FnMut(RepairEvent),
) -> Result<ActivateReport, InstallError>
where
    B: SystemBuilder,
    R: ModuleRepairer,
{
    // The currently-installed module body (no provenance), starting from what
    // `register_module` wrote and updated after each successful regeneration.
    let mut module = reg.normalized_source.clone();
    let mut output = builder
        .build(&reg.module_path)
        .await
        .map_err(InstallError::Engine)?;
    let mut repairs = 0usize;

    while !output.success {
        if repairs >= MAX_REPAIR_ATTEMPTS {
            // Exhausted: transactional rollback + terminal failure.
            rollback(cfg, reg)?;
            return Err(InstallError::Rebuild {
                parsed: parse_build_stderr(&output.stderr).map(Box::new),
                stderr: output.stderr,
            });
        }

        repairs += 1;
        on_event(RepairEvent::Attempt {
            attempt: repairs,
            max: MAX_REPAIR_ATTEMPTS,
            error: parse_build_stderr(&output.stderr).map(Box::new),
        });

        // Ask the backend for a corrected module.
        let raw = repairer
            .repair(&module, &output.stderr)
            .await
            .map_err(InstallError::Repair)?;

        match normalize_module(&raw) {
            Ok(fixed) => {
                // Passed the AST gate: install it and re-build.
                atomic_write(
                    &reg.module_path,
                    &render_module(&reg.plan_id, &reg.prompt, &fixed),
                )?;
                module = fixed;
                on_event(RepairEvent::Rebuilding { attempt: repairs });
                output = builder
                    .build(&reg.module_path)
                    .await
                    .map_err(InstallError::Engine)?;
            }
            Err(InstallError::Normalize(msg)) => {
                // Regeneration didn't even parse. Keep the old module on disk and
                // synthesize a failure so the next repair prompt sees this; the
                // loop stays bounded by `repairs`.
                output = BuildOutput {
                    success: false,
                    exit_code: Some(1),
                    stdout: String::new(),
                    stderr: format!("error: regenerated module did not pass the AST gate: {msg}"),
                };
            }
            Err(e) => return Err(e),
        }
    }

    Ok(ActivateReport {
        healing_attempts: repairs,
    })
}

/// Transactional rollback used when self-healing is exhausted: restore the
/// aggregator to its pre-apply state, then restore the prior module (re-apply)
/// or quarantine the broken one (first apply).
fn rollback(cfg: &NixosApplyConfig, reg: &RegisterReport) -> Result<(), InstallError> {
    restore_aggregator(&reg.aggregator_path, &reg.aggregator_change)?;
    match &reg.module_prior {
        Some(prior) => atomic_write(&reg.module_path, prior)?,
        None => {
            quarantine_module(cfg, &reg.module_path)?;
        }
    }
    Ok(())
}

/// Move a failed module into `<generated>/failed/<name>` so it is out of the
/// aggregator's reach but preserved for inspection. Returns the new path.
fn quarantine_module(cfg: &NixosApplyConfig, module_path: &Path) -> Result<PathBuf, InstallError> {
    let failed_dir = cfg.generated_modules_dir.join("failed");
    create_dir_all(&failed_dir)?;
    let dest = failed_dir.join(
        module_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("module.nix")),
    );
    std::fs::rename(module_path, &dest).map_err(|source| InstallError::Io {
        path: module_path.to_path_buf(),
        source,
    })?;
    Ok(dest)
}

/// Prepend a short, declarative provenance comment (valid Nix) to the module.
fn render_module(plan_id: &str, prompt: &str, module: &str) -> String {
    format!(
        "# Generated by nix-agent — do not edit by hand.\n\
         # plan-id: {plan_id}\n\
         # prompt: {prompt}\n\n\
         {module}",
        prompt = prompt.replace('\n', " "),
    )
}

// ── Filesystem helpers ──────────────────────────────────────────────────────

fn create_dir_all(dir: &Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dir).map_err(|source| InstallError::Io {
        path: dir.to_path_buf(),
        source,
    })
}

fn read_opt(path: &Path) -> Result<Option<String>, InstallError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(InstallError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn write_file(path: &Path, content: &str) -> Result<(), InstallError> {
    std::fs::write(path, content).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Write `content` to `path` via a sibling temp file + rename, so a reader never
/// observes a half-written file.
fn atomic_write(path: &Path, content: &str) -> Result<(), InstallError> {
    let tmp = with_suffix(path, &format!(".tmp.{}", std::process::id()));
    write_file(&tmp, content)?;
    std::fs::rename(&tmp, path).map_err(|source| InstallError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn remove_if_exists(path: &Path) -> Result<(), InstallError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(InstallError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Append a literal suffix to a path's file name (`default.nix` + `.bak`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests;

//! Unit + integration-style tests for system-module installation. Everything
//! runs against temp directories — no `/etc/nixos`, no real `nixos-rebuild`.

use super::*;
use crate::execution::{BuildOutput, EngineError, SystemBuilder};
use crate::plan::Plan;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

// ── temp dir ────────────────────────────────────────────────────────────────

struct TempDir {
    path: PathBuf,
}
impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        path.push(format!("nix-agent-install-{tag}-{nanos}"));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

// A builder with a fixed outcome (stands in for `nixos-rebuild`).
struct ScriptedBuilder {
    output: BuildOutput,
}
impl SystemBuilder for ScriptedBuilder {
    async fn build(&self, _staging_path: &Path) -> Result<BuildOutput, EngineError> {
        Ok(self.output.clone())
    }
}

fn ok_build() -> BuildOutput {
    BuildOutput {
        success: true,
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    }
}
fn fail_build() -> BuildOutput {
    BuildOutput {
        success: false,
        exit_code: Some(1),
        stdout: String::new(),
        stderr: "error: undefined variable 'foo' at /etc/nixos/x.nix:3:5\n".to_owned(),
    }
}

fn plan(id: &str, module: &str) -> Plan {
    Plan {
        id: id.to_owned(),
        prompt: "add ripgrep and fd".to_owned(),
        module_source: module.to_owned(),
    }
}

const BARE_ATTRSET: &str = "{\n  environment.systemPackages = with pkgs; [\n    ripgrep\n    fd\n  ];\n}\n";
const FUNCTION_WITH_PKGS: &str =
    "{ config, pkgs, lib, ... }:\n{\n  environment.systemPackages = with pkgs; [ ripgrep fd ];\n}\n";
const FUNCTION_NO_PKGS: &str =
    "{ config, lib, ... }:\n{\n  environment.systemPackages = with pkgs; [ ripgrep ];\n}\n";

/// Build a temp config dir whose root config imports the aggregator directory.
fn config_with_root_import(dir: &Path) -> NixosApplyConfig {
    let cfg = NixosApplyConfig::for_config_dir(dir.to_path_buf(), None);
    std::fs::write(
        &cfg.root_config_path,
        "{ config, pkgs, ... }:\n{\n  imports = [\n    ./hardware-configuration.nix\n    ./modules/ai-generated\n  ];\n}\n",
    )
    .unwrap();
    cfg
}

// ── normalization ───────────────────────────────────────────────────────────

#[test]
fn normalizes_bare_attrset_into_function_module() {
    let out = normalize_module(BARE_ATTRSET).unwrap();
    assert!(out.starts_with(MODULE_HEADER), "got: {out}");
    assert!(out.contains("{ config, pkgs, lib, ... }:"));
    assert!(out.contains("environment.systemPackages"));
    // Must be valid Nix.
    assert!(crate::ast::NixFile::from_source("m.nix", out).is_ok());
}

#[test]
fn preserves_already_valid_function_module() {
    let out = normalize_module(FUNCTION_WITH_PKGS).unwrap();
    // Header preserved verbatim; not double-wrapped.
    assert!(out.trim_start().starts_with("{ config, pkgs, lib, ... }:"));
    assert_eq!(out.matches("config, pkgs, lib").count(), 1);
}

#[test]
fn injects_pkgs_when_module_uses_but_does_not_bind_it() {
    let out = normalize_module(FUNCTION_NO_PKGS).unwrap();
    // pkgs must now be in the argument pattern.
    let header = out.lines().next().unwrap();
    assert!(header.contains("pkgs"), "header missing pkgs: {header}");
    assert!(header.contains("config") && header.contains("lib"));
    assert!(crate::ast::NixFile::from_source("m.nix", out).is_ok());
}

#[test]
fn references_pkgs_excludes_nixpkgs() {
    assert!(references_pkgs("with pkgs; [ vim ]"));
    assert!(references_pkgs("[ pkgs.vim ]"));
    assert!(!references_pkgs("inputs.nixpkgs.legacyPackages"));
    assert!(!references_pkgs("services.foo.enable = true;"));
}

#[test]
fn normalize_rejects_garbage() {
    assert!(matches!(
        normalize_module("{ foo = ;"),
        Err(InstallError::Normalize(_))
    ));
}

// ── aggregator ──────────────────────────────────────────────────────────────

#[test]
fn creates_aggregator_from_scratch() {
    let dir = TempDir::new("agg-new");
    let agg = dir.path.join("default.nix");
    let change = register_in_aggregator(&agg, "2026-06-29-ripgrep-fd").unwrap();
    assert!(change.changed);
    assert!(change.prior.is_none());
    let content = std::fs::read_to_string(&agg).unwrap();
    assert!(content.contains("./2026-06-29-ripgrep-fd.nix"));
    assert!(content.contains("imports = ["));
    // Valid Nix.
    assert!(crate::ast::NixFile::from_source("agg.nix", content).is_ok());
}

#[test]
fn avoids_duplicate_aggregator_imports() {
    let dir = TempDir::new("agg-dup");
    let agg = dir.path.join("default.nix");
    register_in_aggregator(&agg, "plan-a").unwrap();
    let second = register_in_aggregator(&agg, "plan-a").unwrap();
    assert!(!second.changed, "second registration must be a no-op");
    let content = std::fs::read_to_string(&agg).unwrap();
    assert_eq!(content.matches("./plan-a.nix").count(), 1);
}

#[test]
fn preserves_existing_aggregator_imports_and_backs_up() {
    let dir = TempDir::new("agg-preserve");
    let agg = dir.path.join("default.nix");
    std::fs::write(
        &agg,
        "{ ... }:\n{\n  imports = [\n    ./existing.nix\n  ];\n}\n",
    )
    .unwrap();

    let change = register_in_aggregator(&agg, "plan-b").unwrap();
    assert!(change.changed);
    let content = std::fs::read_to_string(&agg).unwrap();
    assert!(content.contains("./existing.nix"), "existing import dropped");
    assert!(content.contains("./plan-b.nix"));
    // A .bak of the prior content exists.
    let bak = change.backup.expect("backup written");
    assert!(bak.exists());
    assert!(std::fs::read_to_string(&bak).unwrap().contains("./existing.nix"));
}

#[test]
fn restore_aggregator_recovers_prior_content() {
    let dir = TempDir::new("agg-restore");
    let agg = dir.path.join("default.nix");
    let original = "{ ... }:\n{\n  imports = [\n    ./existing.nix\n  ];\n}\n";
    std::fs::write(&agg, original).unwrap();

    let change = register_in_aggregator(&agg, "plan-c").unwrap();
    assert!(std::fs::read_to_string(&agg).unwrap().contains("./plan-c.nix"));

    restore_aggregator(&agg, &change).unwrap();
    let restored = std::fs::read_to_string(&agg).unwrap();
    assert!(!restored.contains("./plan-c.nix"));
    assert!(restored.contains("./existing.nix"));
}

// ── root config verification ────────────────────────────────────────────────

#[test]
fn root_config_importing_passes() {
    assert!(root_imports_aggregator(
        "{\n  imports = [ ./hardware-configuration.nix ./modules/ai-generated ];\n}"
    ));
    // Commented-out import does not count.
    assert!(!root_imports_aggregator("{\n  # ./modules/ai-generated\n}"));
    assert!(!root_imports_aggregator("{ imports = [ ./other.nix ]; }"));
}

#[test]
fn register_fails_when_root_does_not_import_aggregator() {
    let dir = TempDir::new("root-missing-import");
    let cfg = NixosApplyConfig::for_config_dir(dir.path.clone(), None);
    std::fs::write(
        &cfg.root_config_path,
        "{ ... }:\n{\n  imports = [ ./hardware-configuration.nix ];\n}\n",
    )
    .unwrap();

    let err = register_module(&cfg, &plan("2026-06-29-ripgrep-fd", BARE_ATTRSET)).unwrap_err();
    assert!(matches!(err, InstallError::RootNotImporting { .. }));
    let msg = err.to_string();
    assert!(msg.contains("not imported"));
    assert!(msg.contains("./modules/ai-generated"));
    // Nothing was written.
    assert!(!cfg.module_path_for("2026-06-29-ripgrep-fd").exists());
    assert!(!cfg.aggregator_path.exists());
}

// ── package + binary parsing ────────────────────────────────────────────────

#[test]
fn parses_system_packages_with_pkgs() {
    let pkgs = parse_system_packages("environment.systemPackages = with pkgs; [ ripgrep fd ];");
    assert_eq!(pkgs, vec!["ripgrep".to_owned(), "fd".to_owned()]);
}

#[test]
fn parses_system_packages_qualified() {
    let pkgs = parse_system_packages("environment.systemPackages = [ pkgs.ripgrep pkgs.fd ];");
    assert_eq!(pkgs, vec!["ripgrep".to_owned(), "fd".to_owned()]);
}

#[test]
fn maps_ripgrep_to_rg() {
    assert_eq!(package_to_binary("ripgrep"), "rg");
    assert_eq!(package_to_binary("fd"), "fd");
    assert_eq!(package_to_binary("htop"), "htop");
}

#[test]
fn verify_binaries_reports_found_and_missing() {
    let dir = TempDir::new("bins");
    let bin_dir = dir.path.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::write(bin_dir.join("rg"), "").unwrap();
    // `fd` deliberately absent.
    let probe = FsBinaryProbe { bin_dir };

    let report = verify_binaries(&probe, &["ripgrep".to_owned(), "fd".to_owned()]);
    assert_eq!(report.found.len(), 1);
    assert!(report.found[0].ends_with("rg"));
    // For the known ripgrep+fd case, a missing binary is a hard failure in tests.
    assert_eq!(report.missing.len(), 1);
    assert_eq!(report.missing[0].binary, "fd");
}

// ── full register → activate flow ───────────────────────────────────────────

#[tokio::test]
async fn register_then_activate_succeeds() {
    let dir = TempDir::new("flow-ok");
    let cfg = config_with_root_import(&dir.path);

    let reg = register_module(&cfg, &plan("2026-06-29-ripgrep-fd", BARE_ATTRSET)).unwrap();
    assert!(reg.module_path.exists());
    let body = std::fs::read_to_string(&reg.module_path).unwrap();
    assert!(body.contains("# plan-id: 2026-06-29-ripgrep-fd"));
    assert!(body.contains("{ config, pkgs, lib, ... }:")); // normalized
    assert_eq!(reg.packages, vec!["ripgrep".to_owned(), "fd".to_owned()]);
    assert!(std::fs::read_to_string(&cfg.aggregator_path)
        .unwrap()
        .contains("./2026-06-29-ripgrep-fd.nix"));

    let builder = ScriptedBuilder { output: ok_build() };
    activate(&cfg, &builder, &reg).await.unwrap();
    // Module still installed after a successful activation.
    assert!(reg.module_path.exists());
}

#[tokio::test]
async fn activate_failure_rolls_back_and_reports_no_success() {
    let dir = TempDir::new("flow-fail");
    let cfg = config_with_root_import(&dir.path);
    // Pre-existing aggregator with an unrelated import to verify restoration.
    std::fs::create_dir_all(&cfg.generated_modules_dir).unwrap();
    std::fs::write(
        &cfg.aggregator_path,
        "{ ... }:\n{\n  imports = [\n    ./existing.nix\n  ];\n}\n",
    )
    .unwrap();

    let reg = register_module(&cfg, &plan("2026-06-29-broken", BARE_ATTRSET)).unwrap();
    assert!(reg.module_path.exists());

    let builder = ScriptedBuilder {
        output: fail_build(),
    };
    let result = activate(&cfg, &builder, &reg).await;

    // No success: an error is returned.
    let err = result.unwrap_err();
    assert!(matches!(err, InstallError::Rebuild { .. }));

    // Aggregator restored to its prior state (no broken import).
    let agg = std::fs::read_to_string(&cfg.aggregator_path).unwrap();
    assert!(agg.contains("./existing.nix"));
    assert!(!agg.contains("./2026-06-29-broken.nix"));

    // The freshly written module was quarantined, not left active.
    assert!(!reg.module_path.exists());
    assert!(cfg
        .generated_modules_dir
        .join("failed")
        .join("2026-06-29-broken.nix")
        .exists());
}

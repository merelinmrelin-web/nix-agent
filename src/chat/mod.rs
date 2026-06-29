//! Interactive system-iteration REPL (`nix-agent chat`).
//!
//! The conversational loop itself lives in the binary (it owns stdin/stdout and
//! the inference backend); this module holds the *pure*, unit-testable core:
//!   * parsing a REPL line into a [`ChatCommand`],
//!   * accumulating instructions into an in-memory [`ChatSession`] and building
//!     the composite generation prompt,
//!   * reading the current system context (root config + aggregator).
//!
//! Keeping these pure means the REPL's behavior is tested without any TTY,
//! subprocess, or model.

use crate::install::NixosApplyConfig;

/// A single line of REPL input, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatCommand {
    /// Deploy the staged module (`apply`).
    Apply,
    /// Re-render the staged diff (`diff`).
    Diff,
    /// Discard the staged plan and start over (`reset`).
    Reset,
    /// Show the command help (`help`, `?`).
    Help,
    /// Leave the session (`quit`, `exit`).
    Quit,
    /// Blank line — ignore.
    Empty,
    /// Anything else is a natural-language instruction to fold into the plan.
    Instruction(String),
}

/// Classify a line of REPL input. Bare words and `:`/`/`-prefixed forms are
/// accepted for the control commands; everything else is an instruction.
pub fn parse_command(input: &str) -> ChatCommand {
    match input.trim() {
        "" => ChatCommand::Empty,
        "apply" | ":apply" | "/apply" => ChatCommand::Apply,
        "diff" | ":diff" | "/diff" => ChatCommand::Diff,
        "reset" | ":reset" | "/reset" => ChatCommand::Reset,
        "help" | ":help" | "/help" | "?" => ChatCommand::Help,
        "quit" | "exit" | ":q" | ":quit" | "/quit" => ChatCommand::Quit,
        other => ChatCommand::Instruction(other.to_owned()),
    }
}

/// In-memory staging plan: the ordered instructions the user has given and the
/// latest module the backend generated from them.
#[derive(Debug, Clone, Default)]
pub struct ChatSession {
    instructions: Vec<String>,
    staged: String,
}

impl ChatSession {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a natural-language instruction.
    pub fn add_instruction(&mut self, text: &str) {
        let t = text.trim();
        if !t.is_empty() {
            self.instructions.push(t.to_owned());
        }
    }

    /// Record the latest generated module body.
    pub fn set_staged(&mut self, module: String) {
        self.staged = module;
    }

    /// The current staged module (empty before the first generation).
    pub fn staged(&self) -> &str {
        &self.staged
    }

    /// Whether anything is staged for apply yet.
    pub fn has_staged(&self) -> bool {
        !self.staged.trim().is_empty()
    }

    pub fn instructions(&self) -> &[String] {
        &self.instructions
    }

    /// Forget all accumulated state.
    pub fn reset(&mut self) {
        self.instructions.clear();
        self.staged.clear();
    }

    /// Build the composite generation prompt from every instruction so far, so
    /// later turns ("change the tmux aliases") refine the same module rather than
    /// starting from scratch.
    pub fn session_prompt(&self) -> String {
        let mut p = String::from(
            "Produce a single NixOS module that satisfies ALL of the following \
             requirements, applied in order:\n",
        );
        for (i, inst) in self.instructions.iter().enumerate() {
            p.push_str(&format!("{}. {}\n", i + 1, inst));
        }
        p
    }
}

/// Snapshot of the system state the REPL reads on startup.
#[derive(Debug, Clone, Default)]
pub struct SystemContext {
    pub root_config: Option<String>,
    pub aggregator: Option<String>,
}

impl SystemContext {
    pub fn root_present(&self) -> bool {
        self.root_config.is_some()
    }
    pub fn aggregator_present(&self) -> bool {
        self.aggregator.is_some()
    }
}

/// Read the active root config and the generated-modules aggregator, if present.
pub fn read_system_context(cfg: &NixosApplyConfig) -> SystemContext {
    SystemContext {
        root_config: std::fs::read_to_string(&cfg.root_config_path).ok(),
        aggregator: std::fs::read_to_string(&cfg.aggregator_path).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parses_control_commands_and_instructions() {
        assert_eq!(parse_command(""), ChatCommand::Empty);
        assert_eq!(parse_command("  apply "), ChatCommand::Apply);
        assert_eq!(parse_command("/diff"), ChatCommand::Diff);
        assert_eq!(parse_command(":reset"), ChatCommand::Reset);
        assert_eq!(parse_command("?"), ChatCommand::Help);
        assert_eq!(parse_command("exit"), ChatCommand::Quit);
        assert_eq!(
            parse_command("add tmux with custom aliases"),
            ChatCommand::Instruction("add tmux with custom aliases".to_owned())
        );
    }

    #[test]
    fn session_accumulates_instructions_into_prompt() {
        let mut s = ChatSession::new();
        assert!(!s.has_staged());
        s.add_instruction("add tmux");
        s.add_instruction("   "); // ignored
        s.add_instruction("change tmux aliases to gs=git status");
        assert_eq!(s.instructions().len(), 2);

        let prompt = s.session_prompt();
        assert!(prompt.contains("1. add tmux"));
        assert!(prompt.contains("2. change tmux aliases to gs=git status"));

        s.set_staged("{ ... }: { }".to_owned());
        assert!(s.has_staged());
        s.reset();
        assert!(!s.has_staged());
        assert!(s.instructions().is_empty());
    }

    #[test]
    fn reads_system_context_from_disk() {
        let mut base = std::env::temp_dir();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("nix-agent-chat-ctx-{nanos}"));
        std::fs::create_dir_all(&base).unwrap();

        let cfg = NixosApplyConfig::for_config_dir(base.clone(), None);
        // Nothing present yet.
        let ctx = read_system_context(&cfg);
        assert!(!ctx.root_present());
        assert!(!ctx.aggregator_present());

        std::fs::write(&cfg.root_config_path, "{ }\n").unwrap();
        let ctx = read_system_context(&cfg);
        assert!(ctx.root_present());
        assert!(!ctx.aggregator_present());

        std::fs::remove_dir_all(&base).ok();
    }
}

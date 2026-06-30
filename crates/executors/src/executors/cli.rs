//! Interactive-CLI launch specs.
//!
//! "CLI mode" runs a coding agent's own interactive TUI inside a persistent
//! tmux pane (the main workspace surface) instead of the headless executor.
//! Each agent describes how to launch its TUI — binary, flags, how to resume a
//! prior session, how to pass an initial prompt — via
//! [`StandardCodingAgentExecutor::interactive_cli_spec`], and the tmux
//! bootstrap (`local-deployment::pty`) turns that into the shell command it
//! runs. Keeping the recipe agent-owned (built from the agent's already
//! overridden config) is what lets every agent reuse the same managed-mode
//! settings (model / reasoning effort / sandbox / approval) in CLI mode.
//!
//! [`StandardCodingAgentExecutor::interactive_cli_spec`]: super::StandardCodingAgentExecutor::interactive_cli_spec

/// How an agent's interactive CLI resumes a prior session given its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliResume {
    /// `<program> <base_args> <flag> <id>` — the id is appended as the value of
    /// a top-level flag (e.g. claude `--resume <id>`). The `base_args`
    /// (model/effort/…) still apply.
    Flag(String),
    /// `<program> <subcommand> <id>` — resume is a subcommand that restores the
    /// session's own settings, so `base_args` are NOT replayed (e.g. codex
    /// `resume <id>`).
    Subcommand(String),
    /// Resuming by id isn't supported; a fresh session is started instead.
    Unsupported,
}

/// How an agent's interactive CLI accepts the initial prompt at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliPromptArg {
    /// Trailing positional argument: `<program> <base_args> '<prompt>'`.
    Positional,
    /// First message passed as the value of a flag: `<program> <base_args>
    /// <flag> '<prompt>'` (e.g. copilot `-i '<prompt>'`).
    Flag(String),
    /// No command-line way to seed the first prompt; it must be delivered after
    /// launch (e.g. via tmux send-keys). The bootstrap starts a bare TUI.
    Unsupported,
}

/// How to launch the agent's TUI when there's nothing explicit to resume and no
/// initial prompt — i.e. reconnecting to a workspace whose tmux session died.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliContinue {
    /// `<base> <flag> || <base>` — continue the most recent conversation in this
    /// cwd, falling back to a fresh TUI (e.g. claude `--continue`).
    Flag(String),
    /// `<program> <subcommand> --last || <program>` — resume the most recent
    /// session via a subcommand, falling back to fresh (e.g. codex `resume
    /// --last`).
    ResumeLast { subcommand: String },
    /// Always start a fresh TUI.
    Fresh,
}

/// A fully-resolved recipe for launching one agent's interactive CLI in a tmux
/// pane. Built by [`StandardCodingAgentExecutor::interactive_cli_spec`] from the
/// agent's already-overridden config; consumed by the tmux bootstrap.
///
/// [`StandardCodingAgentExecutor::interactive_cli_spec`]: super::StandardCodingAgentExecutor::interactive_cli_spec
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliLaunchSpec {
    /// Binary the bootstrap gates on (`command -v <program>`) and exec's. Must
    /// be a bare command name (validated by the bootstrap before use).
    pub program: String,
    /// Flags applied to every launch form except the resume-by-subcommand form
    /// (model / reasoning effort / sandbox / approval / autonomy / cwd …).
    /// Returned as discrete argv entries; the bootstrap shell-quotes each.
    pub base_args: Vec<String>,
    /// How to resume a prior session by id (handover from the chat UI).
    pub resume: CliResume,
    /// How the workspace's initial prompt is delivered (CLI-first creation).
    pub prompt_arg: CliPromptArg,
    /// The no-id / no-prompt fallback (a workspace whose tmux session died).
    pub continue_fallback: CliContinue,
}

impl CliLaunchSpec {
    /// Convenience constructor for the common positional-prompt agent shape.
    pub fn new(program: impl Into<String>, base_args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            base_args,
            resume: CliResume::Unsupported,
            prompt_arg: CliPromptArg::Positional,
            continue_fallback: CliContinue::Fresh,
        }
    }

    pub fn with_resume(mut self, resume: CliResume) -> Self {
        self.resume = resume;
        self
    }

    pub fn with_prompt_arg(mut self, prompt_arg: CliPromptArg) -> Self {
        self.prompt_arg = prompt_arg;
        self
    }

    pub fn with_continue(mut self, continue_fallback: CliContinue) -> Self {
        self.continue_fallback = continue_fallback;
        self
    }
}

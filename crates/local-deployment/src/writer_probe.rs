use std::{collections::HashSet, path::Path};

use async_trait::async_trait;
use db::{DBService, models::cli_pane_binding::CliPaneBinding};
use services::services::cli_collab::{CliWriterProbe, ProbeReport, SidEvidence};
use uuid::Uuid;

use crate::pty::{
    CLI_AGENT_PROGRAMS, CliPaneAgentProcess, cli_pane_agent_processes,
    cli_tmux_session_exists_checked,
};

#[derive(Clone)]
pub struct LocalCliWriterProbe {
    db: DBService,
}

impl LocalCliWriterProbe {
    pub fn new(db: DBService) -> Self {
        Self { db }
    }
}

fn resume_evidence(cmdlines: &[String]) -> SidEvidence {
    let mut ids = HashSet::new();
    let mut saw_resume = false;
    let mut invalid = false;
    for cmdline in cmdlines {
        let words: Vec<_> = cmdline.split_whitespace().collect();
        for (index, word) in words.iter().enumerate() {
            let candidate = if *word == "--resume" {
                saw_resume = true;
                words.get(index + 1).copied()
            } else if let Some(value) = word.strip_prefix("--resume=") {
                saw_resume = true;
                Some(value)
            } else {
                None
            };
            if let Some(candidate) = candidate {
                match Uuid::parse_str(candidate) {
                    Ok(id) => {
                        ids.insert(id.to_string());
                    }
                    Err(_) => invalid = true,
                }
            } else if *word == "--resume" {
                invalid = true;
            }
        }
    }
    if invalid || ids.len() > 1 {
        SidEvidence::Ambiguous
    } else if let Some(id) = ids.into_iter().next() {
        SidEvidence::ConfirmedResume(id)
    } else if saw_resume {
        SidEvidence::Ambiguous
    } else {
        SidEvidence::NoResumeArg
    }
}

/// Codex options that take a separate value (`codex --help`, `codex resume
/// --help`, codex-cli 0.158). Needed to find the first positional argument,
/// which names the subcommand.
const CODEX_VALUE_OPTIONS: &[&str] = &[
    "-c",
    "--config",
    "-m",
    "--model",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-p",
    "--profile",
    "-i",
    "--image",
    "-C",
    "--cd",
    "--enable",
    "--disable",
    "--add-dir",
    "--local-provider",
    "--remote",
    "--remote-auth-token-env",
];

/// The positional arguments of a Codex command line, and whether `--last`
/// appears. `argv[0]` is the binary.
fn codex_positionals(argv: &[String]) -> (Vec<&str>, bool) {
    let mut positionals = Vec::new();
    let mut last = false;
    let mut args = argv.iter().skip(1).map(String::as_str);
    while let Some(arg) = args.next() {
        if arg == "--" {
            positionals.extend(args.by_ref());
        } else if arg == "--last" {
            last = true;
        } else if CODEX_VALUE_OPTIONS.contains(&arg) {
            args.next();
        } else if !arg.starts_with('-') || arg == "-" {
            positionals.push(arg);
        }
    }
    (positionals, last)
}

/// Resume evidence from Codex argument vectors. `codex resume <uuid>` names
/// the thread. `codex resume --last`, a session name, or the resume picker
/// name no verifiable thread, so they are ambiguous. Any other launch
/// (`codex`, `codex <prompt>`, `codex fork …`) starts a new thread.
fn codex_resume_evidence(argvs: &[Vec<String>]) -> SidEvidence {
    let mut ids = HashSet::new();
    let mut ambiguous = false;
    for argv in argvs {
        let (positionals, last) = codex_positionals(argv);
        if positionals.first() != Some(&"resume") {
            continue;
        }
        match (positionals.get(1), last) {
            (Some(candidate), false) => match Uuid::parse_str(candidate) {
                Ok(id) => {
                    ids.insert(id.to_string());
                }
                Err(_) => ambiguous = true,
            },
            _ => ambiguous = true,
        }
    }
    if ambiguous || ids.len() > 1 {
        SidEvidence::Ambiguous
    } else if let Some(id) = ids.into_iter().next() {
        SidEvidence::ConfirmedResume(id)
    } else {
        SidEvidence::NoResumeArg
    }
}

// `_expected_sid` and `_binding` are load-bearing for `probe_path_never_replaces_live_evidence_with_matching_database_sid`.
fn live_process_report(
    program: Option<&str>,
    processes: &[CliPaneAgentProcess],
    only_active_claude_in_cwd: Option<bool>,
    _expected_sid: Option<&str>,
    _binding: Option<&CliPaneBinding>,
) -> ProbeReport {
    let program = program.filter(|_| !processes.is_empty());
    let sid_evidence = match program {
        None => SidEvidence::Unknown,
        Some("codex") => codex_resume_evidence(
            &processes
                .iter()
                .map(|process| process.argv.clone())
                .collect::<Vec<_>>(),
        ),
        Some(_) => resume_evidence(
            &processes
                .iter()
                .map(|process| process.cmdline.clone())
                .collect::<Vec<_>>(),
        ),
    };
    ProbeReport {
        pane_session_exists: true,
        agent_running: Some(program.is_some()),
        agent_program: program.map(str::to_string),
        sid_evidence,
        probe_failed: false,
        only_active_claude_in_cwd,
    }
}

/// Whether exactly one live `program` process has `effective_dir` as its cwd,
/// and it is one of the pane's processes. A node launcher counts only when no
/// native `program` binary runs in that cwd.
#[cfg(target_os = "linux")]
fn only_active_agent_in_cwd(
    effective_dir: &Path,
    pane_pids: &HashSet<u32>,
    program: &str,
) -> Option<bool> {
    let effective_dir = effective_dir
        .canonicalize()
        .unwrap_or_else(|_| effective_dir.to_path_buf());
    let entries = std::fs::read_dir("/proc").ok()?;
    let mut native = Vec::new();
    let mut wrappers = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(_) => continue,
        };
        let Some(open) = stat.find('(') else {
            continue;
        };
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let comm = &stat[open + 1..close];
        let is_wrapper = comm == "node"
            && std::fs::read(entry.path().join("cmdline"))
                .map(|bytes| {
                    bytes
                        .windows(program.len())
                        .any(|part| part == program.as_bytes())
                })
                .unwrap_or(false);
        if comm != program && !is_wrapper {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
            continue;
        };
        if cwd != effective_dir {
            continue;
        }
        if comm == program {
            native.push(pid);
        } else {
            wrappers.push(pid);
        }
    }
    let candidates = if native.is_empty() { wrappers } else { native };
    Some(candidates.len() == 1 && pane_pids.contains(&candidates[0]))
}

#[cfg(not(target_os = "linux"))]
fn only_active_agent_in_cwd(
    _effective_dir: &Path,
    _pane_pids: &HashSet<u32>,
    _program: &str,
) -> Option<bool> {
    None
}

#[async_trait]
impl CliWriterProbe for LocalCliWriterProbe {
    async fn probe(
        &self,
        workspace_id: Uuid,
        effective_dir: &Path,
        expected_sid: Option<&str>,
        binding: Option<&CliPaneBinding>,
        check_cwd_uniqueness: bool,
    ) -> ProbeReport {
        let exists = match cli_tmux_session_exists_checked(workspace_id).await {
            Ok(exists) => exists,
            Err(error) => {
                tracing::warn!(?error, %workspace_id, "CLI tmux writer probe failed");
                return ProbeReport::failed();
            }
        };
        if !exists {
            if let Some(binding) = binding
                && let Err(error) = CliPaneBinding::release(&self.db.pool, binding.id).await
            {
                tracing::warn!(?error, %workspace_id, "dead CLI pane binding release failed");
                return ProbeReport::failed();
            }
            return ProbeReport {
                pane_session_exists: false,
                agent_running: Some(false),
                agent_program: None,
                sid_evidence: SidEvidence::Unknown,
                probe_failed: false,
                only_active_claude_in_cwd: check_cwd_uniqueness.then_some(false),
            };
        }

        let (program, processes) =
            match cli_pane_agent_processes(workspace_id, CLI_AGENT_PROGRAMS).await {
                Some(found) => found,
                None => return ProbeReport::failed(),
            };
        let cwd_uniqueness = if !check_cwd_uniqueness {
            None
        } else if let Some(program) = program.filter(|_| !processes.is_empty()) {
            let effective_dir = effective_dir.to_path_buf();
            let pane_pids = processes.iter().map(|process| process.pid).collect();
            match tokio::task::spawn_blocking(move || {
                only_active_agent_in_cwd(&effective_dir, &pane_pids, program)
            })
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(?error, %workspace_id, "CLI cwd uniqueness probe failed");
                    None
                }
            }
        } else {
            Some(false)
        };
        live_process_report(program, &processes, cwd_uniqueness, expected_sid, binding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_cmdline_evidence_is_exact_and_ambiguous_on_disagreement() {
        assert_eq!(
            resume_evidence(&["claude --resume 11111111-1111-4111-8111-111111111111".into()]),
            SidEvidence::ConfirmedResume("11111111-1111-4111-8111-111111111111".into())
        );
        assert_eq!(
            resume_evidence(&[
                "claude --resume 11111111-1111-4111-8111-111111111111".into(),
                "claude --resume 22222222-2222-4222-8222-222222222222".into(),
            ]),
            SidEvidence::Ambiguous
        );
        assert_eq!(
            resume_evidence(&["claude --model opus".into()]),
            SidEvidence::NoResumeArg
        );
    }

    #[test]
    fn live_probe_report_never_fabricates_database_resume_evidence() {
        let expected = "11111111-1111-4111-8111-111111111111";
        let observed = "22222222-2222-4222-8222-222222222222";

        let mismatched = live_process_report(
            Some("claude"),
            &[CliPaneAgentProcess {
                pid: 42,
                cmdline: format!("claude --resume {observed}"),
                argv: Vec::new(),
            }],
            None,
            None,
            None,
        );
        assert_eq!(
            mismatched.sid_evidence,
            SidEvidence::ConfirmedResume(observed.to_string())
        );
        assert_ne!(
            mismatched.sid_evidence,
            SidEvidence::ConfirmedResume(expected.to_string())
        );

        let no_resume = live_process_report(
            Some("claude"),
            &[CliPaneAgentProcess {
                pid: 43,
                cmdline: "claude --model opus".to_string(),
                argv: Vec::new(),
            }],
            None,
            None,
            None,
        );
        assert_eq!(no_resume.sid_evidence, SidEvidence::NoResumeArg);
        assert_ne!(
            no_resume.sid_evidence,
            SidEvidence::ConfirmedResume(expected.to_string())
        );
    }

    #[test]
    fn probe_path_never_replaces_live_evidence_with_matching_database_sid() {
        let expected = "11111111-1111-4111-8111-111111111111";
        let observed = "22222222-2222-4222-8222-222222222222";
        let binding = CliPaneBinding {
            id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            claude_session_id: Some(expected.to_string()),
            bound_via: db::models::cli_pane_binding::CliPaneBoundVia::CliResume,
            created_at: chrono::Utc::now(),
            released_at: None,
        };

        let mismatched = live_process_report(
            Some("claude"),
            &[CliPaneAgentProcess {
                pid: 44,
                cmdline: format!("claude --resume {observed}"),
                argv: Vec::new(),
            }],
            None,
            Some(expected),
            Some(&binding),
        );
        assert_eq!(
            mismatched.sid_evidence,
            SidEvidence::ConfirmedResume(observed.to_string())
        );

        let no_resume = live_process_report(
            Some("claude"),
            &[CliPaneAgentProcess {
                pid: 45,
                cmdline: "claude --model opus".to_string(),
                argv: Vec::new(),
            }],
            None,
            Some(expected),
            Some(&binding),
        );
        assert_eq!(no_resume.sid_evidence, SidEvidence::NoResumeArg);
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    const THREAD: &str = "019e6f10-4f27-7d02-9a4b-4f3c2d1e0a55";

    #[test]
    fn codex_resume_subcommand_names_the_thread_past_global_options() {
        let hook = r#"hooks.Stop=[{hooks=[{type="command",command="curl resume x",timeout=3}]}]"#;
        let launched = argv(&[
            "/opt/codex/bin/codex",
            "--dangerously-bypass-hook-trust",
            "-c",
            hook,
            "resume",
            &THREAD.to_uppercase(),
        ]);
        assert_eq!(
            codex_resume_evidence(&[launched]),
            SidEvidence::ConfirmedResume(THREAD.to_string())
        );
        let options_after = argv(&[
            "codex",
            "resume",
            THREAD,
            "-c",
            "model=o3",
            "--no-alt-screen",
        ]);
        assert_eq!(
            codex_resume_evidence(&[options_after]),
            SidEvidence::ConfirmedResume(THREAD.to_string())
        );
    }

    #[test]
    fn codex_resume_without_a_thread_id_is_ambiguous() {
        for args in [
            &["codex", "-s", "danger-full-access", "resume", "--last"][..],
            &["codex", "resume"][..],
            &["codex", "resume", "my-session-name"][..],
            &["codex", "resume", "--last", THREAD][..],
        ] {
            assert_eq!(
                codex_resume_evidence(&[argv(args)]),
                SidEvidence::Ambiguous,
                "{args:?}"
            );
        }
    }

    #[test]
    fn codex_launches_without_the_resume_subcommand_start_a_new_thread() {
        for args in [
            &[
                "codex",
                "-m",
                "gpt-5.5",
                "-s",
                "danger-full-access",
                "--no-alt-screen",
            ][..],
            // A prompt that mentions resuming is one argument, never a subcommand.
            &["codex", "-a", "never", &format!("resume {THREAD}")][..],
            &["codex", "fork", THREAD][..],
            // The value of a value option is never a positional.
            &["codex", "-m", "resume", "hello"][..],
        ] {
            assert_eq!(
                codex_resume_evidence(&[argv(args)]),
                SidEvidence::NoResumeArg,
                "{args:?}"
            );
        }
    }

    #[test]
    fn codex_processes_that_disagree_are_ambiguous() {
        let other = "019e6f10-4f27-7d02-9a4b-4f3c2d1e0a66";
        assert_eq!(
            codex_resume_evidence(&[
                argv(&["codex", "resume", THREAD]),
                argv(&["codex", "resume", other]),
            ]),
            SidEvidence::Ambiguous
        );
        assert_eq!(
            codex_resume_evidence(&[
                argv(&["codex", "resume", THREAD]),
                argv(&["codex", "app-server"]),
            ]),
            SidEvidence::ConfirmedResume(THREAD.to_string())
        );
    }

    #[test]
    fn live_report_reads_codex_evidence_from_argv_and_names_the_program() {
        let report = live_process_report(
            Some("codex"),
            &[CliPaneAgentProcess {
                pid: 46,
                // `ps` joins argv with spaces; the argv is authoritative.
                cmdline: format!("codex -a never resume {THREAD} tail"),
                argv: argv(&["codex", "-a", "never", &format!("resume {THREAD} tail")]),
            }],
            None,
            None,
            None,
        );
        assert_eq!(report.agent_running, Some(true));
        assert_eq!(report.agent_program.as_deref(), Some("codex"));
        assert_eq!(report.sid_evidence, SidEvidence::NoResumeArg);

        let idle = live_process_report(Some("codex"), &[], None, None, None);
        assert_eq!(idle.agent_running, Some(false));
        assert_eq!(idle.agent_program, None);
        assert_eq!(idle.sid_evidence, SidEvidence::Unknown);
    }
}

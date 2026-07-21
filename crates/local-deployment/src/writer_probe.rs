use std::{collections::HashSet, path::Path};

use async_trait::async_trait;
use db::{DBService, models::cli_pane_binding::CliPaneBinding};
use services::services::cli_collab::{CliWriterProbe, ProbeReport, SidEvidence};
use uuid::Uuid;

use crate::pty::{cli_pane_agent_processes, cli_tmux_session_exists_checked};

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

#[cfg(target_os = "linux")]
fn only_active_claude_in_cwd(effective_dir: &Path, pane_pids: &HashSet<u32>) -> Option<bool> {
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
                .map(|bytes| bytes.windows(b"claude".len()).any(|part| part == b"claude"))
                .unwrap_or(false);
        if comm != "claude" && !is_wrapper {
            continue;
        }
        let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) else {
            continue;
        };
        if cwd != effective_dir {
            continue;
        }
        if comm == "claude" {
            native.push(pid);
        } else {
            wrappers.push(pid);
        }
    }
    let candidates = if native.is_empty() { wrappers } else { native };
    Some(candidates.len() == 1 && pane_pids.contains(&candidates[0]))
}

#[cfg(not(target_os = "linux"))]
fn only_active_claude_in_cwd(_effective_dir: &Path, _pane_pids: &HashSet<u32>) -> Option<bool> {
    None
}

#[async_trait]
impl CliWriterProbe for LocalCliWriterProbe {
    async fn probe(
        &self,
        workspace_id: Uuid,
        effective_dir: &Path,
        expected_sid: Option<&str>,
    ) -> ProbeReport {
        let binding =
            match CliPaneBinding::find_active_for_workspace(&self.db.pool, workspace_id).await {
                Ok(binding) => binding,
                Err(error) => {
                    tracing::warn!(?error, %workspace_id, "CLI pane binding probe failed");
                    return ProbeReport::failed();
                }
            };
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
                sid_evidence: SidEvidence::Unknown,
                probe_failed: false,
                only_active_claude_in_cwd: Some(false),
            };
        }

        let processes = match cli_pane_agent_processes(workspace_id, "claude").await {
            Some(processes) => processes,
            None => return ProbeReport::failed(),
        };
        let agent_running = !processes.is_empty();
        let cmdlines: Vec<_> = processes
            .iter()
            .map(|process| process.cmdline.clone())
            .collect();
        let mut sid_evidence = if agent_running {
            resume_evidence(&cmdlines)
        } else {
            SidEvidence::Unknown
        };
        if agent_running
            && let (Some(expected), Some(binding)) = (expected_sid, binding.as_ref())
            && binding.claude_session_id.as_deref() == Some(expected)
        {
            sid_evidence = SidEvidence::ConfirmedResume(expected.to_string());
        }
        let pane_pids = processes.iter().map(|process| process.pid).collect();
        ProbeReport {
            pane_session_exists: true,
            agent_running: Some(agent_running),
            sid_evidence,
            probe_failed: false,
            only_active_claude_in_cwd: if agent_running {
                only_active_claude_in_cwd(effective_dir, &pane_pids)
            } else {
                Some(false)
            },
        }
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
}

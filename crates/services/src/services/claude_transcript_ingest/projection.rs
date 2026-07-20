use std::collections::HashMap;

use db::models::cli_native_record::SessionNativeRecord;
use executors::{
    executors::claude::native::{NativeClaudeNormalizer, adapt_native_claude_line},
    logs::NormalizedEntry,
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use super::forks::{NativeDagRecord, NativeForkView, compute_fork_view};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[serde(rename_all = "lowercase")]
pub enum NativeFeedOrigin {
    Cli,
    App,
    Executor,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeBranchMetadata {
    pub fork_parent_uuid: String,
    pub branch_index: usize,
    pub branch_label: String,
    pub is_default: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct NativeFeedEntry {
    pub normalized_entry: NormalizedEntry,
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub ts: Option<String>,
    pub origin: NativeFeedOrigin,
    pub linked_execution_process_id: Option<Uuid>,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    pub branch: Option<NativeBranchMetadata>,
    pub seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeFeedFork {
    pub claude_session_id: String,
    pub file_id: Uuid,
    pub fork: NativeForkView,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeFileImportHealth {
    pub claude_session_id: String,
    pub file_name: String,
    pub generation: i64,
    pub last_import_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
pub struct NativeIngestHealth {
    pub unknown_kinds: u64,
    pub rescans: u64,
    pub quarantined_files: u64,
    pub watch_degraded: bool,
    pub files: Vec<NativeFileImportHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct NativeFeedSnapshot {
    pub revision: u64,
    pub seq: i64,
    pub entries: Vec<NativeFeedEntry>,
    pub forks: Vec<NativeFeedFork>,
    pub health: NativeIngestHealth,
    /// Coarse hint sourced from the existing workspace CLI activity poller.
    pub cli_session_active: bool,
}

#[derive(Default)]
struct FileDag {
    file_id: Option<Uuid>,
    claude_session_id: String,
    records: Vec<NativeDagRecord>,
    leaf_hint: Option<String>,
}

pub fn build_projection(
    rows: &[SessionNativeRecord],
    revision: u64,
    seq: i64,
    health: NativeIngestHealth,
    cli_session_active: bool,
) -> NativeFeedSnapshot {
    let mut normalizers = HashMap::<String, NativeClaudeNormalizer>::new();
    let mut entry_positions = HashMap::<(String, usize), usize>::new();
    let mut entries = Vec::<NativeFeedEntry>::new();
    let mut file_dags = Vec::<FileDag>::new();
    let mut file_dag_positions = HashMap::<Uuid, usize>::new();

    for row in rows {
        let Ok(line) = adapt_native_claude_line(&row.raw, &row.claude_session_id) else {
            continue;
        };
        let metadata = line.metadata();
        let dag_index = *file_dag_positions.entry(row.file_id).or_insert_with(|| {
            let index = file_dags.len();
            file_dags.push(FileDag {
                file_id: Some(row.file_id),
                claude_session_id: row.claude_session_id.clone(),
                ..FileDag::default()
            });
            index
        });
        let dag = &mut file_dags[dag_index];
        if let Some(uuid) = &metadata.uuid {
            dag.records.push(NativeDagRecord {
                uuid: uuid.clone(),
                parent_uuid: metadata.parent_uuid.clone(),
            });
        }
        if let Some(leaf_hint) = &metadata.leaf_uuid {
            dag.leaf_hint = Some(leaf_hint.clone());
        }

        let normalizer = normalizers
            .entry(row.claude_session_id.clone())
            .or_default();
        for change in normalizer.normalize(&line, &row.dir_path) {
            let key = (row.claude_session_id.clone(), change.index);
            if let Some(position) = entry_positions.get(&key).copied() {
                entries[position].normalized_entry = change.entry;
                continue;
            }

            let linked_execution_process_id = row
                .linked_execution_process_id
                .or(row.bound_turn_execution_process_id);
            let origin = if row.linked_execution_process_id.is_some() {
                NativeFeedOrigin::Executor
            } else if row.bound_coding_agent_turn_id.is_some() {
                NativeFeedOrigin::App
            } else {
                NativeFeedOrigin::Cli
            };
            let position = entries.len();
            entries.push(NativeFeedEntry {
                normalized_entry: change.entry,
                claude_session_id: row.claude_session_id.clone(),
                uuid: metadata.uuid.clone(),
                parent_uuid: metadata.parent_uuid.clone(),
                ts: metadata.timestamp.clone(),
                origin,
                linked_execution_process_id,
                git_branch: metadata.git_branch.clone(),
                version: metadata.version.clone(),
                branch: None,
                seq: row.seq,
            });
            entry_positions.insert(key, position);
        }
    }

    let mut forks = Vec::new();
    let mut branch_by_uuid = HashMap::<String, NativeBranchMetadata>::new();
    for dag in file_dags {
        let Some(fork) = compute_fork_view(&dag.records, dag.leaf_hint.as_deref()) else {
            continue;
        };
        for (branch_index, branch) in fork.branches.iter().enumerate() {
            for uuid in &branch.node_uuids {
                branch_by_uuid.insert(
                    uuid.clone(),
                    NativeBranchMetadata {
                        fork_parent_uuid: fork.fork_parent_uuid.clone(),
                        branch_index,
                        branch_label: branch.label.clone(),
                        is_default: fork.default_branch == Some(branch_index),
                    },
                );
            }
        }
        forks.push(NativeFeedFork {
            claude_session_id: dag.claude_session_id,
            file_id: dag.file_id.expect("file dag always has an id"),
            fork,
        });
    }
    for entry in &mut entries {
        if let Some(uuid) = &entry.uuid {
            entry.branch = branch_by_uuid.get(uuid).cloned();
        }
    }

    NativeFeedSnapshot {
        revision,
        seq,
        entries,
        forks,
        health,
        cli_session_active,
    }
}

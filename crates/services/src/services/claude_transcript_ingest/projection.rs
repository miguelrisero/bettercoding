use std::{
    collections::{BTreeSet, HashMap},
    sync::atomic::{AtomicU64, Ordering},
};

use db::models::cli_native_record::{CliNativeRecordDisposition, SessionNativeRecord};
use executors::{
    executors::{
        claude::native::{NativeClaudeNormalizer, adapt_native_claude_line},
        codex::rollout::{adapt_codex_rollout_line, rollout_thread_id},
    },
    logs::{NormalizedEntry, NormalizedEntryType},
};
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

use super::forks::{ForkFreeTracker, NativeDagRecord, NativeForkView, compute_fork_view};

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
    pub foreign_writer_seen_at: Option<String>,
    pub files: Vec<NativeFileImportHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct NativeFeedSnapshot {
    pub revision: u64,
    pub seq: i64,
    pub entries: Vec<NativeFeedEntry>,
    pub forks: Vec<NativeFeedFork>,
    pub health: NativeIngestHealth,
}

struct FileDag {
    file_id: Uuid,
    claude_session_id: String,
    records: Vec<NativeDagRecord>,
    leaf_hint: Option<String>,
    /// `None` once the tracker can no longer prove this file fork-free.
    tracker: Option<ForkFreeTracker>,
    fork: Option<NativeForkView>,
}

/// Distinguishes one full build from every other, across all sessions, so a
/// change cursor taken from a replaced projection is never applied to it.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// What a feed consumer has already received from one [`NativeProjection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeFeedCursor {
    epoch: u64,
    version: u64,
    len: usize,
}

/// The entry-level difference between a cursor and the current projection.
#[derive(Debug)]
pub struct NativeFeedDelta<'a> {
    /// Entries at or above the cursor's length, starting at `appended_from`.
    pub appended_from: usize,
    pub appended: &'a [NativeFeedEntry],
    /// Entries below the cursor's length that changed in place.
    pub replaced: Vec<(usize, &'a NativeFeedEntry)>,
    pub forks_changed: bool,
}

/// A session's native feed projection, kept so that appended records can be
/// folded in without re-reading or re-adapting the rows already projected.
///
/// [`NativeProjection::build`] over rows `a ++ b` is equal to `build(a)`
/// followed by [`NativeProjection::extend`] with `b`, provided `b` holds only
/// rows ordered after `a` and [`NativeProjection::can_extend`] accepts it. The
/// fold is sequential by construction (one normalizer per Claude session, in
/// row order); fork views are recomputed only for files the append touched
/// and only when [`ForkFreeTracker`] cannot prove they stay fork-free.
pub struct NativeProjection {
    revision: u64,
    last_row_seq: i64,
    rows_visited: u64,
    normalizers: HashMap<String, NativeClaudeNormalizer>,
    entry_positions: HashMap<(String, usize), usize>,
    entries: Vec<NativeFeedEntry>,
    file_dags: Vec<FileDag>,
    file_dag_positions: HashMap<Uuid, usize>,
    file_ids_by_path: HashMap<(String, String), Uuid>,
    files: Vec<NativeFileImportHealth>,
    file_positions: HashMap<Uuid, usize>,
    branch_by_uuid: HashMap<String, NativeBranchMetadata>,
    forks: Vec<NativeFeedFork>,
    epoch: u64,
    version: u64,
    forks_version: u64,
    /// `(version, index)` for every in-place change to an entry, in version
    /// order, so a cursor's replacements are found without scanning entries.
    changes: Vec<(u64, usize)>,
}

impl NativeProjection {
    pub fn build(rows: &[SessionNativeRecord], revision: u64) -> Self {
        let mut projection = Self {
            revision,
            last_row_seq: i64::MIN,
            rows_visited: 0,
            normalizers: HashMap::new(),
            entry_positions: HashMap::new(),
            entries: Vec::new(),
            file_dags: Vec::new(),
            file_dag_positions: HashMap::new(),
            file_ids_by_path: HashMap::new(),
            files: Vec::new(),
            file_positions: HashMap::new(),
            branch_by_uuid: HashMap::new(),
            forks: Vec::new(),
            epoch: NEXT_EPOCH.fetch_add(1, Ordering::Relaxed),
            version: 0,
            forks_version: 0,
            changes: Vec::new(),
        };
        let mut touched = BTreeSet::new();
        for row in rows {
            projection.fold_row(row, 0, &mut touched);
        }
        for dag in &mut projection.file_dags {
            dag.fork = compute_fork_view(&dag.records, dag.leaf_hint.as_deref());
        }
        projection.rebuild_branches(0);
        projection
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Highest outbox `seq` folded in; fetch only rows above it to extend.
    pub fn last_row_seq(&self) -> i64 {
        self.last_row_seq
    }

    #[cfg(test)]
    /// Rows folded in since the full build, including it.
    pub fn rows_visited(&self) -> u64 {
        self.rows_visited
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[NativeFeedEntry] {
        &self.entries
    }

    pub fn forks(&self) -> &[NativeFeedFork] {
        &self.forks
    }

    pub fn files(&self) -> &[NativeFileImportHealth] {
        &self.files
    }

    /// Whether `rows` can be folded in without a full rebuild. A row from a
    /// newer generation of an already projected file means the older
    /// generation's rows have left the canonical row set, which an append
    /// cannot express.
    pub fn can_extend(&self, rows: &[SessionNativeRecord]) -> bool {
        rows.iter().all(|row| {
            row.seq > self.last_row_seq
                && self
                    .file_ids_by_path
                    .get(&(row.dir_path.clone(), row.file_name.clone()))
                    .is_none_or(|file_id| *file_id == row.file_id)
        })
    }

    pub fn extend(&mut self, rows: &[SessionNativeRecord]) {
        if rows.is_empty() {
            return;
        }
        self.version += 1;
        let base_len = self.entries.len();
        let mut touched = BTreeSet::new();
        for row in rows {
            self.fold_row(row, base_len, &mut touched);
        }

        let mut forks_changed = false;
        for index in touched {
            let dag = &mut self.file_dags[index];
            if dag.tracker.is_some() {
                continue;
            }
            // ponytail: a file that already has a fork recomputes its fork
            // view on every append that touches its DAG, O(file) hashing
            // (~0.2 s at 115k records). Maintain the fork view incrementally
            // if forked sessions of that size become common.
            let fork = compute_fork_view(&dag.records, dag.leaf_hint.as_deref());
            if fork != dag.fork {
                dag.fork = fork;
                forks_changed = true;
            }
        }
        if forks_changed {
            self.forks_version = self.version;
            self.rebuild_branches(base_len);
        } else {
            for entry in &mut self.entries[base_len..] {
                entry.branch = entry
                    .uuid
                    .as_ref()
                    .and_then(|uuid| self.branch_by_uuid.get(uuid).cloned());
            }
        }
    }

    pub fn cursor(&self) -> NativeFeedCursor {
        NativeFeedCursor {
            epoch: self.epoch,
            version: self.version,
            len: self.entries.len(),
        }
    }

    /// The difference since `cursor`, or `None` when the cursor belongs to a
    /// different build and the consumer needs the whole projection.
    pub fn delta_since(&self, cursor: NativeFeedCursor) -> Option<NativeFeedDelta<'_>> {
        if cursor.epoch != self.epoch || cursor.len > self.entries.len() {
            return None;
        }
        let first_change = self
            .changes
            .partition_point(|(version, _)| *version <= cursor.version);
        let replaced = self.changes[first_change..]
            .iter()
            .map(|(_, index)| *index)
            .filter(|index| *index < cursor.len)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|index| (index, &self.entries[index]))
            .collect();
        Some(NativeFeedDelta {
            appended_from: cursor.len,
            appended: &self.entries[cursor.len..],
            replaced,
            forks_changed: self.forks_version > cursor.version,
        })
    }

    pub fn snapshot(&self, seq: i64, mut health: NativeIngestHealth) -> NativeFeedSnapshot {
        health.files = self.files.clone();
        NativeFeedSnapshot {
            revision: self.revision,
            seq,
            entries: self.entries.clone(),
            forks: self.forks.clone(),
            health,
        }
    }

    fn fold_row(
        &mut self,
        row: &SessionNativeRecord,
        base_len: usize,
        touched: &mut BTreeSet<usize>,
    ) {
        self.rows_visited += 1;
        self.last_row_seq = self.last_row_seq.max(row.seq);
        self.file_ids_by_path
            .insert((row.dir_path.clone(), row.file_name.clone()), row.file_id);
        let health = NativeFileImportHealth {
            claude_session_id: row.claude_session_id.clone(),
            file_name: row.file_name.clone(),
            generation: row.generation,
            last_import_at: row.last_import_at.map(|time| time.to_rfc3339()),
        };
        match self.file_positions.get(&row.file_id) {
            Some(position) => self.files[*position] = health,
            None => {
                self.file_positions.insert(row.file_id, self.files.len());
                self.files.push(health);
            }
        }

        if rollout_thread_id(&row.file_name).is_some() {
            self.fold_codex_row(row);
            return;
        }
        if row.disposition == CliNativeRecordDisposition::Sidechain.as_str() {
            return;
        }
        let Ok(line) = adapt_native_claude_line(&row.raw, &row.claude_session_id) else {
            return;
        };
        let metadata = line.metadata();
        let dag_index = *self
            .file_dag_positions
            .entry(row.file_id)
            .or_insert_with(|| {
                self.file_dags.push(FileDag {
                    file_id: row.file_id,
                    claude_session_id: row.claude_session_id.clone(),
                    records: Vec::new(),
                    leaf_hint: None,
                    tracker: Some(ForkFreeTracker::default()),
                    fork: None,
                });
                self.file_dags.len() - 1
            });
        let dag = &mut self.file_dags[dag_index];
        if let Some(uuid) = &metadata.uuid {
            let record = NativeDagRecord {
                uuid: uuid.clone(),
                parent_uuid: metadata.parent_uuid.clone(),
                kind: metadata.kind.clone(),
            };
            if dag
                .tracker
                .as_mut()
                .is_some_and(|tracker| !tracker.push(&record))
            {
                dag.tracker = None;
            }
            dag.records.push(record);
            touched.insert(dag_index);
        }
        if let Some(leaf_hint) = &metadata.leaf_uuid {
            dag.leaf_hint = Some(leaf_hint.clone());
            touched.insert(dag_index);
        }

        let normalizer = self
            .normalizers
            .entry(row.claude_session_id.clone())
            .or_default();
        for mut change in normalizer.normalize(&line, &row.dir_path) {
            if matches!(change.entry.entry_type, NormalizedEntryType::UserMessage)
                && let Some(content) = unwrap_pasted_content(&change.entry.content)
            {
                change.entry.content = content;
            }
            let key = (row.claude_session_id.clone(), change.index);
            if let Some(position) = self.entry_positions.get(&key).copied() {
                self.entries[position].normalized_entry = change.entry;
                if position < base_len {
                    self.changes.push((self.version, position));
                }
                continue;
            }

            let (origin, linked_execution_process_id) = row_origin(row);
            self.entry_positions.insert(key, self.entries.len());
            self.entries.push(NativeFeedEntry {
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
        }
    }

    /// A Codex rollout row: every completed item appends its entries once, and
    /// a rollout has no fork DAG, so it never touches forks or branches.
    fn fold_codex_row(&mut self, row: &SessionNativeRecord) {
        if row.disposition != CliNativeRecordDisposition::Renderable.as_str() {
            return;
        }
        // Only rendering rows are stored, and a paginated rollout's legacy
        // events never render, so every stored event is rendered here.
        let Ok(line) = adapt_codex_rollout_line(&row.raw, &row.link_cwd, true) else {
            return;
        };
        let (origin, linked_execution_process_id) = row_origin(row);
        for normalized_entry in line.entries {
            self.entries.push(NativeFeedEntry {
                normalized_entry,
                claude_session_id: row.claude_session_id.clone(),
                uuid: line.item_id.clone(),
                parent_uuid: line.turn_id.clone(),
                ts: line.timestamp.clone(),
                origin,
                linked_execution_process_id,
                git_branch: None,
                version: None,
                branch: None,
                seq: row.seq,
            });
        }
    }

    /// Recompute the fork list and every entry's branch from the per-file fork
    /// views, logging entries below `base_len` whose branch changed.
    fn rebuild_branches(&mut self, base_len: usize) {
        self.forks.clear();
        self.branch_by_uuid.clear();
        for dag in &self.file_dags {
            let Some(fork) = &dag.fork else {
                continue;
            };
            for (branch_index, branch) in fork.branches.iter().enumerate() {
                for uuid in &branch.node_uuids {
                    self.branch_by_uuid.insert(
                        uuid.clone(),
                        NativeBranchMetadata {
                            fork_parent_uuid: fork.fork_parent_uuid.clone(),
                            branch_index,
                            branch_label: format!("Branch {}", branch_index + 1),
                            is_default: fork.default_branch == Some(branch_index),
                        },
                    );
                }
            }
            self.forks.push(NativeFeedFork {
                claude_session_id: dag.claude_session_id.clone(),
                file_id: dag.file_id,
                fork: fork.clone(),
            });
        }
        for (position, entry) in self.entries.iter_mut().enumerate() {
            let branch = entry
                .uuid
                .as_ref()
                .and_then(|uuid| self.branch_by_uuid.get(uuid).cloned());
            if entry.branch != branch {
                entry.branch = branch;
                if position < base_len {
                    self.changes.push((self.version, position));
                }
            }
        }
    }
}

/// Who wrote a native row, and the executor process it belongs to if any.
fn row_origin(row: &SessionNativeRecord) -> (NativeFeedOrigin, Option<Uuid>) {
    let linked = row
        .linked_execution_process_id
        .or(row.bound_turn_execution_process_id);
    let origin = if row.linked_execution_process_id.is_some() {
        NativeFeedOrigin::Executor
    } else if row.bound_turn_execution_process_id.is_some() || row.bound_queued_message_id.is_some()
    {
        NativeFeedOrigin::App
    } else {
        NativeFeedOrigin::Cli
    };
    (origin, linked)
}

/// Claude Code writes a multi-line or large bracketed paste (the chat
/// composer's route into the CLI) into the user turn as
/// `<pasted_content id="…">\n…\n</pasted_content id="…">`. Chat shows the
/// pasted text itself; the stored record keeps the wrapper. Returns `None`
/// when `content` has no complete wrapper.
fn unwrap_pasted_content(content: &str) -> Option<String> {
    const OPEN: &str = "<pasted_content id=\"";
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut changed = false;
    while let Some(start) = rest.find(OPEN) {
        let after_open = &rest[start + OPEN.len()..];
        let Some(id_end) = after_open.find("\">") else {
            break;
        };
        let id = &after_open[..id_end];
        let body_start = &after_open[id_end + 2..];
        let close = format!("</pasted_content id=\"{id}\">");
        let Some(close_at) = body_start.find(&close) else {
            break;
        };
        let body = &body_start[..close_at];
        let body = body.strip_prefix('\n').unwrap_or(body);
        let body = body.strip_suffix('\n').unwrap_or(body);
        out.push_str(&rest[..start]);
        out.push_str(body);
        rest = &body_start[close_at + close.len()..];
        changed = true;
    }
    changed.then(|| {
        out.push_str(rest);
        out
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const FIXTURE: &str = include_str!(
        "../../../../../docs/superpowers/specs/evidence/2026-07-20-cli-ui-seam/evidence-transcript.redacted.jsonl"
    );
    const FIXTURE_SID: &str = "06a7eacd-664b-4d9c-83f3-d4774a6216a8";
    const TOOL_SID: &str = "7b7b7b7b-7b7b-4b7b-8b7b-7b7b7b7b7b7b";
    const LATE_SID: &str = "5c5c5c5c-5c5c-4c5c-8c5c-5c5c5c5c5c5c";

    /// Deterministic xorshift; the sequences must be reproducible.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: usize) -> usize {
            (self.next() % bound as u64) as usize
        }
    }

    struct Line {
        sid: &'static str,
        file_id: Uuid,
        raw: String,
        disposition: CliNativeRecordDisposition,
    }

    fn record(sid: &str, uuid: &str, parent: Option<&str>, kind: &str, content: Value) -> String {
        json!({
            "type": kind,
            "sessionId": sid,
            "uuid": uuid,
            "parentUuid": parent,
            "timestamp": "2026-07-20T20:00:00Z",
            "message": { "role": kind, "content": content },
        })
        .to_string()
    }

    /// A linear tool-calling conversation (each tool result replaces its
    /// tool-use entry), then a fork from an earlier assistant turn that keeps
    /// growing on both branches, plus a sidechain and an unparseable line.
    fn tool_session(file_id: Uuid) -> Vec<Line> {
        let mut raws = Vec::new();
        let mut parent: Option<String> = None;
        let mut fork_point = String::new();
        for turn in 0..12 {
            let user = format!("tool-user-{turn}");
            raws.push(record(
                TOOL_SID,
                &user,
                parent.as_deref(),
                "user",
                json!(format!("run step {turn}")),
            ));
            let call = format!("tool-call-{turn}");
            raws.push(record(
                TOOL_SID,
                &call,
                Some(&user),
                "assistant",
                json!([{ "type": "tool_use", "id": format!("toolu_{turn}"), "name": "Bash",
                         "input": { "command": format!("echo {turn}") } }]),
            ));
            let result = format!("tool-result-{turn}");
            raws.push(record(
                TOOL_SID,
                &result,
                Some(&call),
                "user",
                json!([{ "type": "tool_result", "tool_use_id": format!("toolu_{turn}"),
                         "content": format!("{turn}\n") }]),
            ));
            let answer = format!("tool-answer-{turn}");
            raws.push(record(
                TOOL_SID,
                &answer,
                Some(&result),
                "assistant",
                json!([{ "type": "text", "text": format!("step {turn} done") }]),
            ));
            if turn == 4 {
                fork_point = answer.clone();
            }
            parent = Some(answer);
        }
        raws.push(
            json!({
                "type": "user", "sessionId": TOOL_SID, "uuid": "side-1",
                "parentUuid": parent, "isSidechain": true,
                "message": { "role": "user", "content": "subagent" },
            })
            .to_string(),
        );
        raws.push("{not json".to_string());
        // Rewind to turn 4 and continue on the new branch.
        let mut branch_parent = fork_point;
        for step in 0..6 {
            let uuid = format!("branch-{step}");
            let kind = if step % 2 == 0 { "user" } else { "assistant" };
            let content = if kind == "user" {
                json!(format!("branch prompt {step}"))
            } else {
                json!([{ "type": "text", "text": format!("branch answer {step}") }])
            };
            raws.push(record(TOOL_SID, &uuid, Some(&branch_parent), kind, content));
            branch_parent = uuid;
        }
        raws.push(
            json!({ "type": "summary", "sessionId": TOOL_SID, "leafUuid": "branch-5" }).to_string(),
        );
        raws.into_iter()
            .map(|raw| Line {
                sid: TOOL_SID,
                file_id,
                disposition: if raw.contains("\"isSidechain\":true") {
                    CliNativeRecordDisposition::Sidechain
                } else {
                    CliNativeRecordDisposition::Renderable
                },
                raw,
            })
            .collect()
    }

    /// A child written before its parent; the tracker cannot prove this.
    fn late_parent_session(file_id: Uuid) -> Vec<Line> {
        [
            record(
                LATE_SID,
                "late-2",
                Some("late-1"),
                "assistant",
                json!([{ "type": "text", "text": "child first" }]),
            ),
            record(LATE_SID, "late-1", None, "user", json!("parent second")),
            record(LATE_SID, "late-3", Some("late-2"), "user", json!("after")),
        ]
        .into_iter()
        .map(|raw| Line {
            sid: LATE_SID,
            file_id,
            raw,
            disposition: CliNativeRecordDisposition::Renderable,
        })
        .collect()
    }

    fn interleave(rng: &mut Rng, mut sources: Vec<Vec<Line>>) -> Vec<SessionNativeRecord> {
        for source in &mut sources {
            source.reverse();
        }
        let mut rows = Vec::new();
        let mut line_seqs = HashMap::<Uuid, i64>::new();
        while sources.iter().any(|source| !source.is_empty()) {
            let live = sources
                .iter()
                .enumerate()
                .filter(|(_, source)| !source.is_empty())
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let line = sources[live[rng.below(live.len())]].pop().unwrap();
            let line_seq = line_seqs.entry(line.file_id).or_default();
            rows.push(SessionNativeRecord {
                file_id: line.file_id,
                line_seq: *line_seq,
                claude_session_id: line.sid.to_string(),
                uuid: None,
                parent_uuid: None,
                kind: String::new(),
                ts: None,
                raw: line.raw,
                disposition: line.disposition.as_str().to_string(),
                linked_execution_process_id: None,
                bound_turn_execution_process_id: None,
                bound_queued_message_id: None,
                seq: rows.len() as i64 + 1,
                dir_path: "/tmp/native-projection-test".to_string(),
                file_name: format!("{}.jsonl", line.sid),
                generation: 1,
                last_import_at: None,
                link_cwd: "/tmp/native-projection-test".to_string(),
            });
            *line_seq += 1;
        }
        rows
    }

    fn session_rows(seed: u64) -> Vec<SessionNativeRecord> {
        let mut rng = Rng(seed);
        let fixture = FIXTURE
            .lines()
            .map(|raw| Line {
                sid: FIXTURE_SID,
                file_id: Uuid::from_u128(1),
                raw: raw.to_string(),
                disposition: CliNativeRecordDisposition::Renderable,
            })
            .collect();
        interleave(
            &mut rng,
            vec![
                fixture,
                tool_session(Uuid::from_u128(2)),
                late_parent_session(Uuid::from_u128(3)),
            ],
        )
    }

    fn entries_json(entries: &[NativeFeedEntry]) -> Vec<Value> {
        entries
            .iter()
            .map(|entry| serde_json::to_value(entry).unwrap())
            .collect()
    }

    /// Applies deltas the way the feed socket does: replace in place, then
    /// append at the exact index.
    struct Client {
        entries: Vec<Value>,
        forks: Vec<NativeFeedFork>,
        cursor: NativeFeedCursor,
    }

    impl Client {
        fn catch_up(&mut self, projection: &NativeProjection) {
            let delta = projection
                .delta_since(self.cursor)
                .expect("a cursor from the same build yields a delta");
            for (index, entry) in delta.replaced {
                self.entries[index] = serde_json::to_value(entry).unwrap();
            }
            assert_eq!(delta.appended_from, self.entries.len());
            self.entries.extend(entries_json(delta.appended));
            if delta.forks_changed {
                self.forks = projection.forks().to_vec();
            }
            self.cursor = projection.cursor();
        }
    }

    #[test]
    fn incremental_extend_matches_full_rebuild_for_every_append_sequence() {
        let mut saw_replacement = false;
        let mut saw_fork_change = false;
        for seed in 1..=24u64 {
            let rows = session_rows(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let mut rng = Rng(seed + 7);
            let mut consumed = rng.below(rows.len() / 4);
            let mut projection = NativeProjection::build(&rows[..consumed], 3);
            let mut client = Client {
                entries: entries_json(projection.entries()),
                forks: projection.forks().to_vec(),
                cursor: projection.cursor(),
            };
            while consumed < rows.len() {
                let batch = (1 + rng.below(6)).min(rows.len() - consumed);
                let visited = projection.rows_visited();
                let forks_before = projection.forks().to_vec();
                let changes_before = projection.changes.len();
                assert!(projection.can_extend(&rows[consumed..consumed + batch]));
                projection.extend(&rows[consumed..consumed + batch]);
                consumed += batch;
                assert_eq!(projection.rows_visited(), visited + batch as u64);
                saw_replacement |= projection.changes.len() > changes_before;
                saw_fork_change |= projection.forks() != forks_before;

                let full = NativeProjection::build(&rows[..consumed], 3);
                assert_eq!(
                    entries_json(projection.entries()),
                    entries_json(full.entries()),
                    "seed {seed}: entries diverged after {consumed} rows"
                );
                assert_eq!(projection.forks(), full.forks(), "seed {seed}");
                assert_eq!(projection.files(), full.files(), "seed {seed}");
                assert_eq!(projection.last_row_seq(), full.last_row_seq());

                // A consumer that skips some updates still converges.
                if rng.below(3) != 0 || consumed == rows.len() {
                    client.catch_up(&projection);
                    assert_eq!(client.entries, entries_json(full.entries()), "seed {seed}");
                    assert_eq!(client.forks, full.forks(), "seed {seed}");
                }
            }
        }
        assert!(saw_replacement, "no sequence replaced an entry in place");
        assert!(saw_fork_change, "no sequence changed the fork topology");
    }

    #[test]
    fn codex_rows_extend_exactly_like_a_full_build() {
        const CODEX: &str = include_str!(
            "../../../../executors/src/executors/codex/testdata/rollout-0.157.1.redacted.jsonl"
        );
        let file_id = Uuid::from_u128(7);
        let rows = CODEX
            .lines()
            .enumerate()
            .map(|(index, raw)| SessionNativeRecord {
                file_id,
                line_seq: index as i64,
                claude_session_id: "01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868".to_string(),
                uuid: None,
                parent_uuid: None,
                kind: String::new(),
                ts: None,
                raw: raw.to_string(),
                disposition: CliNativeRecordDisposition::Renderable.as_str().to_string(),
                linked_execution_process_id: None,
                bound_turn_execution_process_id: None,
                bound_queued_message_id: None,
                seq: index as i64 + 1,
                dir_path: "/sessions/2026/09/28".to_string(),
                file_name: "rollout-2026-09-28T12-20-29-01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868.jsonl"
                    .to_string(),
                generation: 0,
                last_import_at: None,
                link_cwd: "/workspace/demo".to_string(),
            })
            .collect::<Vec<_>>();
        let full = NativeProjection::build(&rows, 0);
        assert_eq!(full.entries().len(), 10);
        assert!(full.forks().is_empty());
        for split in 0..rows.len() {
            let mut projection = NativeProjection::build(&rows[..split], 0);
            let cursor = projection.cursor();
            assert!(projection.can_extend(&rows[split..]));
            projection.extend(&rows[split..]);
            assert_eq!(
                entries_json(projection.entries()),
                entries_json(full.entries())
            );
            let delta = projection.delta_since(cursor).unwrap();
            assert!(delta.replaced.is_empty() && !delta.forks_changed);
        }
    }

    #[test]
    fn pasted_content_wrappers_unwrap_to_the_pasted_text() {
        assert_eq!(
            unwrap_pasted_content(
                "fix this:\n\n<pasted_content id=\"9932\">\nline one\n    indented\n</pasted_content id=\"9932\">\nthanks"
            )
            .as_deref(),
            Some("fix this:\n\nline one\n    indented\nthanks")
        );
        assert_eq!(
            unwrap_pasted_content(
                "<pasted_content id=\"a1\">\nA\n</pasted_content id=\"a1\"> and <pasted_content id=\"b2\">\nB\n</pasted_content id=\"b2\">"
            )
            .as_deref(),
            Some("A and B")
        );
        // A bare mention or an unterminated wrapper is left alone.
        assert_eq!(
            unwrap_pasted_content("wraps in `<pasted_content>` tags"),
            None
        );
        assert_eq!(
            unwrap_pasted_content("<pasted_content id=\"c3\">\nhalf"),
            None
        );
    }

    #[test]
    fn a_cursor_from_another_build_needs_a_full_snapshot() {
        let rows = session_rows(42);
        let first = NativeProjection::build(&rows[..10], 0);
        let second = NativeProjection::build(&rows[..10], 0);
        assert!(second.delta_since(first.cursor()).is_none());
        assert!(first.delta_since(first.cursor()).is_some());
    }

    #[test]
    fn a_newer_file_generation_cannot_be_appended() {
        let rows = session_rows(9);
        let projection = NativeProjection::build(&rows[..20], 0);
        let mut replacement = rows[20].clone();
        replacement.file_id = Uuid::from_u128(99);
        replacement.file_name = rows[0].file_name.clone();
        assert!(!projection.can_extend(&[replacement]));
        assert!(projection.can_extend(&rows[20..21]));
    }
}

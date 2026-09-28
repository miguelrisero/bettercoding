//! Read-only adapter for Codex's native rollout files
//! (`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<thread id>.jsonl`).
//!
//! A rollout carries the same turn several times: model-facing
//! `response_item`s, legacy `event_msg`s, and, in a paginated-history rollout,
//! the typed `item_completed` thread items the Codex TUI renders. A paginated
//! rollout renders only `item_completed`; a legacy-history rollout (a thread an
//! older Codex or the app server started, which the TUI keeps appending to in
//! the same format) has none, and renders its message and tool events
//! instead. Either way a turn is shown exactly once and injected context
//! (AGENTS.md, environment, developer instructions) never appears as a user
//! message. Parsing is tolerant `serde_json::Value` access: rollouts from
//! newer Codex versions than the pinned protocol crates still adapt, and
//! anything unrecognised is reported as [`CodexRolloutDisposition::Unknown`]
//! instead of failing.

use serde_json::Value;
use workspace_utils::{diff::normalize_unified_diff, path::make_path_relative};

use crate::logs::{
    ActionType, CommandExitStatus, CommandRunResult, FileChange, NormalizedEntry,
    NormalizedEntryType, ToolResult, ToolStatus, utils::shell_command_parsing::CommandCategory,
};

const ROLLOUT_PREFIX: &str = "rollout-";
const ROLLOUT_SUFFIX: &str = ".jsonl";
const THREAD_ID_LEN: usize = 36;

/// The Codex thread id a rollout file name ends with, or `None` when the name
/// is not a rollout.
pub fn rollout_thread_id(file_name: &str) -> Option<&str> {
    let stem = file_name
        .strip_prefix(ROLLOUT_PREFIX)?
        .strip_suffix(ROLLOUT_SUFFIX)?;
    let id = stem.get(stem.len().checked_sub(THREAD_ID_LEN)?..)?;
    uuid::Uuid::parse_str(id).ok().map(|_| id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexRolloutDisposition {
    /// A completed thread item that renders as chat entries.
    Renderable,
    /// A known record that carries nothing to render (token counts, turn
    /// context, model-facing duplicates, empty reasoning).
    Bookkeeping,
    /// A record type or thread item this adapter does not know.
    Unknown,
}

/// One adapted rollout line.
#[derive(Debug, Clone)]
pub struct CodexRolloutLine {
    pub disposition: CodexRolloutDisposition,
    /// `user`, `assistant`, `tool`, `system`, the raw record type for
    /// bookkeeping, or `unknown`.
    pub kind: String,
    pub item_id: Option<String>,
    pub turn_id: Option<String>,
    pub timestamp: Option<String>,
    pub entries: Vec<NormalizedEntry>,
}

impl CodexRolloutLine {
    fn new(disposition: CodexRolloutDisposition, kind: &str, record: &Value) -> Self {
        Self {
            disposition,
            kind: kind.to_string(),
            item_id: None,
            turn_id: None,
            timestamp: str_at(record, "timestamp"),
            entries: Vec::new(),
        }
    }
}

/// Record types every current rollout carries that never render.
const BOOKKEEPING_RECORDS: &[&str] = &[
    "session_meta",
    "response_item",
    "turn_context",
    "world_state",
    "token_usage_record",
    "compacted",
    "inter_agent_communication_metadata",
];

/// `event_msg` types every current rollout carries besides `item_completed`.
const BOOKKEEPING_EVENTS: &[&str] = &[
    "token_count",
    "task_started",
    "task_complete",
    "turn_aborted",
    "agent_message",
    "user_message",
    "thread_settings_applied",
    "thread_goal_updated",
    "patch_apply_end",
    "context_compacted",
    "web_search_end",
    "mcp_tool_call_end",
];

/// Thread items that are known but have no chat rendering.
const BOOKKEEPING_ITEMS: &[&str] = &["SubAgentActivity", "CollabAgentToolCall"];

/// `Extension` kinds besides web search: tool plumbing the TUI shows no
/// transcript cell for.
const BOOKKEEPING_EXTENSIONS: &[&str] = &["clock.sleep", "image_gen.generation"];

/// Whether a rollout whose first line is `session_meta_line` records turns
/// as legacy events rather than `item_completed` thread items. Only a
/// `history_mode` of `paginated` has thread items; Codex versions before the
/// field existed wrote legacy history.
pub fn rollout_has_legacy_history(session_meta_line: &str) -> Option<bool> {
    let record: Value = serde_json::from_str(session_meta_line).ok()?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let mode = record
        .get("payload")
        .and_then(|payload| payload.get("history_mode"))
        .and_then(Value::as_str);
    Some(mode != Some("paginated"))
}

/// Adapt one rollout line. `legacy_history` renders the message and tool
/// events of a legacy-history rollout; a paginated rollout repeats some of
/// them next to its thread items, so they stay bookkeeping there.
pub fn adapt_codex_rollout_line(
    raw: &str,
    worktree_path: &str,
    legacy_history: bool,
) -> Result<CodexRolloutLine, serde_json::Error> {
    let record: Value = serde_json::from_str(raw)?;
    let record_type = record.get("type").and_then(Value::as_str).unwrap_or("");
    let payload = record.get("payload").unwrap_or(&Value::Null);

    if record_type == "event_msg" {
        let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type == "item_completed" {
            return Ok(adapt_item(&record, payload, worktree_path));
        }
        if legacy_history
            && let Some(line) = adapt_legacy_event(&record, payload, event_type, worktree_path)
        {
            return Ok(line);
        }
        // Lifecycle and legacy message events duplicate or annotate the
        // completed items.
        let disposition = if BOOKKEEPING_EVENTS.contains(&event_type) {
            CodexRolloutDisposition::Bookkeeping
        } else {
            CodexRolloutDisposition::Unknown
        };
        let kind = if disposition == CodexRolloutDisposition::Unknown {
            "unknown"
        } else {
            record_type
        };
        return Ok(CodexRolloutLine::new(disposition, kind, &record));
    }
    let disposition = if BOOKKEEPING_RECORDS.contains(&record_type) {
        CodexRolloutDisposition::Bookkeeping
    } else {
        CodexRolloutDisposition::Unknown
    };
    let kind = if disposition == CodexRolloutDisposition::Unknown {
        "unknown"
    } else {
        record_type
    };
    Ok(CodexRolloutLine::new(disposition, kind, &record))
}

fn adapt_item(record: &Value, payload: &Value, worktree_path: &str) -> CodexRolloutLine {
    let item = payload.get("item").unwrap_or(&Value::Null);
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    let timestamp = str_at(record, "timestamp");
    let entry = |entry_type, content: String| NormalizedEntry {
        timestamp: timestamp.clone(),
        entry_type,
        content,
        metadata: None,
    };

    let (kind, entries) = match item_type {
        "UserMessage" => {
            let text = joined_text(item.get("content"), "text");
            ("user", vec![entry(NormalizedEntryType::UserMessage, text)])
        }
        "AgentMessage" => {
            let text = joined_text(item.get("content"), "Text");
            let entries = if text.is_empty() {
                Vec::new()
            } else {
                vec![entry(NormalizedEntryType::AssistantMessage, text)]
            };
            ("assistant", entries)
        }
        "Reasoning" => {
            let summary = item
                .get("summary_text")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            let entries = if summary.trim().is_empty() {
                Vec::new()
            } else {
                vec![entry(NormalizedEntryType::Thinking, summary)]
            };
            ("thinking", entries)
        }
        "CommandExecution" => {
            let command = display_command(item.get("command"));
            let output = str_at(item, "formatted_output")
                .filter(|output| !output.is_empty())
                .or_else(|| str_at(item, "aggregated_output"));
            let entry_type = NormalizedEntryType::ToolUse {
                tool_name: "bash".to_string(),
                action_type: ActionType::CommandRun {
                    command: command.clone(),
                    result: Some(CommandRunResult {
                        exit_status: item
                            .get("exit_code")
                            .and_then(Value::as_i64)
                            .map(|code| CommandExitStatus::ExitCode { code: code as i32 }),
                        output,
                    }),
                    category: CommandCategory::from_command(&command),
                },
                status: item_status(item),
            };
            ("tool", vec![entry(entry_type, command)])
        }
        "McpToolCall" => (
            "tool",
            vec![mcp_entry(
                item,
                item.get("result"),
                item_status(item),
                timestamp.clone(),
            )],
        ),
        "FileChange" => (
            "tool",
            file_change_entries(
                item.get("changes"),
                item_status(item),
                worktree_path,
                &timestamp,
            ),
        ),
        "Extension" if item.get("kind").and_then(Value::as_str) == Some("web.search") => {
            ("tool", vec![web_search_entry(item, timestamp.clone())])
        }
        "ImageView" => {
            let path = str_at(item, "path").unwrap_or_default();
            let path = path.strip_prefix("file://").unwrap_or(&path);
            let relative = make_path_relative(path, worktree_path);
            let entry_type = NormalizedEntryType::ToolUse {
                tool_name: "view_image".to_string(),
                action_type: ActionType::FileRead {
                    path: relative.clone(),
                },
                status: ToolStatus::Success,
            };
            ("tool", vec![entry(entry_type, relative)])
        }
        "ContextCompaction" => (
            "system",
            vec![entry(
                NormalizedEntryType::SystemMessage,
                "Context compacted".to_string(),
            )],
        ),
        "Extension"
            if item
                .get("kind")
                .and_then(Value::as_str)
                .is_some_and(|kind| BOOKKEEPING_EXTENSIONS.contains(&kind)) =>
        {
            ("bookkeeping", Vec::new())
        }
        known if BOOKKEEPING_ITEMS.contains(&known) => ("bookkeeping", Vec::new()),
        _ => {
            let mut line =
                CodexRolloutLine::new(CodexRolloutDisposition::Unknown, "unknown", record);
            line.item_id = str_at(item, "id");
            line.turn_id = str_at(payload, "turn_id");
            return line;
        }
    };

    let disposition = if entries.is_empty() {
        CodexRolloutDisposition::Bookkeeping
    } else {
        CodexRolloutDisposition::Renderable
    };
    CodexRolloutLine {
        disposition,
        kind: kind.to_string(),
        item_id: str_at(item, "id"),
        turn_id: str_at(payload, "turn_id"),
        timestamp,
        entries,
    }
}

/// A legacy-history `event_msg` that renders, or `None` for any other event.
fn adapt_legacy_event(
    record: &Value,
    payload: &Value,
    event_type: &str,
    worktree_path: &str,
) -> Option<CodexRolloutLine> {
    let timestamp = str_at(record, "timestamp");
    let entry = |entry_type, content: String| NormalizedEntry {
        timestamp: timestamp.clone(),
        entry_type,
        content,
        metadata: None,
    };
    let message = || str_at(payload, "message").filter(|text| !text.trim().is_empty());
    let succeeded = || match payload.get("success").and_then(Value::as_bool) {
        Some(false) => ToolStatus::Failed,
        _ => ToolStatus::Success,
    };
    let (kind, entries) = match event_type {
        "user_message" => (
            "user",
            message()
                .map(|text| entry(NormalizedEntryType::UserMessage, text))
                .into_iter()
                .collect(),
        ),
        "agent_message" => (
            "assistant",
            message()
                .map(|text| entry(NormalizedEntryType::AssistantMessage, text))
                .into_iter()
                .collect(),
        ),
        "patch_apply_end" => (
            "tool",
            file_change_entries(
                payload.get("changes"),
                succeeded(),
                worktree_path,
                &timestamp,
            ),
        ),
        "mcp_tool_call_end" => {
            let invocation = payload.get("invocation").unwrap_or(&Value::Null);
            let result = payload.get("result");
            let status = match result {
                Some(result) if result.get("Err").is_some() => ToolStatus::Failed,
                _ => ToolStatus::Success,
            };
            let result = result.map(|result| result.get("Ok").unwrap_or(result));
            (
                "tool",
                vec![mcp_entry(invocation, result, status, timestamp.clone())],
            )
        }
        "web_search_end" => ("tool", vec![web_search_entry(payload, timestamp.clone())]),
        "context_compacted" => (
            "system",
            vec![entry(
                NormalizedEntryType::SystemMessage,
                "Context compacted".to_string(),
            )],
        ),
        _ => return None,
    };
    let disposition = if entries.is_empty() {
        CodexRolloutDisposition::Bookkeeping
    } else {
        CodexRolloutDisposition::Renderable
    };
    Some(CodexRolloutLine {
        disposition,
        kind: kind.to_string(),
        item_id: str_at(payload, "call_id"),
        turn_id: str_at(payload, "turn_id"),
        timestamp,
        entries,
    })
}

/// An MCP call from its `server`, `tool` and `arguments`, with `result`.
fn mcp_entry(
    call: &Value,
    result: Option<&Value>,
    status: ToolStatus,
    timestamp: Option<String>,
) -> NormalizedEntry {
    let server = str_at(call, "server").unwrap_or_default();
    let tool = str_at(call, "tool").unwrap_or_default();
    let tool_name = format!("mcp:{server}:{tool}");
    NormalizedEntry {
        timestamp,
        entry_type: NormalizedEntryType::ToolUse {
            tool_name: tool_name.clone(),
            action_type: ActionType::Tool {
                tool_name,
                arguments: call.get("arguments").cloned(),
                result: result.map(mcp_result),
            },
            status,
        },
        content: tool,
        metadata: None,
    }
}

/// One edit entry per changed path in a `{path: change}` map.
fn file_change_entries(
    changes: Option<&Value>,
    status: ToolStatus,
    worktree_path: &str,
    timestamp: &Option<String>,
) -> Vec<NormalizedEntry> {
    let Some(changes) = changes.and_then(Value::as_object) else {
        return Vec::new();
    };
    changes
        .iter()
        .map(|(path, change)| {
            let relative = make_path_relative(path, worktree_path);
            NormalizedEntry {
                timestamp: timestamp.clone(),
                entry_type: NormalizedEntryType::ToolUse {
                    tool_name: "edit".to_string(),
                    action_type: ActionType::FileEdit {
                        path: relative.clone(),
                        changes: file_changes(&relative, change, worktree_path),
                    },
                    status: status.clone(),
                },
                content: relative,
                metadata: None,
            }
        })
        .collect()
}

fn web_search_entry(search: &Value, timestamp: Option<String>) -> NormalizedEntry {
    let query = str_at(search, "query")
        .filter(|query| !query.is_empty())
        .unwrap_or_else(|| "Web search".to_string());
    NormalizedEntry {
        timestamp,
        entry_type: NormalizedEntryType::ToolUse {
            tool_name: "web_search".to_string(),
            action_type: ActionType::WebFetch { url: query.clone() },
            status: ToolStatus::Success,
        },
        content: query,
        metadata: None,
    }
}

fn str_at(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Concatenated `text` of every content part whose `type` is `part_type`.
fn joined_text(content: Option<&Value>, part_type: &str) -> String {
    content
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some(part_type))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `["/bin/zsh", "-lc", "<script>"]` shows as the script; anything else as
/// its words joined.
fn display_command(command: Option<&Value>) -> String {
    let words = command
        .and_then(Value::as_array)
        .map(|words| words.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    match words.as_slice() {
        [_, flag, script] if flag.starts_with('-') && flag.ends_with('c') => script.to_string(),
        _ => words.join(" "),
    }
}

fn item_status(item: &Value) -> ToolStatus {
    match item.get("status").and_then(Value::as_str) {
        Some("completed") => ToolStatus::Success,
        Some("declined") => ToolStatus::Denied { reason: None },
        Some("failed") => ToolStatus::Failed,
        _ => ToolStatus::Created,
    }
}

fn mcp_result(result: &Value) -> ToolResult {
    let content = result.get("content").and_then(Value::as_array);
    let texts = content.and_then(|blocks| {
        blocks
            .iter()
            .map(|block| {
                (block.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| block.get("text").and_then(Value::as_str))
                    .flatten()
            })
            .collect::<Option<Vec<_>>>()
    });
    match texts {
        Some(texts) => ToolResult::markdown(texts.join("\n")),
        None => ToolResult {
            r#type: crate::logs::ToolResultValueType::Json,
            value: result
                .get("structured_content")
                .cloned()
                .unwrap_or_else(|| result.clone()),
        },
    }
}

fn file_changes(relative: &str, change: &Value, worktree_path: &str) -> Vec<FileChange> {
    match change.get("type").and_then(Value::as_str) {
        Some("add") => vec![FileChange::Write {
            content: str_at(change, "content").unwrap_or_default(),
        }],
        Some("delete") => vec![FileChange::Delete],
        _ => {
            let mut changes = Vec::new();
            if let Some(dest) = change.get("move_path").and_then(Value::as_str) {
                changes.push(FileChange::Rename {
                    new_path: make_path_relative(dest, worktree_path),
                });
            }
            let diff = str_at(change, "unified_diff").unwrap_or_default();
            changes.push(FileChange::Edit {
                unified_diff: normalize_unified_diff(relative, &diff),
                has_line_numbers: true,
            });
            changes
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Redacted rollout written by the installed Codex TUI (0.157.1), with one
    /// tool-using turn spliced in from real 0.15x item shapes, plus a record
    /// and an item type no Codex writes.
    const FIXTURE: &str = include_str!("testdata/rollout-0.157.1.redacted.jsonl");

    fn adapt_all() -> Vec<CodexRolloutLine> {
        FIXTURE
            .lines()
            .map(|line| adapt_codex_rollout_line(line, "/workspace/demo", false).unwrap())
            .collect()
    }

    #[test]
    fn thread_id_comes_from_the_rollout_file_name() {
        assert_eq!(
            rollout_thread_id(
                "rollout-2026-09-28T12-20-29-01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868.jsonl"
            ),
            Some("01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868")
        );
        assert_eq!(
            rollout_thread_id("01a0e7f5-9f42-73e0-9b4d-2dfa51b6f868.jsonl"),
            None
        );
        assert_eq!(rollout_thread_id("rollout-short.jsonl"), None);
    }

    #[test]
    fn a_rollout_renders_each_turn_once_without_injected_context() {
        let lines = adapt_all();
        let rendered: Vec<_> = lines
            .iter()
            .flat_map(|line| line.entries.iter())
            .map(|entry| match &entry.entry_type {
                NormalizedEntryType::UserMessage => format!("user: {}", entry.content),
                NormalizedEntryType::AssistantMessage => format!("assistant: {}", entry.content),
                NormalizedEntryType::ToolUse { tool_name, .. } => {
                    format!("tool {tool_name}: {}", entry.content)
                }
                NormalizedEntryType::SystemMessage => format!("system: {}", entry.content),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            rendered,
            [
                "user: Reply with OK only.",
                "assistant: OK",
                "user: List the files, then add a greeting to README.md.",
                "assistant: OK",
                "tool bash: ls",
                "tool mcp:docs:search: search",
                "tool web_search: markdown greeting",
                "tool edit: README.md",
                "system: Context compacted",
                "assistant: Added a greeting to README.md.",
            ]
        );
        assert!(
            lines
                .iter()
                .filter(|line| line.disposition == CodexRolloutDisposition::Renderable)
                .all(|line| line.item_id.is_some() && line.turn_id.is_some())
        );
    }

    #[test]
    fn unknown_records_and_items_are_reported_not_rendered() {
        let unknown: Vec<_> = adapt_all()
            .into_iter()
            .filter(|line| line.disposition == CodexRolloutDisposition::Unknown)
            .collect();
        assert_eq!(unknown.len(), 2);
        assert!(unknown.iter().all(|line| line.entries.is_empty()));
        assert!(adapt_codex_rollout_line("{not json", "/", false).is_err());
        for line in [
            r#"{"type":"event_msg","payload":{"type":"hologram_delta"}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension","kind":"holo.call","id":"x"}}}"#,
        ] {
            assert_eq!(
                adapt_codex_rollout_line(line, "/", false)
                    .unwrap()
                    .disposition,
                CodexRolloutDisposition::Unknown
            );
        }
        for line in [
            r#"{"type":"event_msg","payload":{"type":"token_count"}}"#,
            r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"Extension","kind":"clock.sleep","id":"x"}}}"#,
        ] {
            assert_eq!(
                adapt_codex_rollout_line(line, "/", false)
                    .unwrap()
                    .disposition,
                CodexRolloutDisposition::Bookkeeping
            );
        }
    }

    /// A thread the app server (0.124.0) started, resumed and continued by
    /// the Codex TUI (0.158.0), which keeps its legacy history format. The
    /// last turn splices in the remaining legacy event shapes seen in local
    /// 0.144 to 0.154 rollouts, with synthetic values.
    const LEGACY_FIXTURE: &str = include_str!("testdata/rollout-0.124.0-legacy.redacted.jsonl");

    fn rendered(fixture: &str, legacy_history: bool) -> Vec<String> {
        fixture
            .lines()
            .map(|line| adapt_codex_rollout_line(line, "/workspace/demo", legacy_history).unwrap())
            .flat_map(|line| line.entries)
            .map(|entry| match &entry.entry_type {
                NormalizedEntryType::UserMessage => format!("user: {}", entry.content),
                NormalizedEntryType::AssistantMessage => format!("assistant: {}", entry.content),
                NormalizedEntryType::ToolUse {
                    tool_name, status, ..
                } => format!("tool {tool_name} {status:?}: {}", entry.content),
                NormalizedEntryType::SystemMessage => format!("system: {}", entry.content),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn the_history_mode_comes_from_session_meta() {
        let first = |fixture: &str| fixture.lines().next().unwrap().to_string();
        assert_eq!(
            rollout_has_legacy_history(&first(LEGACY_FIXTURE)),
            Some(true)
        );
        assert_eq!(rollout_has_legacy_history(&first(FIXTURE)), Some(false));
        assert_eq!(
            rollout_has_legacy_history(r#"{"type":"event_msg","payload":{}}"#),
            None
        );
        assert_eq!(rollout_has_legacy_history("{not json"), None);
    }

    #[test]
    fn a_legacy_history_rollout_renders_its_message_and_tool_events() {
        assert_eq!(
            rendered(LEGACY_FIXTURE, true),
            [
                "user: Remember the codeword PAPAYA. Reply with just OK.",
                "user: Also remember the second codeword MANGO. Reply with just OK2.",
                "assistant: OK2",
                "user: Use apply_patch to create notes.txt containing the line hello, then run ls and tell me the file count.",
                "assistant: I’ll create `notes.txt`, then list the directory and count its files.",
                "tool edit Success: notes.txt",
                "assistant: Created `notes.txt` containing `hello`. `ls` shows 4 entries, so the file count is 4.",
                "user: Search the docs, then compact.",
                "tool mcp:docs:search Success: search",
                "tool mcp:docs:fetch Failed: fetch",
                "tool web_search Success: markdown greeting",
                "system: Context compacted",
            ]
        );
        let lines: Vec<_> = LEGACY_FIXTURE
            .lines()
            .map(|line| adapt_codex_rollout_line(line, "/workspace/demo", true).unwrap())
            .collect();
        assert!(
            lines
                .iter()
                .all(|line| line.disposition != CodexRolloutDisposition::Unknown)
        );
        let edit = lines
            .iter()
            .flat_map(|line| &line.entries)
            .find_map(|entry| match &entry.entry_type {
                NormalizedEntryType::ToolUse {
                    action_type: ActionType::FileEdit { changes, .. },
                    ..
                } => Some(changes.clone()),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            edit.as_slice(),
            [FileChange::Write { content }] if content == "hello\n"
        ));
    }

    #[test]
    fn a_paginated_rollout_never_renders_its_legacy_events() {
        // A paginated rollout can repeat turns as legacy events next to its
        // thread items; those stay bookkeeping.
        assert!(rendered(LEGACY_FIXTURE, false).is_empty());
        assert_eq!(rendered(FIXTURE, true), rendered(FIXTURE, false));
    }

    #[test]
    fn tool_items_carry_their_results() {
        let lines = adapt_all();
        let command = lines
            .iter()
            .flat_map(|line| &line.entries)
            .find_map(|entry| match &entry.entry_type {
                NormalizedEntryType::ToolUse {
                    action_type: ActionType::CommandRun { result, .. },
                    status,
                    ..
                } => Some((result.clone().unwrap(), status.clone())),
                _ => None,
            })
            .unwrap();
        assert_eq!(command.0.output.as_deref(), Some("README.md\nsrc\n"));
        assert!(matches!(
            command.0.exit_status,
            Some(CommandExitStatus::ExitCode { code: 0 })
        ));
        assert!(matches!(command.1, ToolStatus::Success));
        let edit = lines
            .iter()
            .flat_map(|line| &line.entries)
            .find_map(|entry| match &entry.entry_type {
                NormalizedEntryType::ToolUse {
                    action_type: ActionType::FileEdit { changes, .. },
                    ..
                } => Some(changes.clone()),
                _ => None,
            })
            .unwrap();
        assert!(matches!(
            edit.as_slice(),
            [FileChange::Edit { unified_diff, .. }] if unified_diff.contains("+Hello!")
        ));
    }
}

//! Adapter for Claude Code's private, camelCase native JSONL store.
//!
//! The public surface deliberately stops at envelope metadata and normalized
//! changes. `ClaudeJson` and the executor's state machine remain implementation
//! details of the Claude executor module.

use serde::{Deserialize, Serialize};

use super::{ClaudeJson, ClaudeLogProcessor, ClaudeMessage, ClaudeMessageContent, HistoryStrategy};
use crate::logs::{
    NormalizedEntry,
    utils::{EntryIndexProvider, patch::extract_normalized_entry_from_patch},
};

const BOOKKEEPING_KINDS: &[&str] = &[
    "attachment",
    "queue-operation",
    "last-prompt",
    "mode",
    "bridge-session",
    "permission-mode",
    // Native `system` records are store bookkeeping, not stream-json init.
    "system",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NativeClaudeEnvelopeMetadata {
    pub claude_session_id: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub timestamp: Option<String>,
    pub version: Option<String>,
    pub git_branch: Option<String>,
    pub kind: String,
    pub leaf_uuid: Option<String>,
    pub is_sidechain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeClaudeSkipReason {
    Bookkeeping,
    Sidechain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeClaudeDisposition {
    Renderable,
    Skip(NativeClaudeSkipReason),
    Unknown,
}

#[derive(Debug, Clone)]
pub struct NativeClaudeLine {
    metadata: NativeClaudeEnvelopeMetadata,
    disposition: NativeClaudeDisposition,
    // Deliberately private: services can normalize through the adapter without
    // taking a dependency on the executor's stream-json representation.
    event: Option<ClaudeJson>,
}

impl NativeClaudeLine {
    pub fn metadata(&self) -> &NativeClaudeEnvelopeMetadata {
        &self.metadata
    }

    pub fn disposition(&self) -> NativeClaudeDisposition {
        self.disposition
    }

    pub fn is_unknown(&self) -> bool {
        self.disposition == NativeClaudeDisposition::Unknown
    }

    pub fn is_sidechain(&self) -> bool {
        matches!(
            self.disposition,
            NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Sidechain)
        )
    }

    pub fn plain_user_text(&self) -> Option<String> {
        let Some(ClaudeJson::User { message, .. }) = self.event.as_ref() else {
            return None;
        };

        match &message.content {
            ClaudeMessageContent::Text(text) => Some(text.clone()),
            ClaudeMessageContent::Array(items) => {
                let text = items
                    .iter()
                    .filter_map(|item| match item {
                        super::ClaudeContentItem::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then_some(text)
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeClaudeWireEnvelope {
    #[serde(rename = "type")]
    kind: Option<String>,
    session_id: Option<String>,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    timestamp: Option<String>,
    version: Option<String>,
    git_branch: Option<String>,
    #[serde(default)]
    is_sidechain: bool,
    leaf_uuid: Option<String>,
    // Keep the envelope tolerant even if a newer/private content-block shape
    // is not yet representable by `ClaudeMessage`.
    message: Option<serde_json::Value>,
    #[serde(default, rename = "isSynthetic")]
    is_synthetic: bool,
    #[serde(default, rename = "isReplay")]
    is_replay: bool,
}

/// Parse one complete native-store line. Missing `sessionId` is attributed to
/// the file sid; unknown record kinds remain distinguishable from malformed
/// JSON so callers can persist and count them without aborting a tail pass.
pub fn adapt_native_claude_line(
    raw: &str,
    file_session_id: &str,
) -> Result<NativeClaudeLine, serde_json::Error> {
    let wire: NativeClaudeWireEnvelope = serde_json::from_str(raw)?;
    let kind = wire.kind.unwrap_or_else(|| "unknown".to_string());
    let session_id = wire
        .session_id
        .clone()
        .unwrap_or_else(|| file_session_id.to_string());
    let metadata = NativeClaudeEnvelopeMetadata {
        claude_session_id: session_id.clone(),
        uuid: wire.uuid.clone(),
        parent_uuid: wire.parent_uuid,
        timestamp: wire.timestamp,
        version: wire.version,
        git_branch: wire.git_branch,
        kind: kind.clone(),
        leaf_uuid: wire.leaf_uuid,
        is_sidechain: wire.is_sidechain,
    };

    if wire.is_sidechain {
        return Ok(NativeClaudeLine {
            metadata,
            disposition: NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Sidechain),
            event: None,
        });
    }

    if BOOKKEEPING_KINDS.contains(&kind.as_str()) {
        return Ok(NativeClaudeLine {
            metadata,
            disposition: NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Bookkeeping),
            event: None,
        });
    }

    let event = match (kind.as_str(), wire.message) {
        ("user", Some(message)) => ClaudeJson::User {
            message: match serde_json::from_value::<ClaudeMessage>(message) {
                Ok(message) => message,
                Err(_) => {
                    return Ok(NativeClaudeLine {
                        metadata,
                        disposition: NativeClaudeDisposition::Unknown,
                        event: None,
                    });
                }
            },
            session_id: Some(session_id),
            uuid: wire.uuid,
            is_synthetic: wire.is_synthetic,
            is_replay: wire.is_replay,
        },
        ("assistant", Some(message)) => ClaudeJson::Assistant {
            message: match serde_json::from_value::<ClaudeMessage>(message) {
                Ok(message) => message,
                Err(_) => {
                    return Ok(NativeClaudeLine {
                        metadata,
                        disposition: NativeClaudeDisposition::Unknown,
                        event: None,
                    });
                }
            },
            session_id: Some(session_id),
            uuid: wire.uuid,
        },
        _ => {
            return Ok(NativeClaudeLine {
                metadata,
                disposition: NativeClaudeDisposition::Unknown,
                event: None,
            });
        }
    };

    Ok(NativeClaudeLine {
        metadata,
        disposition: NativeClaudeDisposition::Renderable,
        event: Some(event),
    })
}

#[derive(Debug, Clone)]
pub struct NativeNormalizedChange {
    pub index: usize,
    pub entry: NormalizedEntry,
}

/// Stateful native projection. Tool-use records and their later tool-result
/// records must share one processor, exactly as they do in stream-json.
pub struct NativeClaudeNormalizer {
    processor: ClaudeLogProcessor,
    index_provider: EntryIndexProvider,
}

impl Default for NativeClaudeNormalizer {
    fn default() -> Self {
        Self {
            processor: ClaudeLogProcessor::new_with_strategy(HistoryStrategy::NativeClaude),
            index_provider: EntryIndexProvider::default(),
        }
    }
}

impl NativeClaudeNormalizer {
    pub fn normalize(
        &mut self,
        line: &NativeClaudeLine,
        worktree_path: &str,
    ) -> Vec<NativeNormalizedChange> {
        let Some(event) = line.event.as_ref() else {
            return Vec::new();
        };

        self.processor
            .normalize_entries(event, worktree_path, &self.index_provider)
            .iter()
            .filter_map(extract_normalized_entry_from_patch)
            .map(|(index, mut entry)| {
                entry.timestamp = line.metadata.timestamp.clone();
                NativeNormalizedChange { index, entry }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::NormalizedEntryType;

    fn user_line(content: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"u-1","parentUuid":"p-1","timestamp":"2026-07-20T00:00:00Z","message":{{"role":"user","content":{content}}}}}"#
        )
    }

    #[test]
    fn parses_camel_case_and_attributes_missing_session_id() {
        let line = adapt_native_claude_line(&user_line(r#""hello""#), "file-session").unwrap();
        let metadata = line.metadata();
        assert_eq!(metadata.claude_session_id, "file-session");
        assert_eq!(metadata.parent_uuid.as_deref(), Some("p-1"));
        assert_eq!(metadata.uuid.as_deref(), Some("u-1"));
        assert_eq!(line.plain_user_text().as_deref(), Some("hello"));
    }

    #[test]
    fn native_strategy_emits_plain_user_as_user_message() {
        let line = adapt_native_claude_line(&user_line(r#""hello""#), "sid").unwrap();
        let changes = NativeClaudeNormalizer::default().normalize(&line, "");
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0].entry.entry_type,
            NormalizedEntryType::UserMessage
        ));
        assert_eq!(changes[0].entry.content, "hello");
    }

    fn normalize_with_strategy(
        line: &NativeClaudeLine,
        strategy: HistoryStrategy,
    ) -> Vec<NormalizedEntry> {
        let mut processor = ClaudeLogProcessor::new_with_strategy(strategy);
        let provider = EntryIndexProvider::default();
        processor
            .normalize_entries(line.event.as_ref().unwrap(), "", &provider)
            .iter()
            .filter_map(extract_normalized_entry_from_patch)
            .map(|(_, entry)| entry)
            .collect()
    }

    #[test]
    fn strategy_matrix_keeps_default_and_amp_behavior() {
        let string_line = adapt_native_claude_line(&user_line(r#""hello""#), "sid").unwrap();

        let default_entries = normalize_with_strategy(&string_line, HistoryStrategy::Default);
        assert!(
            default_entries
                .iter()
                .all(|entry| !matches!(entry.entry_type, NormalizedEntryType::UserMessage))
        );
        assert!(
            default_entries
                .iter()
                .any(|entry| matches!(entry.entry_type, NormalizedEntryType::SystemMessage))
        );

        let amp_string_entries = normalize_with_strategy(&string_line, HistoryStrategy::AmpResume);
        assert!(
            amp_string_entries
                .iter()
                .all(|entry| !matches!(entry.entry_type, NormalizedEntryType::UserMessage))
        );

        let array_line =
            adapt_native_claude_line(&user_line(r#"[{"type":"text","text":"hello"}]"#), "sid")
                .unwrap();
        let default_array = normalize_with_strategy(&array_line, HistoryStrategy::Default);
        assert!(default_array.is_empty());
        let amp_array = normalize_with_strategy(&array_line, HistoryStrategy::AmpResume);
        assert!(
            amp_array
                .iter()
                .any(|entry| matches!(entry.entry_type, NormalizedEntryType::UserMessage))
        );
        let native_array = normalize_with_strategy(&array_line, HistoryStrategy::NativeClaude);
        assert!(
            native_array
                .iter()
                .any(|entry| matches!(entry.entry_type, NormalizedEntryType::UserMessage))
        );
    }

    #[test]
    fn bookkeeping_is_skipped_and_unknown_is_tolerated() {
        let bookkeeping =
            adapt_native_claude_line(r#"{"type":"last-prompt","leafUuid":"leaf"}"#, "sid").unwrap();
        assert!(matches!(
            bookkeeping.disposition(),
            NativeClaudeDisposition::Skip(NativeClaudeSkipReason::Bookkeeping)
        ));
        assert_eq!(bookkeeping.metadata().leaf_uuid.as_deref(), Some("leaf"));

        let unknown = adapt_native_claude_line(r#"{"type":"future-kind"}"#, "sid").unwrap();
        assert!(unknown.is_unknown());
        assert_eq!(unknown.metadata().kind, "future-kind");
    }
}

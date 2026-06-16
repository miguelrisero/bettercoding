//! Best-effort workspace title + branch-slug generation from the first message.
//!
//! Runs a fast Claude Haiku call (reusing the same pinned claude CLI the
//! executor uses) to turn a workspace's first message into a concise human
//! title and a git branch slug — a smarter replacement for slugifying the first
//! few words. It is bounded by a short timeout and returns `None` on any
//! failure, so callers always fall back to heuristic naming and workspace
//! creation is never blocked for long or broken by a slow/absent model.

use std::{process::Stdio, time::Duration};

use serde::Deserialize;

use crate::executors::claude::base_command;

/// A generated workspace title (human-facing) and branch slug (git-safe-ish; the
/// container re-sanitizes it before use).
#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceNames {
    pub title: String,
    pub branch_slug: String,
}

#[derive(Deserialize)]
struct RawNames {
    #[serde(default)]
    title: String,
    #[serde(default)]
    branch: String,
}

const TITLE_MAX_CHARS: usize = 72;
const BRANCH_MAX_CHARS: usize = 40;
const PROMPT_MAX_CHARS: usize = 2000;
const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Ask Claude Haiku for a concise title + branch slug describing `first_message`.
/// Returns `None` on any failure (spawn error, non-zero exit, timeout, or
/// unparsable output) so the caller can fall back to heuristic naming.
pub async fn generate_workspace_names(first_message: &str) -> Option<WorkspaceNames> {
    let task = first_message.trim();
    if task.is_empty() {
        return None;
    }
    let task: String = task.chars().take(PROMPT_MAX_CHARS).collect();
    let prompt = format!(
        "Summarize the following task as a PR title and a git branch slug. \
         Reply with ONLY a minified JSON object, no prose and no code fence: \
         {{\"title\":\"imperative summary, at most 6 words, no trailing period\",\
         \"branch\":\"lowercase kebab-case, at most 30 chars, hyphen-separated, no slashes\"}}.\
         \n\nTask:\n{task}"
    );

    // Reuse the executor's pinned claude CLI (`npx -y @anthropic-ai/claude-code@X`),
    // headless with no MCP servers (`--strict-mcp-config`) and from a neutral cwd
    // so no project context loads — that keeps the call ~3-5s and deterministic.
    let base = base_command(false);
    let mut parts = base.split_whitespace();
    let program = parts.next()?;
    let lead_args: Vec<&str> = parts.collect();

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&lead_args)
        .arg("-p")
        .arg(&prompt)
        .arg("--model")
        .arg("haiku")
        .arg("--strict-mcp-config")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let output = tokio::time::timeout(CALL_TIMEOUT, cmd.output())
        .await
        .ok()? // timed out
        .ok()?; // spawn / IO error
    if !output.status.success() {
        return None;
    }
    parse_workspace_names(&String::from_utf8_lossy(&output.stdout))
}

/// Extract `{title, branch}` from claude's stdout (which may wrap the JSON in a
/// markdown code fence or stray prose) and normalize into a title + branch slug.
/// Pure, so it is unit-testable without spawning anything.
fn parse_workspace_names(stdout: &str) -> Option<WorkspaceNames> {
    let start = stdout.find('{')?;
    let end = stdout.rfind('}')?;
    if end <= start {
        return None;
    }
    let raw: RawNames = serde_json::from_str(&stdout[start..=end]).ok()?;

    let title: String = raw
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect::<String>()
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }

    // Prefer the model's branch; fall back to slugifying the title. The container
    // re-applies its own git-safe slug, but we hand it a clean value either way.
    let seed = if raw.branch.trim().is_empty() {
        title.as_str()
    } else {
        raw.branch.as_str()
    };
    let branch_slug = slugify(seed, BRANCH_MAX_CHARS);
    if branch_slug.is_empty() {
        return None;
    }

    Some(WorkspaceNames { title, branch_slug })
}

/// Lowercase ASCII slug: alphanumerics kept, every other run collapsed to a
/// single hyphen, trimmed, capped at `max` chars (trailing hyphen trimmed again).
fn slugify(input: &str, max: usize) -> String {
    let mut slug = String::new();
    for ch in input.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-')
        .chars()
        .take(max)
        .collect::<String>()
        .trim_end_matches('-')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{parse_workspace_names, slugify};

    #[test]
    fn parses_fenced_json() {
        let s = "```json\n{\"title\":\"Add CSV export to reports\",\"branch\":\"add-csv-export-reports\"}\n```";
        let n = parse_workspace_names(s).unwrap();
        assert_eq!(n.title, "Add CSV export to reports");
        assert_eq!(n.branch_slug, "add-csv-export-reports");
    }

    #[test]
    fn sanitizes_slashes_and_caps_length() {
        let s = "{\"title\":\"Fix login redirect loop on Safari\",\"branch\":\"fix/safari-login-redirect-loop-and-more-tail\"}";
        let n = parse_workspace_names(s).unwrap();
        assert!(!n.branch_slug.contains('/'));
        assert!(n.branch_slug.chars().count() <= 40);
        assert!(!n.branch_slug.ends_with('-'));
    }

    #[test]
    fn falls_back_to_title_when_branch_missing() {
        let s = "{\"title\":\"Improve onboarding\",\"branch\":\"\"}";
        assert_eq!(
            parse_workspace_names(s).unwrap().branch_slug,
            "improve-onboarding"
        );
    }

    #[test]
    fn collapses_title_whitespace() {
        let s = "{\"title\":\"  Add   export  \",\"branch\":\"add-export\"}";
        assert_eq!(parse_workspace_names(s).unwrap().title, "Add export");
    }

    #[test]
    fn rejects_non_json_and_empty_title() {
        assert!(parse_workspace_names("sorry, I can't help with that").is_none());
        assert!(parse_workspace_names("{\"title\":\"   \",\"branch\":\"x\"}").is_none());
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Add CSV Export!!", 40), "add-csv-export");
        assert_eq!(slugify("feature/Foo Bar", 40), "feature-foo-bar");
        assert_eq!(slugify("a".repeat(50).as_str(), 8), "aaaaaaaa");
        assert_eq!(slugify("---", 40), "");
    }
}

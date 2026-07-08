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

const TITLE_MAX_CHARS: usize = 48;
const BRANCH_MAX_CHARS: usize = 40;
const PROMPT_MAX_CHARS: usize = 2000;
// claude's startup (loading the user's possibly-large ~/.claude.json) can take
// 10-15s+, so allow generous headroom — measured calls were 5-14s. A genuinely
// stuck call still falls back to heuristic naming.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Arrow first lines get extra slack over the 48-char short-line bound
/// (matching the frontend's 100-char first-line name cap), but an arbitrarily
/// long arrow line — e.g. a pasted log line containing " -> " — is a task, not
/// a title, and must not be stored verbatim as the workspace name.
const HINT_ARROW_MAX_CHARS: usize = 100;

/// If the user's first line already reads like a deliberate title, return it
/// (lightly trimmed) so we keep it verbatim instead of asking the model.
///
/// Signals: contains " -> " (Miguel's explicit format) at a plausible title
/// length, OR is short (<= 8 words AND <= 48 chars) AND the message has a body
/// after it (a lone short message is a task, not a title hint — still
/// model-styled) unless it contains an arrow. A trailing clause punctuation
/// (":", ",", "...") rejects, since it signals the first line is the start of
/// a sentence, not a title.
pub fn first_line_title_hint(message: &str) -> Option<String> {
    let mut lines = message.trim().lines();
    let first = lines.next()?.trim().trim_start_matches('#').trim();
    if first.is_empty() {
        return None;
    }
    let has_arrow = (first.contains(" -> ") || first.contains(" → "))
        && first.chars().count() <= HINT_ARROW_MAX_CHARS;
    let has_body = lines.any(|l| !l.trim().is_empty());
    let word_count = first.split_whitespace().count();
    let short_enough = word_count <= 8 && first.chars().count() <= 48;
    if !(has_arrow || (short_enough && has_body)) {
        return None;
    }
    if first.ends_with(':') || first.ends_with(',') || first.ends_with("...") {
        return None;
    }
    Some(
        first
            .trim_end_matches('.')
            .replace(" → ", " -> ")
            .to_string(),
    )
}

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
        "You name coding-agent workspaces. Reply with ONLY a minified JSON object, \
         no prose and no code fence: {{\"title\":\"...\",\"branch\":\"...\"}}.\n\
         \n\
         Title rules:\n\
         - Format: '<keyword> -> <gist>' where keyword is the product, repo, service, \
         or person the task is about (infer it from the task text: repo names, \
         project codenames, people, tools), and gist is 2-5 words.\n\
         - Hard cap: 40 characters total.\n\
         - Lowercase everything except proper nouns and code identifiers.\n\
         - Terse noun fragments, never sentences. No trailing period. \
         Never start with 'Fix bug where', 'Implement', 'Update the', or similar prose.\n\
         - If the task's FIRST LINE already looks like a title (8 words or fewer, \
         not a full sentence, no trailing verb clause), reuse that line as the title \
         verbatim (only trim trailing punctuation) instead of inventing one.\n\
         \n\
         Good titles:\n\
         - bp -> runflow dogfood\n\
         - sentinel -> throughput fixes\n\
         - bp -> customer.io migration\n\
         - patri -> main chief\n\
         \n\
         Bad titles (never do this):\n\
         - Fix bug where b2b companies cannot start an order\n\
         - Implement CSV export for the reports page\n\
         \n\
         Branch rules: lowercase kebab-case, at most 30 chars, hyphens only, \
         no slashes, no arrows — a descriptive slug of the task \
         (e.g. 'bp-customerio-migration', 'sentinel-throughput-fixes').\n\
         \n\
         Task:\n{task}"
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
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Each branch logs WHY it fell back — the original silent `?` chain made a
    // timed-out/erroring call indistinguishable from "model produced no title".
    let output = match tokio::time::timeout(CALL_TIMEOUT, cmd.output()).await {
        Err(_) => {
            tracing::warn!(
                "workspace title generation timed out after {}s; using heuristic naming",
                CALL_TIMEOUT.as_secs()
            );
            return None;
        }
        Ok(Err(e)) => {
            tracing::warn!("workspace title generation could not spawn claude: {e}");
            return None;
        }
        Ok(Ok(output)) => output,
    };
    if !output.status.success() {
        tracing::warn!(
            "workspace title generation exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    let names = parse_workspace_names(&String::from_utf8_lossy(&output.stdout));
    if names.is_none() {
        tracing::warn!(
            "workspace title generation returned unparseable output: {}",
            String::from_utf8_lossy(&output.stdout).trim()
        );
    }
    names
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
        .replace(" → ", " -> ")
        .chars()
        .take(TITLE_MAX_CHARS)
        .collect::<String>()
        // A mid-word cut (or a model-emitted period) must never leave dangling
        // punctuation/space at the tail.
        .trim()
        .trim_end_matches(['.', ',', ':', ';', ' '])
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
    use super::{TITLE_MAX_CHARS, first_line_title_hint, parse_workspace_names, slugify};

    #[test]
    fn parses_fenced_json() {
        let s = "```json\n{\"title\":\"Add CSV export to reports\",\"branch\":\"add-csv-export-reports\"}\n```";
        let n = parse_workspace_names(s).unwrap();
        assert_eq!(n.title, "Add CSV export to reports");
        assert_eq!(n.branch_slug, "add-csv-export-reports");
    }

    #[test]
    fn hint_arrow_wins_even_without_body() {
        assert_eq!(
            first_line_title_hint("patri -> main chief").as_deref(),
            Some("patri -> main chief")
        );
        assert_eq!(
            first_line_title_hint("bp -> runflow dogfood\n\ndetails here").as_deref(),
            Some("bp -> runflow dogfood")
        );
    }

    #[test]
    fn hint_normalizes_unicode_arrow_and_trims_period() {
        assert_eq!(
            first_line_title_hint("bp → customer.io migration.").as_deref(),
            Some("bp -> customer.io migration")
        );
    }

    #[test]
    fn hint_short_first_line_with_body_wins() {
        assert_eq!(
            first_line_title_hint("Fix mobile scroll\n\nthe terminal jumps").as_deref(),
            Some("Fix mobile scroll")
        );
    }

    #[test]
    fn hint_strips_leading_markdown_heading() {
        assert_eq!(
            first_line_title_hint("# bp -> runflow dogfood").as_deref(),
            Some("bp -> runflow dogfood")
        );
    }

    #[test]
    fn hint_rejects_long_sentence_and_lone_short_line() {
        // 9 words, no body, no arrow -> not a title hint.
        assert!(
            first_line_title_hint("Fix bug where b2b companies cannot start an order").is_none()
        );
        // Short but no body and no arrow -> a task, not a title.
        assert!(first_line_title_hint("Fix mobile scroll").is_none());
    }

    #[test]
    fn hint_rejects_trailing_clause_punctuation() {
        assert!(first_line_title_hint("Please do the following:\n\n- a\n- b").is_none());
        assert!(first_line_title_hint("add this, and that,\n\nbody").is_none());
        assert!(first_line_title_hint("Do these things...\n\nbody").is_none());
    }

    #[test]
    fn hint_rejects_empty() {
        assert!(first_line_title_hint("").is_none());
        assert!(first_line_title_hint("\n\n").is_none());
    }

    #[test]
    fn hint_rejects_overlong_arrow_line() {
        // A pasted log line containing " -> " is a task, not a title — it must
        // fall through to the model path instead of being stored verbatim.
        let long_arrow = format!("request -> response {}", "x".repeat(120));
        assert!(first_line_title_hint(&long_arrow).is_none());
        assert!(first_line_title_hint(&format!("{long_arrow}\n\nbody")).is_none());
        // At the boundary (100 chars) the arrow line is still accepted.
        let at_cap = format!("bp -> {}", "y".repeat(94)); // 6 + 94 = 100 chars
        assert_eq!(at_cap.chars().count(), 100);
        assert_eq!(
            first_line_title_hint(&at_cap).as_deref(),
            Some(at_cap.as_str())
        );
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
    fn keeps_arrow_in_title_and_slugifies_branch() {
        let s =
            "{\"title\":\"bp -> customer.io migration\",\"branch\":\"bp-customerio-migration\"}";
        let n = parse_workspace_names(s).unwrap();
        assert_eq!(n.title, "bp -> customer.io migration");
        assert!(!n.branch_slug.contains('>'));
        assert!(!n.branch_slug.contains(' '));
    }

    #[test]
    fn normalizes_unicode_arrow_in_title() {
        let s = "{\"title\":\"bp → runflow dogfood\",\"branch\":\"bp-runflow-dogfood\"}";
        assert_eq!(
            parse_workspace_names(s).unwrap().title,
            "bp -> runflow dogfood"
        );
    }

    #[test]
    fn caps_title_at_max_and_trims_trailing_punctuation() {
        let long = "a".repeat(60);
        let s = format!("{{\"title\":\"{long}\",\"branch\":\"x\"}}");
        let title = parse_workspace_names(&s).unwrap().title;
        assert!(title.chars().count() <= TITLE_MAX_CHARS);
    }

    #[test]
    fn strips_trailing_period_from_title() {
        let s = "{\"title\":\"sentinel -> throughput fixes.\",\"branch\":\"x\"}";
        assert_eq!(
            parse_workspace_names(s).unwrap().title,
            "sentinel -> throughput fixes"
        );
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
        // An arrow title degrades to a clean branch slug (no '>', no spaces).
        assert_eq!(
            slugify("bp -> customer.io migration", 40),
            "bp-customer-io-migration"
        );
        assert_eq!(slugify("bp -> runflow dogfood", 40), "bp-runflow-dogfood");
    }
}

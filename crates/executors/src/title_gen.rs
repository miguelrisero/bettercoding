//! Best-effort workspace title + branch-slug generation from the first message.
//!
//! Runs a fast Claude Haiku call (reusing the same pinned claude CLI the
//! executor uses) to turn a workspace's first message into a concise human
//! title and a git branch slug — a smarter replacement for slugifying the first
//! few words. It is bounded by a short timeout and returns `None` on any
//! failure, so callers always fall back to heuristic naming and workspace
//! creation is never blocked for long or broken by a slow/absent model.

use std::{
    io,
    process::{Output, Stdio},
    time::Duration,
};

#[cfg(unix)]
use command_group::{AsyncCommandGroup, AsyncGroupChild};
use serde::Deserialize;
#[cfg(unix)]
use tokio::io::{AsyncRead, AsyncReadExt};

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

/// Rewrite every unicode arrow (" → ") to the ASCII form (" -> ").
/// `String::replace` alone is non-overlapping ("a → → b" would keep its second
/// arrow because the shared space is consumed by the first match), so loop
/// until stable; each pass strictly reduces the '→' count, so it terminates.
fn normalize_arrows(s: &str) -> String {
    let mut out = s.to_string();
    while out.contains(" → ") {
        out = out.replace(" → ", " -> ");
    }
    out
}

/// Normalize a candidate title into the stored form shared by the hint and
/// model paths: collapse whitespace, normalize the unicode arrow, cap at
/// `max_chars`, and trim trailing sentence punctuation — a mid-word cut (or a
/// model-emitted period) must never leave dangling punctuation, whitespace, or
/// an arrow fragment at the tail. Length *policy* stays per-path (the hint
/// path rejects overlong arrow lines up front; the model path caps at
/// [`TITLE_MAX_CHARS`]).
fn canonicalize_title(raw: &str, max_chars: usize) -> String {
    let joined = normalize_arrows(&raw.split_whitespace().collect::<Vec<_>>().join(" "));
    let capped: String = joined.chars().take(max_chars).collect();
    // A cap cut inside " -> " (or degenerate input like "a -> ->") leaves
    // dangling arrow fragments, possibly shielding more trailing punctuation
    // ("word. ->"). Trim punctuation and arrow fragments to a fixpoint — each
    // pass strictly shortens the string, so the loop terminates.
    let mut tail = capped.as_str();
    loop {
        let before = tail;
        tail = tail.trim_end_matches(['.', ',', ':', ';', ' ']);
        tail = tail
            .strip_suffix(" ->")
            .or_else(|| tail.strip_suffix(" -"))
            .unwrap_or(tail);
        if tail.len() == before.len() {
            break;
        }
    }
    tail.to_string()
}

/// If the user's first line already reads like a deliberate title, return it
/// (lightly normalized) so we keep it verbatim instead of asking the model.
///
/// Signals: contains " -> " (Miguel's explicit format) at a plausible title
/// length, OR is short (<= 8 words AND <= 48 chars) AND the message has a body
/// after it (a lone short message is a task, not a title hint — still
/// model-styled) unless it contains an arrow. A trailing clause punctuation
/// (":", ",", "..." or the unicode "…") rejects, since it signals the first
/// line is the start of a sentence, not a title.
pub fn first_line_title_hint(message: &str) -> Option<String> {
    let mut lines = message.trim().lines();
    let raw_first = lines.next()?.trim();
    // Strip a Markdown ATX heading marker (1-6 leading '#' followed by
    // whitespace or end of line, per CommonMark); keep issue-ref-style
    // prefixes like "#27 -> ..." and 7+-hash runs intact — those '#' are
    // content, not markup.
    let hashes = raw_first.len() - raw_first.trim_start_matches('#').len();
    let first = if (1..=6).contains(&hashes) {
        if hashes == raw_first.len() {
            ""
        } else if raw_first[hashes..].starts_with(char::is_whitespace) {
            raw_first[hashes..].trim_start()
        } else {
            raw_first
        }
    } else {
        raw_first
    };
    if first.is_empty() {
        return None;
    }
    // Collapse whitespace and normalize unicode arrows BEFORE measuring, so
    // acceptance bounds exactly the form that gets stored (runs of spaces
    // shrink; " → " grows into " -> ").
    let first = normalize_arrows(&first.split_whitespace().collect::<Vec<_>>().join(" "));
    let char_count = first.chars().count();
    let has_arrow = first.contains(" -> ") && char_count <= HINT_ARROW_MAX_CHARS;
    let has_body = lines.any(|l| !l.trim().is_empty());
    let word_count = first.split_whitespace().count();
    let short_enough = word_count <= 8 && char_count <= 48;
    if !(has_arrow || (short_enough && has_body)) {
        return None;
    }
    if first.ends_with(':')
        || first.ends_with(',')
        || first.ends_with("...")
        || first.ends_with('…')
    {
        return None;
    }
    // Acceptance already bounds the line, so the cap here is a no-op backstop;
    // canonicalization can strip a line down to nothing (e.g. "."), so
    // re-check emptiness before declaring it a title.
    let title = canonicalize_title(&first, HINT_ARROW_MAX_CHARS);
    (!title.is_empty()).then_some(title)
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
    let mut args: Vec<String> = parts.map(str::to_owned).collect();
    args.extend([
        "-p".to_string(),
        prompt,
        "--model".to_string(),
        "haiku".to_string(),
        "--strict-mcp-config".to_string(),
    ]);

    generate_workspace_names_with_command(program, &args, CALL_TIMEOUT).await
}

/// Run and parse a title command. Keeping the program, arguments, and timeout
/// injectable makes process lifecycle failures testable without invoking npx or
/// a model.
async fn generate_workspace_names_with_command(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Option<WorkspaceNames> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Each branch logs WHY it fell back — the original silent `?` chain made a
    // timed-out/erroring call indistinguishable from "model produced no title".
    let output = match command_output_with_timeout(&mut cmd, timeout).await {
        Ok(None) => {
            tracing::warn!(
                "workspace title generation timed out after {}s; using heuristic naming",
                timeout.as_secs()
            );
            return None;
        }
        Err(e) => {
            tracing::warn!("workspace title generation could not spawn claude: {e}");
            return None;
        }
        Ok(Some(output)) => output,
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

#[cfg(unix)]
async fn command_output_with_timeout(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> io::Result<Option<Output>> {
    let mut child = cmd.group_spawn()?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("title command stdout was not piped"))?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("title command stderr was not piped"))?;

    let collect_output = async {
        let (status, stdout, stderr) = tokio::try_join!(
            wait_for_group_exit(&mut child),
            read_to_end(stdout),
            read_to_end(stderr),
        )?;
        Ok::<Output, io::Error>(Output {
            status,
            stdout,
            stderr,
        })
    };

    match tokio::time::timeout(timeout, collect_output).await {
        Ok(output) => output.map(Some),
        Err(_) => {
            kill_and_reap_group(&mut child).await;
            Ok(None)
        }
    }
}

#[cfg(not(unix))]
async fn command_output_with_timeout(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> io::Result<Option<Output>> {
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(output) => output.map(Some),
        Err(_) => Ok(None),
    }
}

#[cfg(unix)]
async fn wait_for_group_exit(child: &mut AsyncGroupChild) -> io::Result<std::process::ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
async fn read_to_end(mut reader: impl AsyncRead + Unpin) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(unix)]
async fn kill_and_reap_group(child: &mut AsyncGroupChild) {
    // command-group retains the PGID captured at spawn, so this SIGKILL reaches
    // descendants even if the npx group leader has already exited.
    if let Err(error) = child.start_kill() {
        tracing::warn!("failed to SIGKILL timed-out title process group: {error}");
    }
    if let Err(error) = child.wait().await {
        tracing::warn!("failed to reap timed-out title process group: {error}");
    }
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

    let title = canonicalize_title(&raw.title, TITLE_MAX_CHARS);
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
    use std::time::Duration;

    use super::{
        TITLE_MAX_CHARS, first_line_title_hint, generate_workspace_names_with_command,
        parse_workspace_names, slugify,
    };

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_background_grandchild() {
        let temp_dir = tempfile::tempdir().unwrap();
        let progress_path = temp_dir.path().join("grandchild-progress");
        let script = r#"
            while :; do
                printf x >> "$1"
                sleep 0.02
            done &
            wait
        "#;
        let args = vec![
            "-c".to_string(),
            script.to_string(),
            "title-gen-timeout-test".to_string(),
            progress_path.to_string_lossy().into_owned(),
        ];

        let names =
            generate_workspace_names_with_command("/bin/sh", &args, Duration::from_millis(150))
                .await;

        assert!(names.is_none());
        let size_after_return = std::fs::metadata(&progress_path).unwrap().len();
        assert!(size_after_return > 0, "grandchild never wrote progress");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            std::fs::metadata(&progress_path).unwrap().len(),
            size_after_return,
            "grandchild kept running after the timeout returned"
        );
    }

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
        assert_eq!(
            first_line_title_hint("### bp -> runflow dogfood").as_deref(),
            Some("bp -> runflow dogfood")
        );
        // A bare hash run is a heading marker with no content.
        assert!(first_line_title_hint("###\n\nbody").is_none());
        // 7+ hashes are not an ATX heading (CommonMark) — kept as content.
        assert_eq!(
            first_line_title_hint("####### bp -> runflow dogfood").as_deref(),
            Some("####### bp -> runflow dogfood")
        );
    }

    #[test]
    fn hint_measures_length_on_collapsed_whitespace() {
        // Interior space runs collapse before the length check, so a line
        // whose RAW form exceeds the arrow cap but whose stored form is short
        // is still accepted.
        let padded = format!("bp{}->{}dogfood", " ".repeat(50), " ".repeat(50));
        assert!(padded.chars().count() > 100);
        assert_eq!(
            first_line_title_hint(&padded).as_deref(),
            Some("bp -> dogfood")
        );
    }

    #[test]
    fn hint_keeps_issue_ref_hash_prefix() {
        // '#' not followed by whitespace is content (issue ref), not markup.
        assert_eq!(
            first_line_title_hint("#27 -> short arrow titles").as_deref(),
            Some("#27 -> short arrow titles")
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
        // Unicode ellipsis (smart-punctuation form of "...") rejects too.
        assert!(first_line_title_hint("Do these things…\n\nbody").is_none());
    }

    #[test]
    fn hint_rejects_empty() {
        assert!(first_line_title_hint("").is_none());
        assert!(first_line_title_hint("\n\n").is_none());
        // Canonicalization can strip a line to nothing — never store "".
        assert!(first_line_title_hint(".\n\nbody").is_none());
    }

    #[test]
    fn hint_normalizes_like_the_model_path() {
        // Whitespace collapse and trailing-punctuation trim match the stored
        // form the Haiku path produces for the same string.
        assert_eq!(
            first_line_title_hint("bp   ->  runflow dogfood\n\nbody").as_deref(),
            Some("bp -> runflow dogfood")
        );
        assert_eq!(
            first_line_title_hint("Fix scroll bug;\n\nbody").as_deref(),
            Some("Fix scroll bug")
        );
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
        // The bound is measured AFTER unicode-arrow normalization (" → " grows
        // into " -> "), so a 100-char unicode-arrow line that would exceed the
        // cap once normalized is rejected, never silently truncated.
        let unicode_at_raw_cap = format!("bp → {}", "y".repeat(95)); // 100 raw, 101 normalized
        assert_eq!(unicode_at_raw_cap.chars().count(), 100);
        assert!(first_line_title_hint(&unicode_at_raw_cap).is_none());
        let unicode_fits = format!("bp → {}", "y".repeat(94)); // 100 once normalized
        assert_eq!(
            first_line_title_hint(&unicode_fits).as_deref(),
            Some(format!("bp -> {}", "y".repeat(94)).as_str())
        );
    }

    #[test]
    fn hint_normalizes_adjacent_unicode_arrows() {
        // String::replace is non-overlapping; the stable loop converts both.
        assert_eq!(
            first_line_title_hint("a → → b\n\nbody").as_deref(),
            Some("a -> -> b")
        );
    }

    #[test]
    fn hint_keeps_cjk_title_verbatim() {
        // A non-ASCII hint is a fine NAME; the un-sluggable branch seed is
        // guarded at the rename site (empty slug -> heuristic branch kept).
        assert_eq!(
            first_line_title_hint("修复登录\n\n详情").as_deref(),
            Some("修复登录")
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
    fn cap_cut_never_leaves_dangling_arrow_fragment() {
        // 45-char keyword + " -> gist" = 53 chars; the 48-char cap cuts inside
        // the arrow, which must be stripped rather than stored as "... ->".
        let keyword = "a".repeat(45);
        let s = format!("{{\"title\":\"{keyword} -> gist\",\"branch\":\"x\"}}");
        let title = parse_workspace_names(&s).unwrap().title;
        assert_eq!(title, keyword);
        // One char shorter cut ends in " -" — also stripped.
        let keyword = "a".repeat(46);
        let s = format!("{{\"title\":\"{keyword} -> gist\",\"branch\":\"x\"}}");
        let title = parse_workspace_names(&s).unwrap().title;
        assert_eq!(title, keyword);
        // Chained fragments trim to a fixpoint, not just one pass.
        let s = "{\"title\":\"a -> -> \",\"branch\":\"x\"}";
        assert_eq!(parse_workspace_names(s).unwrap().title, "a");
        let s = "{\"title\":\"fix -> a - -\",\"branch\":\"x\"}";
        assert_eq!(parse_workspace_names(s).unwrap().title, "fix -> a");
        // A 43-char keyword + \" -> -> b\" cap-cut at 48 chains two fragments.
        let keyword = "a".repeat(43);
        let s = format!("{{\"title\":\"{keyword} -> -> b\",\"branch\":\"x\"}}");
        assert_eq!(parse_workspace_names(&s).unwrap().title, keyword);
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

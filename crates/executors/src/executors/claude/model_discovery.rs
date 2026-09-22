//! Read the model aliases out of the Claude Code CLI that this executor will
//! actually launch, instead of shipping a list that goes stale.
//!
//! The picker's models used to be a hardcoded array in `default_discovered_options`.
//! It drifted in both directions, because the alias set is a property of the CLI
//! *version*:
//!
//! ```text
//! 2.1.152  sonnet opus haiku best sonnet[1m] opus[1m] opusplan
//! 2.1.270  + fable fable[1m]
//! ```
//!
//! so the old list simultaneously offered `fable` to a CLI that rejects it and
//! hid `best` / `opusplan` / `sonnet[1m]` from one that accepts them. Effort
//! tiers already avoid this by deriving from `ClaudeEffort::VARIANTS`; this is
//! the same idea for models, with the CLI binary as the source of truth.
//!
//! The default base command tracks `@latest`, so that version changes underneath
//! us with no code change at all — which makes a hardcoded list not merely
//! stale-prone but impossible to keep correct. Discovery is what makes `@latest`
//! safe to point at.
//!
//! The CLI ships as a single compiled bundle (npm's `bin/claude.exe`, or the
//! native installer's `~/.local/share/claude/versions/<v>`), and the alias array
//! survives minification verbatim as a JSON array literal. We scan for it. That
//! is a heuristic on someone else's build output, so every failure path returns
//! `None` and the caller keeps the static fallback — a stale picker is a much
//! better outcome than an empty one.

use std::{
    fs::File,
    io::{BufReader, Read},
    path::{Path, PathBuf},
};

/// Stable prefix of the alias array. Both the full list and a shorter
/// base-alias array (`["sonnet","opus","haiku","fable"]`) start this way, so
/// matches are collected and the longest wins — the full list is a superset.
const ANCHOR: &[u8] = b"[\"sonnet\",\"opus\",\"haiku\"";

/// Upper bound on the array literal. Comfortably over the ~90 bytes observed;
/// bounds the forward scan so a stray anchor can't walk the whole bundle.
const MAX_ARRAY_BYTES: usize = 512;

/// Read granularity. The bundle is ~240 MB, so it is streamed rather than read
/// into memory, with an overlap so a match spanning a chunk boundary is found.
const CHUNK_BYTES: usize = 1 << 20;

/// Longest alias we will accept, to reject a bogus parse.
const MAX_ALIAS_LEN: usize = 32;

/// Extract the CLI's model aliases from its bundle.
///
/// Returns `None` when the file is unreadable or no plausible array is found.
pub(crate) fn extract_model_aliases(bundle: &Path) -> Option<Vec<String>> {
    let file = File::open(bundle).ok()?;
    let mut reader = BufReader::new(file);

    // Keep the tail of each chunk so an array straddling the boundary is still
    // anchored and fully readable in the next pass.
    let overlap = ANCHOR.len() + MAX_ARRAY_BYTES;
    let mut buf: Vec<u8> = Vec::with_capacity(CHUNK_BYTES + overlap);
    let mut chunk = vec![0u8; CHUNK_BYTES];
    let mut best: Option<Vec<String>> = None;

    loop {
        let n = reader.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);

        for found in parse_all(&buf) {
            if best.as_ref().is_none_or(|b| found.len() > b.len()) {
                best = Some(found);
            }
        }

        if buf.len() > overlap {
            buf.drain(..buf.len() - overlap);
        }
    }

    best
}

/// Every valid alias array anchored in `haystack`.
fn parse_all(haystack: &[u8]) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut from = 0usize;

    while let Some(rel) = find(&haystack[from..], ANCHOR) {
        let start = from + rel;
        if let Some(aliases) = parse_at(haystack, start) {
            out.push(aliases);
        }
        from = start + 1;
    }

    out
}

/// Parse the array literal beginning at `start` (which points at its `[`).
fn parse_at(haystack: &[u8], start: usize) -> Option<Vec<String>> {
    let limit = (start + MAX_ARRAY_BYTES).min(haystack.len());
    let end = closing_bracket(&haystack[start..limit])?;
    let literal = std::str::from_utf8(&haystack[start..=start + end]).ok()?;
    let aliases: Vec<String> = serde_json::from_str(literal).ok()?;

    if aliases.is_empty() || !aliases.iter().all(|a| is_plausible_alias(a)) {
        return None;
    }
    Some(aliases)
}

/// Offset of the `]` that closes the array starting at `slice[0]`.
///
/// Must ignore brackets inside strings: the 1M-context aliases are spelled
/// `"opus[1m]"`, so the first `]` in the literal belongs to an element, not to
/// the array. Scanning for it naively truncates the parse to the elements
/// before the first such alias.
fn closing_bracket(slice: &[u8]) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;

    for (offset, &byte) in slice.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match byte {
            b'\\' if in_string => escaped = true,
            b'"' => in_string = !in_string,
            b']' if !in_string => return Some(offset),
            _ => {}
        }
    }
    None
}

/// Guard against a coincidental JSON array of unrelated strings. Real aliases
/// are lowercase alphanumerics with an optional `[1m]` context suffix.
fn is_plausible_alias(alias: &str) -> bool {
    let base = alias.strip_suffix("[1m]").unwrap_or(alias);
    !base.is_empty()
        && base.len() <= MAX_ALIAS_LEN
        && base
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Locate the CLI bundle that `base_command` will run.
///
/// Resolution has to follow the configured command, not just `claude` on PATH:
/// the default is a version-pinned `npx`, and a user can point
/// `base_command_override` anywhere. A miss returns `None` (static fallback).
pub(crate) fn resolve_cli_bundle(base_command: &str) -> Option<PathBuf> {
    if let Some(spec) = npm_version_spec(base_command) {
        return npx_cached_bundle(&spec);
    }
    let program = base_command.split_whitespace().next()?;
    resolve_program(program)
}

/// `npx -y @anthropic-ai/claude-code@<spec> ...` -> `<spec>`.
///
/// The spec is an exact version (`2.1.267`) or a dist-tag (`latest`, `next`).
fn npm_version_spec(base_command: &str) -> Option<String> {
    const PKG: &str = "@anthropic-ai/claude-code@";
    let rest = base_command.split(PKG).nth(1)?;
    let spec: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!spec.is_empty()).then_some(spec)
}

/// An exact version, as opposed to a dist-tag or range. Versions start with a
/// digit; `latest`, `next` and `^2.1` do not.
fn is_exact_version(spec: &str) -> bool {
    spec.starts_with(|c: char| c.is_ascii_digit())
}

/// Find the bundle for a version spec in npx's package cache.
///
/// npx keys its cache directories by an opaque hash, so versions are read back
/// out of each candidate's `package.json` rather than derived.
///
/// An exact spec matches that version. A dist-tag cannot be matched this way —
/// nothing on disk records that a directory was installed as `latest`, and the
/// tag moves — so the most recently written entry wins: npx refreshes the cache
/// when it resolves a tag, making mtime the best available proxy for "what the
/// next launch will use". A newly published version therefore shows up in the
/// picker on the run after npx picks it up, not before.
///
/// Returns `None` before the first `npx` run has populated the cache; discovery
/// falls back until then, and the result is cached once it succeeds.
fn npx_cached_bundle(spec: &str) -> Option<PathBuf> {
    let root = home_dir()?.join(".npm/_npx");
    let exact = is_exact_version(spec);
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let pkg = entry.path().join("node_modules/@anthropic-ai/claude-code");
        if !pkg.is_dir() {
            continue;
        }
        let manifest = pkg.join("package.json");
        let Some(version) = package_version(&manifest) else {
            continue;
        };
        if exact && version != spec {
            continue;
        }
        let Some(bin) = bundle_in_package(&pkg) else {
            continue;
        };
        if exact {
            return Some(bin);
        }
        let Ok(mtime) = manifest.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(best, _)| mtime > *best) {
            newest = Some((mtime, bin));
        }
    }

    newest.map(|(_, bin)| bin)
}

fn package_version(manifest: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(manifest).ok()?;
    let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
    json.get("version")?.as_str().map(str::to_string)
}

/// The npm package's executable. `bin.claude` is authoritative; the literal
/// names are a fallback for a manifest shape we don't expect.
fn bundle_in_package(pkg: &Path) -> Option<PathBuf> {
    let manifest = std::fs::read_to_string(pkg.join("package.json")).ok()?;
    let json: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    if let Some(rel) = json
        .get("bin")
        .and_then(|b| b.get("claude"))
        .and_then(|b| b.as_str())
    {
        let path = pkg.join(rel);
        if path.is_file() {
            return Some(path);
        }
    }
    ["bin/claude.exe", "bin/claude"]
        .iter()
        .map(|rel| pkg.join(rel))
        .find(|p| p.is_file())
}

/// Resolve a bare program name or path to a real file, following symlinks —
/// `~/.local/bin/claude` is a symlink into `share/claude/versions/<v>`.
fn resolve_program(program: &str) -> Option<PathBuf> {
    let direct = Path::new(program);
    if direct.is_absolute() || program.contains('/') {
        return std::fs::canonicalize(direct).ok().filter(|p| p.is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .filter(|candidate| candidate.is_file())
        .find_map(|candidate| std::fs::canonicalize(candidate).ok())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Display name for an alias. Known aliases get the CLI picker's wording;
/// anything new gets a readable title-cased fallback, which is the point — an
/// alias added after this ships still appears, just without a curated label.
pub(crate) fn label_for_alias(alias: &str) -> String {
    let (base, one_m) = match alias.strip_suffix("[1m]") {
        Some(base) => (base, true),
        None => (alias, false),
    };

    let name = match base {
        "sonnet" => "Sonnet".to_string(),
        "opus" => "Opus".to_string(),
        "haiku" => "Haiku".to_string(),
        "fable" => "Fable".to_string(),
        "claude-opus-5-5" => "Opus 5.5".to_string(),
        // Not model names: `best` lets the CLI pick, `opusplan` plans on Opus
        // and executes on Sonnet.
        "best" => "Best available".to_string(),
        "opusplan" => "Opus plan mode".to_string(),
        other => title_case(other),
    };

    if one_m {
        format!("{name} (1M context)")
    } else {
        name
    }
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two real arrays, as they appear in the minified bundles.
    const V_2_1_152: &str =
        r#"["sonnet","opus","haiku","best","sonnet[1m]","opus[1m]","opusplan"]"#;
    const V_2_1_270: &str = r#"["sonnet","opus","haiku","fable","best","sonnet[1m]","opus[1m]","fable[1m]","opusplan"]"#;

    fn parse(s: &str) -> Option<Vec<String>> {
        parse_all(s.as_bytes()).into_iter().max_by_key(Vec::len)
    }

    #[test]
    fn parses_both_shipped_alias_arrays() {
        assert_eq!(
            parse(V_2_1_152).unwrap(),
            [
                "sonnet",
                "opus",
                "haiku",
                "best",
                "sonnet[1m]",
                "opus[1m]",
                "opusplan"
            ]
        );
        assert_eq!(parse(V_2_1_270).unwrap().len(), 9);
    }

    #[test]
    fn picks_the_superset_when_a_base_array_is_also_present() {
        // 2.1.270 carries both; the shorter one must not win.
        let bundle = format!(r#"var wDt=["sonnet","opus","haiku","fable"],OO={V_2_1_270};"#);
        assert_eq!(parse(&bundle).unwrap().len(), 9);
    }

    #[test]
    fn finds_the_array_surrounded_by_minified_noise() {
        let bundle = format!(r#"a{{b:1}},OO={V_2_1_152},wDt=[];function vg(e){{}}"#);
        assert!(parse(&bundle).unwrap().contains(&"opusplan".to_string()));
    }

    #[test]
    fn does_not_stop_at_the_bracket_inside_a_1m_alias() {
        // Regression: `"sonnet[1m]"` puts a `]` inside a string, so a scan for
        // the first `]` truncated the array to the elements before it.
        let aliases = parse(V_2_1_270).unwrap();
        assert!(aliases.contains(&"sonnet[1m]".to_string()));
        assert_eq!(aliases.last().unwrap(), "opusplan");
    }

    #[test]
    fn rejects_unterminated_and_non_alias_arrays() {
        assert!(parse(r#"["sonnet","opus","haiku","#).is_none());
        assert!(parse(r#"["sonnet","opus","haiku","Not An Alias"]"#).is_none());
        assert!(parse(r#"["sonnet","opus","haiku","a/../../etc/passwd"]"#).is_none());
    }

    #[test]
    fn extracts_across_a_chunk_boundary() {
        use std::io::Write;
        // Straddle the 1 MiB read boundary: the array starts a few bytes before
        // it and ends after, which is the case a naive chunked scan misses.
        let mut blob = vec![b'x'; CHUNK_BYTES - 10];
        blob.extend_from_slice(V_2_1_270.as_bytes());
        blob.extend_from_slice(&vec![b'y'; 4096]);

        let dir = std::env::temp_dir().join(format!("bc-alias-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bundle.bin");
        File::create(&path).unwrap().write_all(&blob).unwrap();

        let aliases = extract_model_aliases(&path).unwrap();
        assert_eq!(aliases.len(), 9);
        assert!(aliases.contains(&"fable[1m]".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_bundle_is_none_not_an_error() {
        assert!(extract_model_aliases(Path::new("/nonexistent/claude")).is_none());
    }

    #[test]
    fn reads_the_version_spec_out_of_the_base_command() {
        // The shipped default: a dist-tag, not a version.
        assert_eq!(
            npm_version_spec("npx -y @anthropic-ai/claude-code@latest").as_deref(),
            Some("latest")
        );
        assert_eq!(
            npm_version_spec("npx -y @anthropic-ai/claude-code@2.1.267 --foo").as_deref(),
            Some("2.1.267")
        );
        assert!(npm_version_spec("claude").is_none());
        // The router wrapper is a different package and must not be mistaken
        // for Claude Code.
        assert!(npm_version_spec("npx -y @musistudio/claude-code-router@1.0.66 code").is_none());
    }

    #[test]
    fn separates_exact_versions_from_dist_tags() {
        // Exact specs select a matching cache entry; everything else falls back
        // to "most recently written", since a tag matches nothing on disk.
        assert!(is_exact_version("2.1.267"));
        assert!(!is_exact_version("latest"));
        assert!(!is_exact_version("next"));
        assert!(!is_exact_version("stable"));
        assert!(!is_exact_version("^2.1.0"));
    }

    /// End-to-end check against a real install, which is the only thing that
    /// exercises the resolver and the scan over an actual 240 MB bundle.
    /// Ignored because it depends on the machine; run it after a CLI bump with
    ///   cargo test -p executors claude::model_discovery -- --ignored --nocapture
    #[test]
    #[ignore = "requires Claude Code installed on this machine"]
    fn extracts_aliases_from_the_installed_cli() {
        let bundle = resolve_program("claude").expect("claude on PATH");
        let aliases = extract_model_aliases(&bundle).expect("aliases in bundle");
        println!("{} -> {aliases:?}", bundle.display());
        assert!(aliases.iter().any(|a| a == "opus"));
        assert!(aliases.iter().any(|a| a == "sonnet"));
    }

    #[test]
    fn labels_known_aliases_and_title_cases_unknown_ones() {
        assert_eq!(label_for_alias("opus"), "Opus");
        assert_eq!(label_for_alias("opus[1m]"), "Opus (1M context)");
        assert_eq!(label_for_alias("best"), "Best available");
        assert_eq!(label_for_alias("opusplan"), "Opus plan mode");
        // The case this exists for: an alias nobody has seen yet.
        assert_eq!(label_for_alias("mythos"), "Mythos");
        assert_eq!(label_for_alias("mythos[1m]"), "Mythos (1M context)");
    }
}

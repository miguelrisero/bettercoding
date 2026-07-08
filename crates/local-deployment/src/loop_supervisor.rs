//! Agentic-loop supervisor — keeps a workspace's CLI agent going when a chat
//! stops for a NON-completion reason.
//!
//! CLI pane output is ephemeral (streamed to the browser, never persisted), so a
//! background poller is the only way to notice that an agent stalled on a usage
//! or rate limit. Each tick this:
//!
//! 1. **Delivers** any due [`ScheduledWakeup`] by typing the continuation prompt
//!    into the live tmux pane (or re-parking it if the session died).
//! 2. **Detects** limit banners in every enabled workspace's pane and schedules
//!    a wake-up: a short retry for a transient rate limit, or a wake at the
//!    parsed reset time for a usage-window limit.
//!
//! Opt-in per workspace (default OFF). Kill-switch: `DISABLE_LOOP_AUTOMATION`.
//! Idle-only re-prompting falls out of the design: a limit-kind wake-up is only
//! delivered while the limit banner is still present, so a manually-resumed
//! agent is never interrupted.

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, Offset, TimeZone, Utc};
use db::{
    DBService,
    models::{
        loop_automation::{LoopAutomation, ScheduledWakeup, WakeupKind},
        session::Session,
    },
};
use uuid::Uuid;

use crate::pty::{
    CliPromptDelivery, capture_cli_pane, cli_tmux_available, cli_tmux_session_exists, send_cli_keys,
};

/// How often to poll panes for limit banners and check for due wake-ups.
const POLL_INTERVAL: Duration = Duration::from_secs(20);

/// Fallback wake delay for a usage-window limit whose reset time couldn't be
/// parsed from the banner — re-detection at wake time reschedules if still
/// limited, so this only bounds how often we re-check during a long window.
const USAGE_BACKOFF_SECS: i64 = 1800; // 30 minutes

/// Wake this long after a usage/session window's stated reset time, so the
/// window has actually rolled before we re-prompt (a "resets 3:10pm" banner is
/// only precise to the minute, and the provider clears the cap slightly late).
const USAGE_POST_RESET_BUFFER_SECS: i64 = 300; // 5 minutes

/// Backoff for an Anthropic "API Error: 529 Overloaded" — a transient
/// server-side error that usually clears within a few minutes; re-detection at
/// wake time reschedules another backoff if it hasn't. Kept inside the
/// provider's suggested 3–5 minute window.
const OVERLOAD_BACKOFF_SECS: i64 = 240; // 4 minutes

/// Env kill-switch (mirrors `DISABLE_CLI_SESSION_REAP`).
const DISABLE_ENV: &str = "DISABLE_LOOP_AUTOMATION";

/// Default continuation prompt when neither the wake-up nor the policy set one.
const DEFAULT_CONTINUATION: &str = "Continue.";

/// What a CLI pane's tail tells us about why the agent stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitSignal {
    /// Transient provider throttling ("temporarily limiting requests · Rate
    /// limited") — retry after a short backoff.
    RateLimited,
    /// Anthropic "API Error: 529 Overloaded" — a transient server-side error;
    /// retry a few minutes out.
    Overloaded,
    /// A usage- or session-window limit ("reached your usage limit", "You've
    /// hit your session limit · resets 3:10pm") — wake at the reset time when we
    /// can parse one.
    UsageLimited { reset_at: Option<DateTime<Utc>> },
}

impl LimitSignal {
    /// The wake-up kind used to schedule (and dedupe) a retry for this signal,
    /// and to check on delivery that the *same* limit is still showing.
    fn wakeup_kind(&self) -> WakeupKind {
        match self {
            LimitSignal::RateLimited => WakeupKind::RateLimitRetry,
            LimitSignal::Overloaded => WakeupKind::OverloadRetry,
            LimitSignal::UsageLimited { .. } => WakeupKind::UsageLimitWake,
        }
    }
}

/// The machine's current UTC offset — CLI-agent banners render reset times in
/// the host's local zone (the supervisor runs on the same host), so this is the
/// default used to interpret a reset clock time that carries no explicit zone.
fn local_offset() -> FixedOffset {
    Local::now().offset().fix()
}

/// Classify a CLI pane's visible content. Rate-limit is checked first because
/// the provider's rate-limit banner explicitly says "(not your usage limit)",
/// which would otherwise trip the usage-limit matcher.
pub fn detect_limit(pane: &str) -> Option<LimitSignal> {
    detect_limit_at(pane, Utc::now(), local_offset())
}

fn detect_limit_at(pane: &str, now: DateTime<Utc>, default_tz: FixedOffset) -> Option<LimitSignal> {
    let lower = pane.to_lowercase();

    // Transient rate limit — the user's exact phrasing: "Server is temporarily
    // limiting requests (not your usage limit) · Rate limited".
    if lower.contains("temporarily limiting requests") || lower.contains("rate limited") {
        return Some(LimitSignal::RateLimited);
    }

    // Usage-/session-window limit (5-hour / weekly / session). Checked before the
    // 529 case: a pane can show a stale "overloaded" line above a current
    // usage/session banner, and the window limit (with its concrete reset time)
    // must win over a transient retry — otherwise we'd hammer a hard-capped agent
    // every few minutes instead of waking once at its reset. "session limit" is
    // narrowed to the actual banner phrasing ("hit your session limit") so it
    // can't match chatter.
    if lower.contains("usage limit")
        || lower.contains("hit your session limit")
        || lower.contains("limit reached")
        || lower.contains("limit will reset")
        || lower.contains("5-hour limit")
        || lower.contains("weekly limit")
        || lower.contains("out of usage")
    {
        return Some(LimitSignal::UsageLimited {
            reset_at: parse_reset_at(pane, now, default_tz),
        });
    }

    // Transient "API Error: 529 Overloaded" — a server-side blip, not a usage
    // cap. Require the 529 code and the word on the SAME line so ordinary pane
    // text (code, logs, docs) that merely mentions either in passing isn't
    // misread as a provider error.
    if lower
        .lines()
        .any(|line| line.contains("overloaded") && line.contains("529"))
    {
        return Some(LimitSignal::Overloaded);
    }

    None
}

/// Best-effort parse of a reset time ("resets at 3:45pm", "limit will reset at
/// 23:00", "resets 3:10pm (UTC)") into the UTC instant for that clock time on
/// *today's* date. The clock is interpreted in the timezone the banner states
/// (`(UTC)`, `(GMT)`, `Z`, or an explicit `±HH[:MM]` offset); when no zone is
/// stated it's read in `default_tz` — the host's local zone in production, since
/// CLI agents render reset times in local time — then converted to UTC so
/// scheduling (always in UTC) lands on the right absolute instant. An imperfect
/// guess self-corrects because re-detection at wake time reschedules while the
/// limit persists.
///
/// The result may be in the past (we detected the banner after its reset
/// elapsed) — deciding whether that means "wake soon" or "the next reset is
/// tomorrow" belongs to [`usage_wake_at`], not here, so the day-roll accounts
/// for the post-reset buffer. Returns None when no time is found.
fn parse_reset_at(
    text: &str,
    now: DateTime<Utc>,
    default_tz: FixedOffset,
) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    let anchor = lower.find("reset")?;
    let after = &lower[anchor..];
    let (hour, minute, clock_end) = parse_clock_after(after)?;

    let zone = match parse_tz_offset_secs(&after[clock_end..]) {
        Some(secs) => FixedOffset::east_opt(secs)?,
        None => default_tz,
    };

    let today = now.with_timezone(&zone).date_naive();
    let naive = today.and_hms_opt(hour, minute, 0)?;
    Some(
        zone.from_local_datetime(&naive)
            .single()?
            .with_timezone(&Utc),
    )
}

/// When to actually re-prompt after a usage/session-window limit whose window
/// resets at `reset_at` (None if the banner named no time). Adds the post-reset
/// buffer to the stated reset; if that instant has *already elapsed* (we saw the
/// banner well after its reset — e.g. after a supervisor restart), assume the
/// next same-clock reset is a day out rather than hammering the provider
/// immediately. Falls back to a coarse backoff when the banner named no time.
fn usage_wake_at(reset_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> DateTime<Utc> {
    match reset_at {
        Some(reset) => {
            let fire = reset + chrono::Duration::seconds(USAGE_POST_RESET_BUFFER_SECS);
            if fire <= now {
                fire + chrono::Duration::days(1)
            } else {
                fire
            }
        }
        None => now + chrono::Duration::seconds(USAGE_BACKOFF_SECS),
    }
}

/// Scan for the first `H`, `H:MM`, optionally `am`/`pm` token in `s` and return
/// `(hour24, minute, end_index)`, where `end_index` is the byte offset just past
/// the matched time (so the caller can look for a trailing timezone). Returns
/// None if nothing time-like is found or it's out of range.
fn parse_clock_after(s: &str) -> Option<(u32, u32, usize)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && !bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }

    // Hour (1-2 digits).
    let h_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i - h_start < 2 {
        i += 1;
    }
    let mut hour: u32 = s[h_start..i].parse().ok()?;

    // Optional ":MM".
    let mut minute: u32 = 0;
    if i < bytes.len() && bytes[i] == b':' {
        let m_start = i + 1;
        let mut j = m_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() && j - m_start < 2 {
            j += 1;
        }
        if j > m_start {
            minute = s[m_start..j].parse().ok()?;
            i = j;
        }
    }

    // Optional am/pm (allowing a space and "a.m."-style dots).
    let marker: String = s[i..]
        .chars()
        .take(6)
        .filter(|c| !c.is_whitespace() && *c != '.')
        .collect();
    if marker.starts_with("pm") {
        if hour < 12 {
            hour += 12;
        }
        i = advance_past_ampm(s, i);
    } else if marker.starts_with("am") {
        if hour == 12 {
            hour = 0;
        }
        i = advance_past_ampm(s, i);
    }

    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute, i))
}

/// Advance past an `am`/`pm` marker starting at/after `i`, tolerating a leading
/// space and interspersed/trailing dots ("a.m."). Consumes the two meridiem
/// letters plus surrounding dots and spaces, so a following timezone token
/// (`(UTC-5)`) is left cleanly for the caller. The two-letter cap keeps it from
/// eating into a trailing word (e.g. the "m" of a later "may").
fn advance_past_ampm(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    let mut letters = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'a' | b'p' | b'm' if letters < 2 => {
                letters += 1;
                i += 1;
            }
            b'.' | b' ' => i += 1,
            _ => break,
        }
    }
    i
}

/// Parse an explicit timezone token at the start of `s` (already lowercased),
/// returning its offset from UTC in seconds. Recognizes `utc`/`gmt`/`z`
/// (optionally followed by a `±HH[:MM]` offset) and bare numeric offsets like
/// `+05:30`, `-0800`, `+2`, tolerating a leading space and `(`. Returns None
/// when no timezone-like token is present — or when a `utc`/`gmt` token is
/// followed by a malformed offset (e.g. `UTC+99`, `UTCx`) — so the caller falls
/// back to its default zone rather than silently pretending it's UTC.
fn parse_tz_offset_secs(s: &str) -> Option<i32> {
    let t = s.trim_start_matches([' ', '(']);
    if let Some(rest) = t.strip_prefix("utc").or_else(|| t.strip_prefix("gmt")) {
        let rest = rest.trim_start();
        if rest.starts_with(['+', '-']) {
            // An explicit offset attached to UTC/GMT: honor it, or reject the
            // whole token if it's out of range / unparseable.
            return parse_signed_offset(rest);
        }
        // Bare `UTC`/`GMT` — allow only a trailing separator (closing paren,
        // comma, period). Anything else is unrecognized, not UTC.
        return rest
            .trim_start_matches([')', ',', '.', ' '])
            .is_empty()
            .then_some(0);
    }
    if t.starts_with('z') {
        return Some(0);
    }
    if t.starts_with(['+', '-']) {
        return parse_signed_offset(t);
    }
    None
}

/// Parse a leading `±HH[:MM]` / `±HHMM` offset into seconds. Returns None if `s`
/// doesn't start with a sign followed by an hour.
fn parse_signed_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.strip_prefix('+') {
        Some(r) => (1, r),
        None => (-1, s.strip_prefix('-')?),
    };
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 2 {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let hours: i32 = rest[..i].parse().ok()?;

    // Optional minutes as ":MM" or a bare "MM" tail ("HHMM").
    let mut minutes: i32 = 0;
    if i < bytes.len() && bytes[i] == b':' {
        let tail = &rest[i + 1..];
        let mm: String = tail
            .chars()
            .take(2)
            .filter(|c| c.is_ascii_digit())
            .collect();
        if !mm.is_empty() {
            minutes = mm.parse().ok()?;
        }
    } else if i < bytes.len() && bytes[i].is_ascii_digit() {
        let tail = &rest[i..];
        if tail.len() >= 2 && tail.as_bytes()[1].is_ascii_digit() {
            minutes = tail[..2].parse().ok()?;
        }
    }

    if hours > 14 || minutes > 59 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

pub struct LoopSupervisor;

impl LoopSupervisor {
    pub fn spawn(db: DBService) {
        tokio::spawn(async move {
            if std::env::var(DISABLE_ENV).is_ok() {
                tracing::info!("{DISABLE_ENV} set; loop automation supervisor disabled");
                return;
            }
            if !cli_tmux_available() {
                tracing::debug!("tmux unavailable; loop automation supervisor not started");
                return;
            }

            let mut interval = tokio::time::interval(POLL_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            loop {
                interval.tick().await;
                if let Err(e) = tick(&db).await {
                    tracing::warn!("loop automation tick failed: {e}");
                }
            }
        });
    }
}

async fn tick(db: &DBService) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    deliver_due_wakeups(db, now).await?;
    detect_and_schedule(db, now).await?;
    Ok(())
}

/// Deliver (or skip) every wake-up whose fire time has passed.
async fn deliver_due_wakeups(db: &DBService, _now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let pool = &db.pool;
    for wakeup in ScheduledWakeup::list_due(pool).await? {
        let wid = wakeup.workspace_id;
        let policy = LoopAutomation::get(pool, wid).await.unwrap_or(None);
        let is_limit = !matches!(wakeup.kind, WakeupKind::Manual);

        // A limit-kind wake-up only fires while the loop is still enabled and
        // under its attempt cap (the user may have disabled it since).
        if is_limit
            && !policy
                .as_ref()
                .map(|p| p.enabled && !p.cap_reached())
                .unwrap_or(false)
        {
            ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
            continue;
        }

        let prompt = wakeup
            .prompt
            .clone()
            .or_else(|| policy.as_ref().map(|p| p.continuation_prompt.clone()))
            .unwrap_or_else(|| DEFAULT_CONTINUATION.to_string());

        // The tmux session may have been reaped before a long usage-window wake
        // came due — re-park the prompt so the next attach delivers it.
        if !cli_tmux_session_exists(wid).await {
            repark_prompt(pool, wid, &prompt).await;
            tracing::info!("loop: workspace {wid} session gone; re-parked wake-up prompt");
            ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
            continue;
        }

        // For a limit-kind wake, only re-prompt if the SAME kind of limit is
        // still showing — a manually-resumed agent must not be interrupted, and
        // an overload retry must not fire into a now-different (usage/rate)
        // limit, which would waste an attempt and re-hit a real cap. Matching
        // the visible signal's kind also makes an `Unknown` wake (no signal maps
        // to it) skip itself. Manual wake-ups are the user's explicit intent and
        // always deliver.
        if is_limit {
            // A failed capture is transient (the session still exists — that was
            // checked above); leave the wake-up pending and retry next tick
            // rather than marking it fired and losing the retry.
            let Some(pane) = capture_cli_pane(wid).await else {
                tracing::warn!("loop: pane capture failed for due wake-up on workspace {wid}");
                continue;
            };
            let still_matches =
                detect_limit(&pane).is_some_and(|sig| sig.wakeup_kind() == wakeup.kind);
            if !still_matches {
                ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
                continue;
            }
        }

        // Serialize with any in-flight parked-prompt delivery (terminal
        // attach): two concurrent paste+Enter pairs into the same pane would
        // interleave into one garbled submission. If a delivery holds the
        // claim, leave the wake-up pending — the next tick retries after the
        // delivery window closes.
        let Some(_claim) = CliPromptDelivery::try_claim(wid) else {
            tracing::debug!(
                "loop: prompt delivery in flight for workspace {wid}; deferring wake-up"
            );
            continue;
        };

        if send_cli_keys(wid, &prompt).await {
            if is_limit {
                let _ = LoopAutomation::increment_attempts(pool, wid).await;
            }
            tracing::info!(
                "loop: delivered {} wake-up to workspace {wid}",
                wakeup.kind.as_str()
            );
        } else {
            // Delivery failed (e.g. tmux rejected an over-long send). Re-park the
            // prompt so the next terminal attach delivers it rather than dropping
            // the wake-up — mirrors the session-gone branch above.
            tracing::warn!("loop: failed to deliver wake-up to workspace {wid}; re-parking");
            repark_prompt(pool, wid, &prompt).await;
        }
        ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
    }
    Ok(())
}

/// Best-effort: park `prompt` on the workspace's latest session so a terminal
/// attach delivers it (the attach path pastes a parked prompt into a live
/// agent pane, or hands it to the next fresh launch). Shared by both wake-up
/// failure branches so the re-park policy has exactly one owner. Parks only
/// into an EMPTY slot: continuation boilerplate must never overwrite a
/// parked-but-undelivered user prompt (that prompt delivers first, and the
/// loop re-detects its limit banner afterwards).
async fn repark_prompt(pool: &sqlx::SqlitePool, wid: Uuid, prompt: &str) {
    match Session::find_latest_by_workspace_id(pool, wid).await {
        Ok(Some(session)) => {
            match Session::set_pending_cli_prompt_if_empty(pool, session.id, prompt).await {
                Ok(true) => {}
                Ok(false) => tracing::info!(
                    "loop: workspace {wid} already has a parked prompt; wake-up not re-parked"
                ),
                Err(e) => {
                    tracing::warn!(
                        "loop: failed to re-park wake-up prompt for workspace {wid}: {e}"
                    )
                }
            }
        }
        Ok(None) => {
            tracing::warn!("loop: no session to re-park wake-up prompt for workspace {wid}")
        }
        Err(e) => {
            tracing::warn!("loop: failed to re-park wake-up prompt for workspace {wid}: {e}")
        }
    }
}

/// Scan each enabled workspace's pane and schedule a wake-up on a fresh limit.
async fn detect_and_schedule(db: &DBService, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let pool = &db.pool;
    for policy in LoopAutomation::list_enabled(pool).await? {
        if policy.cap_reached() {
            continue;
        }
        let wid = policy.workspace_id;
        if !cli_tmux_session_exists(wid).await {
            continue;
        }
        let Some(pane) = capture_cli_pane(wid).await else {
            continue;
        };

        // All three limit kinds schedule the same way — dedupe by kind, then
        // create one wake-up — differing only in when to fire, so compute the
        // kind + fire time and share the insert.
        let Some(signal) = detect_limit_at(&pane, now, local_offset()) else {
            continue;
        };
        let kind = signal.wakeup_kind();
        if ScheduledWakeup::has_pending(pool, wid, kind).await? {
            continue;
        }
        let fire_at = match &signal {
            LimitSignal::RateLimited => {
                now + chrono::Duration::seconds(policy.retry_interval_secs.max(1))
            }
            LimitSignal::Overloaded => now + chrono::Duration::seconds(OVERLOAD_BACKOFF_SECS),
            LimitSignal::UsageLimited { reset_at } => usage_wake_at(*reset_at, now),
        };
        ScheduledWakeup::create(pool, wid, fire_at, kind, None, policy.attempts_used + 1).await?;
        tracing::info!(
            "loop: {} on workspace {wid}; wake at {fire_at}",
            kind.as_str()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    /// A fixed UTC offset for the `default_tz` slot — keeps banner-parsing tests
    /// deterministic regardless of the host's actual local zone.
    fn utc() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    /// `detect_limit_at` with a deterministic (UTC) default zone.
    fn detect(pane: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
        detect_limit_at(pane, now, utc())
    }

    /// `parse_reset_at` with a deterministic (UTC) default zone.
    fn reset_at(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        parse_reset_at(text, now, utc())
    }

    #[test]
    fn rate_limit_banner_is_classified_first() {
        let pane = "API Error: Server is temporarily limiting requests (not your usage limit) · Rate limited";
        assert_eq!(detect_limit(pane), Some(LimitSignal::RateLimited));
    }

    #[test]
    fn rate_limited_phrase_alone_matches() {
        assert_eq!(
            detect_limit("· Rate limited, retrying"),
            Some(LimitSignal::RateLimited)
        );
    }

    #[test]
    fn usage_limit_banner_is_classified() {
        let now = at(2026, 6, 30, 9, 0);
        let got = detect("You've reached your usage limit.", now);
        assert!(matches!(got, Some(LimitSignal::UsageLimited { .. })));
    }

    #[test]
    fn normal_output_is_not_a_limit() {
        assert_eq!(detect_limit("Running tests... 12 passed"), None);
        assert_eq!(detect_limit(""), None);
    }

    #[test]
    fn parses_12h_reset_time() {
        // now 09:00 UTC, "resets at 3:45pm" -> today 15:45 (raw; buffer + any
        // day-roll are the scheduler's job, not this parser's).
        let now = at(2026, 6, 30, 9, 0);
        let got = reset_at("Your limit will reset at 3:45pm", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 15, 45));
    }

    #[test]
    fn parses_24h_reset_time() {
        let now = at(2026, 6, 30, 9, 0);
        let got = reset_at("limit will reset at 23:00", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 23, 0));
    }

    #[test]
    fn parse_reset_at_returns_todays_clock_even_when_past() {
        // now 16:00, "resets 3pm" -> today 15:00 (already past). The parser does
        // NOT roll to tomorrow; usage_wake_at owns that decision.
        let now = at(2026, 6, 30, 16, 0);
        let got = reset_at("resets 3pm", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 15, 0));
    }

    #[test]
    fn usage_limit_without_a_time_has_no_reset() {
        let now = at(2026, 6, 30, 9, 0);
        match detect("You are out of usage for now.", now) {
            Some(LimitSignal::UsageLimited { reset_at }) => assert!(reset_at.is_none()),
            other => panic!("expected usage limit, got {other:?}"),
        }
    }

    #[test]
    fn parse_clock_handles_noon_and_midnight() {
        let hm = |s| parse_clock_after(s).map(|(h, m, _)| (h, m));
        assert_eq!(hm("at 12am"), Some((0, 0)));
        assert_eq!(hm("at 12pm"), Some((12, 0)));
        assert_eq!(hm("at 12:30pm"), Some((12, 30)));
    }

    #[test]
    fn session_limit_banner_is_classified_as_usage_with_reset() {
        // The exact Claude Code banner, reset time stated in UTC.
        let now = at(2026, 6, 30, 9, 0);
        let banner = "You've hit your session limit · resets 3:10pm (UTC)";
        match detect(banner, now) {
            Some(LimitSignal::UsageLimited { reset_at }) => {
                assert_eq!(reset_at, Some(at(2026, 6, 30, 15, 10)));
            }
            other => panic!("expected session/usage limit, got {other:?}"),
        }
    }

    #[test]
    fn overloaded_529_is_transient() {
        let now = at(2026, 6, 30, 9, 0);
        let banner = "API Error: 529 Overloaded. This is a server-side issue, usually temporary - try again in a moment.";
        assert_eq!(detect(banner, now), Some(LimitSignal::Overloaded));
    }

    #[test]
    fn usage_limit_wins_over_a_stale_overloaded_line() {
        // A pane showing a lingering 529 line ABOVE a current session-limit banner
        // must classify as the usage window (wake once at reset), not as a
        // transient overload (which would hammer the capped agent every 4 min).
        let now = at(2026, 6, 30, 9, 0);
        let pane = "API Error: 529 Overloaded\n<retried>\nYou've hit your session limit · resets 3:10pm (UTC)";
        match detect(pane, now) {
            Some(LimitSignal::UsageLimited { reset_at }) => {
                assert_eq!(reset_at, Some(at(2026, 6, 30, 15, 10)));
            }
            other => panic!("expected usage limit to win, got {other:?}"),
        }
    }

    #[test]
    fn tightened_matchers_reject_casual_mentions() {
        let now = at(2026, 6, 30, 9, 0);
        // "overloaded" without the 529 code is ordinary output, not a provider error.
        assert_eq!(detect("the worker pool looks overloaded", now), None);
        // A bare "session limit" mention (e.g. discussing the feature) is not a cap.
        assert_eq!(
            detect("TODO: document the session limit behavior", now),
            None
        );
        // 529 and overloaded on DIFFERENT lines is not the provider banner.
        assert_eq!(
            detect("saw error 529 earlier\nthe queue looked overloaded", now),
            None
        );
    }

    #[test]
    fn reset_time_with_explicit_offset_converts_to_utc() {
        let now = at(2026, 6, 30, 9, 0);
        // 3:10pm in UTC-5 == 20:10 UTC.
        assert_eq!(
            reset_at("resets 3:10pm (UTC-5)", now),
            Some(at(2026, 6, 30, 20, 10))
        );
        // 20:00 in UTC+05:30 == 14:30 UTC.
        assert_eq!(
            reset_at("limit will reset at 20:00 (UTC+05:30)", now),
            Some(at(2026, 6, 30, 14, 30))
        );
    }

    #[test]
    fn dotted_meridiem_keeps_explicit_offset() {
        // "3:00 p.m." must not swallow/block the trailing "(UTC+2)".
        let now = at(2026, 6, 30, 9, 0);
        // 15:00 in UTC+2 == 13:00 UTC.
        assert_eq!(
            reset_at("limit will reset at 3:00 p.m. (UTC+2)", now),
            Some(at(2026, 6, 30, 13, 0))
        );
    }

    #[test]
    fn bare_time_uses_the_default_timezone() {
        // No explicit zone → interpret in default_tz (the host's local zone in
        // production). 3:10pm in UTC-7 == 22:10 UTC.
        let now = at(2026, 6, 30, 9, 0);
        let pdt = FixedOffset::east_opt(-7 * 3600).unwrap();
        assert_eq!(
            parse_reset_at("resets 3:10pm", now, pdt),
            Some(at(2026, 6, 30, 22, 10))
        );
    }

    #[test]
    fn utc_label_matches_bare_utc_interpretation() {
        // With a UTC default, a bare time and an explicit "(UTC)" agree.
        let now = at(2026, 6, 30, 9, 0);
        let labeled = reset_at("resets 3:10pm (UTC)", now).unwrap();
        let bare = reset_at("resets 3:10pm", now).unwrap();
        assert_eq!(labeled, bare);
        assert_eq!(labeled, at(2026, 6, 30, 15, 10));
    }

    #[test]
    fn usage_wake_at_applies_buffer_and_rolls_only_when_elapsed() {
        let buffer = chrono::Duration::seconds(USAGE_POST_RESET_BUFFER_SECS);
        let reset = at(2026, 6, 30, 15, 10);
        // Reset ahead of now → wake reset+buffer today.
        assert_eq!(
            usage_wake_at(Some(reset), at(2026, 6, 30, 9, 0)),
            reset + buffer
        );
        // Reset a minute ago but within the buffer → still today (NOT tomorrow).
        assert_eq!(
            usage_wake_at(Some(reset), at(2026, 6, 30, 15, 12)),
            reset + buffer
        );
        // Reset elapsed beyond the buffer → assume next same-clock reset tomorrow.
        assert_eq!(
            usage_wake_at(Some(reset), at(2026, 6, 30, 15, 30)),
            reset + buffer + chrono::Duration::days(1)
        );
        // No parsed time → coarse backoff from now.
        assert_eq!(
            usage_wake_at(None, at(2026, 6, 30, 9, 0)),
            at(2026, 6, 30, 9, 0) + chrono::Duration::seconds(USAGE_BACKOFF_SECS)
        );
    }

    #[test]
    fn signal_maps_to_expected_wakeup_kind() {
        assert_eq!(
            LimitSignal::RateLimited.wakeup_kind(),
            WakeupKind::RateLimitRetry
        );
        assert_eq!(
            LimitSignal::Overloaded.wakeup_kind(),
            WakeupKind::OverloadRetry
        );
        assert_eq!(
            LimitSignal::UsageLimited { reset_at: None }.wakeup_kind(),
            WakeupKind::UsageLimitWake
        );
    }

    #[test]
    fn parses_signed_offsets() {
        assert_eq!(parse_tz_offset_secs("(utc)"), Some(0));
        assert_eq!(parse_tz_offset_secs(" utc"), Some(0));
        assert_eq!(parse_tz_offset_secs("gmt"), Some(0));
        assert_eq!(parse_tz_offset_secs("z"), Some(0));
        assert_eq!(parse_tz_offset_secs("(utc+2)"), Some(2 * 3600));
        assert_eq!(
            parse_tz_offset_secs("(utc-05:30)"),
            Some(-(5 * 3600 + 30 * 60))
        );
        assert_eq!(parse_tz_offset_secs("+0800"), Some(8 * 3600));
        assert_eq!(parse_tz_offset_secs(" reset soon"), None);
        assert_eq!(parse_tz_offset_secs(""), None);
        // A malformed offset on UTC/GMT is rejected (→ caller's default zone),
        // not silently collapsed to UTC.
        assert_eq!(parse_tz_offset_secs("(utc+99)"), None);
        assert_eq!(parse_tz_offset_secs("(utcx)"), None);
    }

    #[test]
    fn parses_bare_hhmm_offset_minutes() {
        // "HHMM" form with non-zero minutes must not be truncated to the hour.
        assert_eq!(parse_tz_offset_secs("+0530"), Some(5 * 3600 + 30 * 60));
        assert_eq!(
            parse_tz_offset_secs("(utc-0830)"),
            Some(-(8 * 3600 + 30 * 60))
        );
    }

    #[test]
    fn parse_clock_after_is_utf8_boundary_safe() {
        // Multi-byte chars (middot, em dash, accented letter) around the clock
        // must never slice on a non-char boundary.
        let hm = |s| parse_clock_after(s).map(|(h, m, _)| (h, m));
        assert_eq!(hm("reset · 3:10pm"), Some((15, 10)));
        assert_eq!(hm("reset — 9am"), Some((9, 0)));
        assert_eq!(hm("résets 23:00"), Some((23, 0)));
        // Full pipeline on the real banner (leading apostrophe + middot).
        let now = at(2026, 6, 30, 9, 0);
        assert_eq!(
            reset_at("You've hit your session limit · resets 3:10pm (UTC)", now),
            Some(at(2026, 6, 30, 15, 10))
        );
    }
}

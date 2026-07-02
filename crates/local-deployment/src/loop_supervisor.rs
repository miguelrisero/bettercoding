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

use chrono::{DateTime, FixedOffset, TimeZone, Utc};
use db::{
    DBService,
    models::{
        loop_automation::{LoopAutomation, ScheduledWakeup, WakeupKind},
        session::Session,
    },
};

use crate::pty::{capture_cli_pane, cli_tmux_available, cli_tmux_session_exists, send_cli_keys};

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

/// Classify a CLI pane's visible content. Rate-limit is checked first because
/// the provider's rate-limit banner explicitly says "(not your usage limit)",
/// which would otherwise trip the usage-limit matcher.
pub fn detect_limit(pane: &str) -> Option<LimitSignal> {
    detect_limit_at(pane, Utc::now())
}

fn detect_limit_at(pane: &str, now: DateTime<Utc>) -> Option<LimitSignal> {
    let lower = pane.to_lowercase();

    // Transient rate limit — the user's exact phrasing: "Server is temporarily
    // limiting requests (not your usage limit) · Rate limited".
    if lower.contains("temporarily limiting requests") || lower.contains("rate limited") {
        return Some(LimitSignal::RateLimited);
    }

    // Transient "API Error: 529 Overloaded" — a server-side blip, not a usage
    // cap. Checked before the usage matcher; "overloaded" is distinctive enough
    // that a false positive would only cost one harmless retry.
    if lower.contains("overloaded") {
        return Some(LimitSignal::Overloaded);
    }

    // Usage-/session-window limit (5-hour / weekly / session). Checked after the
    // transient cases so the rate-limit "(not your usage limit)" disclaimer and
    // the 529 banner never land here.
    if lower.contains("usage limit")
        || lower.contains("session limit")
        || lower.contains("limit reached")
        || lower.contains("limit will reset")
        || lower.contains("5-hour limit")
        || lower.contains("weekly limit")
        || lower.contains("out of usage")
    {
        return Some(LimitSignal::UsageLimited {
            reset_at: parse_reset_at(pane, now),
        });
    }

    None
}

/// Best-effort parse of a reset time ("resets at 3:45pm", "limit will reset at
/// 23:00", "resets 3:10pm (UTC)") into the next occurrence of that clock time
/// at-or-after `now`, as a UTC instant (no post-reset buffer — the caller adds
/// one). The clock is interpreted in whatever timezone the banner states
/// (`(UTC)`, `(GMT)`, `Z`, or an explicit `±HH[:MM]` offset) and converted to
/// UTC; when no timezone is stated it's read as UTC, and an imperfect guess
/// self-corrects because re-detection at wake time reschedules while the limit
/// persists. Returns None when no time is found.
fn parse_reset_at(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    let anchor = lower.find("reset")?;
    let after = &lower[anchor..];
    let (hour, minute, clock_end) = parse_clock_after(after)?;

    // The banner states the reset clock time in some zone (often the machine's
    // local zone, sometimes an explicit "(UTC)"); read that offset so scheduling
    // — which is always in UTC — lands on the right absolute instant.
    let offset_secs = parse_tz_offset_secs(&after[clock_end..]).unwrap_or(0);
    let zone = FixedOffset::east_opt(offset_secs)?;

    // Next occurrence of `hour:minute` in `zone`, at or after `now`.
    let today = now.with_timezone(&zone).date_naive();
    let fire = next_occurrence(zone, today, hour, minute, now)?;
    Some(fire)
}

/// The UTC instant for `hour:minute` on `date` in `zone`, rolled to the next day
/// if that is already at/before `now`.
fn next_occurrence(
    zone: FixedOffset,
    date: chrono::NaiveDate,
    hour: u32,
    minute: u32,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let in_zone = |d: chrono::NaiveDate| -> Option<DateTime<Utc>> {
        let naive = d.and_hms_opt(hour, minute, 0)?;
        Some(
            zone.from_local_datetime(&naive)
                .single()?
                .with_timezone(&Utc),
        )
    };
    let today = in_zone(date)?;
    if today <= now {
        in_zone(date + chrono::Duration::days(1))
    } else {
        Some(today)
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
/// space and interspersed dots ("a.m."). Consumes at most the two meridiem
/// letters plus their spacing.
fn advance_past_ampm(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    let mut letters = 0;
    while i < bytes.len() && letters < 2 {
        match bytes[i] {
            b'a' | b'p' | b'm' => {
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
/// when no timezone-like token is present, so the caller falls back to UTC.
fn parse_tz_offset_secs(s: &str) -> Option<i32> {
    let t = s.trim_start_matches([' ', '(']);
    if let Some(rest) = t.strip_prefix("utc").or_else(|| t.strip_prefix("gmt")) {
        return Some(parse_signed_offset(rest).unwrap_or(0));
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
            if let Ok(Some(session)) = Session::find_latest_by_workspace_id(pool, wid).await {
                let _ = Session::set_pending_cli_prompt(pool, session.id, &prompt).await;
                tracing::info!("loop: workspace {wid} session gone; re-parked wake-up prompt");
            }
            ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
            continue;
        }

        // For a limit-kind wake, only re-prompt if the limit banner is STILL
        // showing — a manually-resumed agent must not be interrupted. Manual
        // wake-ups are the user's explicit intent and always deliver.
        if is_limit {
            let still_limited = capture_cli_pane(wid)
                .await
                .map(|pane| detect_limit(&pane).is_some())
                .unwrap_or(false);
            if !still_limited {
                ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
                continue;
            }
        }

        if send_cli_keys(wid, &prompt).await {
            if is_limit {
                let _ = LoopAutomation::increment_attempts(pool, wid).await;
            }
            tracing::info!(
                "loop: delivered {} wake-up to workspace {wid}",
                wakeup.kind.as_str()
            );
        } else {
            tracing::warn!("loop: failed to deliver wake-up to workspace {wid}");
        }
        ScheduledWakeup::mark_fired(pool, wakeup.id).await?;
    }
    Ok(())
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

        match detect_limit_at(&pane, now) {
            Some(LimitSignal::RateLimited) => {
                if !ScheduledWakeup::has_pending(pool, wid, WakeupKind::RateLimitRetry).await? {
                    let fire_at =
                        now + chrono::Duration::seconds(policy.retry_interval_secs.max(1));
                    ScheduledWakeup::create(
                        pool,
                        wid,
                        fire_at,
                        WakeupKind::RateLimitRetry,
                        None,
                        policy.attempts_used + 1,
                    )
                    .await?;
                    tracing::info!("loop: rate limit on workspace {wid}; retry at {fire_at}");
                }
            }
            Some(LimitSignal::Overloaded) => {
                if !ScheduledWakeup::has_pending(pool, wid, WakeupKind::OverloadRetry).await? {
                    let fire_at = now + chrono::Duration::seconds(OVERLOAD_BACKOFF_SECS);
                    ScheduledWakeup::create(
                        pool,
                        wid,
                        fire_at,
                        WakeupKind::OverloadRetry,
                        None,
                        policy.attempts_used + 1,
                    )
                    .await?;
                    tracing::info!("loop: 529 overloaded on workspace {wid}; retry at {fire_at}");
                }
            }
            Some(LimitSignal::UsageLimited { reset_at }) => {
                if !ScheduledWakeup::has_pending(pool, wid, WakeupKind::UsageLimitWake).await? {
                    // Wake a few minutes after the stated reset so the window has
                    // rolled; if we couldn't parse a reset time, re-check on a
                    // coarse backoff instead.
                    let fire_at = reset_at
                        .map(|r| r + chrono::Duration::seconds(USAGE_POST_RESET_BUFFER_SECS))
                        .unwrap_or_else(|| now + chrono::Duration::seconds(USAGE_BACKOFF_SECS));
                    ScheduledWakeup::create(
                        pool,
                        wid,
                        fire_at,
                        WakeupKind::UsageLimitWake,
                        None,
                        policy.attempts_used + 1,
                    )
                    .await?;
                    tracing::info!("loop: usage limit on workspace {wid}; wake at {fire_at}");
                }
            }
            None => {}
        }
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
        let got = detect_limit_at("You've reached your usage limit.", now);
        assert!(matches!(got, Some(LimitSignal::UsageLimited { .. })));
    }

    #[test]
    fn normal_output_is_not_a_limit() {
        assert_eq!(detect_limit("Running tests... 12 passed"), None);
        assert_eq!(detect_limit(""), None);
    }

    #[test]
    fn parses_12h_reset_time_to_next_occurrence() {
        // now 09:00 UTC, "resets at 3:45pm" -> today 15:45 (raw; buffer added by
        // the scheduler, not this parser).
        let now = at(2026, 6, 30, 9, 0);
        let got = parse_reset_at("Your limit will reset at 3:45pm", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 15, 45));
    }

    #[test]
    fn parses_24h_reset_time() {
        let now = at(2026, 6, 30, 9, 0);
        let got = parse_reset_at("limit will reset at 23:00", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 23, 0));
    }

    #[test]
    fn reset_time_already_past_rolls_to_tomorrow() {
        // now 16:00, "resets 3pm" -> 15:00 already past -> tomorrow 15:00.
        let now = at(2026, 6, 30, 16, 0);
        let got = parse_reset_at("resets 3pm", now).unwrap();
        assert_eq!(got, at(2026, 7, 1, 15, 0));
    }

    #[test]
    fn usage_limit_without_a_time_has_no_reset() {
        let now = at(2026, 6, 30, 9, 0);
        match detect_limit_at("You are out of usage for now.", now) {
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
        match detect_limit_at(banner, now) {
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
        assert_eq!(detect_limit_at(banner, now), Some(LimitSignal::Overloaded));
    }

    #[test]
    fn reset_time_with_explicit_offset_converts_to_utc() {
        let now = at(2026, 6, 30, 9, 0);
        // 3:10pm in UTC-5 == 20:10 UTC.
        let got = parse_reset_at("resets 3:10pm (UTC-5)", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 20, 10));

        // 09:00 in UTC+05:30 == 03:30 UTC (already past 09:00 UTC now → rolls to
        // tomorrow).
        let got = parse_reset_at("limit will reset at 09:00 (UTC+05:30)", now).unwrap();
        assert_eq!(got, at(2026, 7, 1, 3, 30));
    }

    #[test]
    fn utc_label_matches_bare_utc_interpretation() {
        let now = at(2026, 6, 30, 9, 0);
        let labeled = parse_reset_at("resets 3:10pm (UTC)", now).unwrap();
        let bare = parse_reset_at("resets 3:10pm", now).unwrap();
        assert_eq!(labeled, bare);
        assert_eq!(labeled, at(2026, 6, 30, 15, 10));
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
    }
}

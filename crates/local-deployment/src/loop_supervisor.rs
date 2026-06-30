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

use chrono::{DateTime, Utc};
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
    /// A usage-window limit ("reached your usage limit") — wake at the reset
    /// time when we can parse one.
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

    // Usage-window limit (5-hour / weekly). Checked after rate-limit so the
    // "(not your usage limit)" disclaimer never lands here.
    if lower.contains("usage limit")
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
/// 23:00") into the next occurrence of that clock time at-or-after `now`. The
/// banner's timezone isn't reliably stated, so the clock time is interpreted as
/// UTC; an imperfect guess self-corrects because re-detection at wake time
/// reschedules while the limit persists. Returns None when no time is found.
fn parse_reset_at(text: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let lower = text.to_lowercase();
    let anchor = lower.find("reset")?;
    let (hour, minute) = parse_clock_after(&lower[anchor..])?;

    let today = now.date_naive().and_hms_opt(hour, minute, 0)?.and_utc();
    let fire = if today <= now {
        today + chrono::Duration::days(1)
    } else {
        today
    };
    // Wake a touch after the stated reset so the window has actually rolled.
    Some(fire + chrono::Duration::seconds(60))
}

/// Scan for the first `H`, `H:MM`, optionally `am`/`pm` token in `s` and return
/// `(hour24, minute)`. Returns None if nothing time-like is found or it's out of
/// range.
fn parse_clock_after(s: &str) -> Option<(u32, u32)> {
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
    let rest: String = s[i..]
        .chars()
        .take(6)
        .filter(|c| !c.is_whitespace() && *c != '.')
        .collect();
    if rest.starts_with("pm") {
        if hour < 12 {
            hour += 12;
        }
    } else if rest.starts_with("am") && hour == 12 {
        hour = 0;
    }

    if hour > 23 || minute > 59 {
        return None;
    }
    Some((hour, minute))
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
            Some(LimitSignal::UsageLimited { reset_at }) => {
                if !ScheduledWakeup::has_pending(pool, wid, WakeupKind::UsageLimitWake).await? {
                    let fire_at = reset_at
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
        // now 09:00 UTC, "resets at 3:45pm" -> today 15:45 (+60s buffer).
        let now = at(2026, 6, 30, 9, 0);
        let got = parse_reset_at("Your limit will reset at 3:45pm", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 15, 45) + chrono::Duration::seconds(60));
    }

    #[test]
    fn parses_24h_reset_time() {
        let now = at(2026, 6, 30, 9, 0);
        let got = parse_reset_at("limit will reset at 23:00", now).unwrap();
        assert_eq!(got, at(2026, 6, 30, 23, 0) + chrono::Duration::seconds(60));
    }

    #[test]
    fn reset_time_already_past_rolls_to_tomorrow() {
        // now 16:00, "resets 3pm" -> 15:00 already past -> tomorrow 15:00.
        let now = at(2026, 6, 30, 16, 0);
        let got = parse_reset_at("resets 3pm", now).unwrap();
        assert_eq!(got, at(2026, 7, 1, 15, 0) + chrono::Duration::seconds(60));
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
        assert_eq!(parse_clock_after("at 12am"), Some((0, 0)));
        assert_eq!(parse_clock_after("at 12pm"), Some((12, 0)));
        assert_eq!(parse_clock_after("at 12:30pm"), Some((12, 30)));
    }
}

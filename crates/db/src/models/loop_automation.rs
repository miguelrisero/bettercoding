//! Agentic-loop automation persistence.
//!
//! Two entities keep a workspace's CLI agent going when a chat stops for a
//! non-completion reason:
//! - [`LoopAutomation`] — the per-workspace, opt-in policy (default OFF).
//! - [`ScheduledWakeup`] — a pending/delivered re-prompt at a specific time,
//!   created by the supervisor on detecting a limit banner, or by the user
//!   ("ping at 05:00 UTC").

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use uuid::Uuid;

/// Why a wake-up was scheduled. Stored as a snake_case string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(export)]
pub enum WakeupKind {
    /// Transient "Server is temporarily limiting requests · Rate limited" — a
    /// short backoff retry (every `retry_interval_secs`).
    RateLimitRetry,
    /// A usage-window limit ("usage limit reached") — wake at the reset time.
    UsageLimitWake,
    /// User-scheduled ("ping at 05:00 UTC", next-day).
    Manual,
}

impl WakeupKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            WakeupKind::RateLimitRetry => "rate_limit_retry",
            WakeupKind::UsageLimitWake => "usage_limit_wake",
            WakeupKind::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "rate_limit_retry" => WakeupKind::RateLimitRetry,
            "usage_limit_wake" => WakeupKind::UsageLimitWake,
            _ => WakeupKind::Manual,
        }
    }
}

/// Per-workspace loop-automation policy. A row exists only once the user has
/// touched the toggle; absence means disabled (the default).
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct LoopAutomation {
    pub workspace_id: Uuid,
    pub enabled: bool,
    pub retry_interval_secs: i64,
    pub continuation_prompt: String,
    pub max_attempts: i64,
    pub attempts_used: i64,
    #[ts(type = "string")]
    pub updated_at: DateTime<Utc>,
}

impl LoopAutomation {
    /// The effective policy for a workspace, or `None` when never configured
    /// (treated as disabled).
    pub async fn get(pool: &SqlitePool, workspace_id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 enabled,
                 retry_interval_secs,
                 continuation_prompt,
                 max_attempts,
                 attempts_used,
                 updated_at as "updated_at!: DateTime<Utc>"
               FROM loop_automations
               WHERE workspace_id = $1"#,
            workspace_id
        )
        .fetch_optional(pool)
        .await?;

        Ok(row.map(|r| Self {
            workspace_id: r.workspace_id,
            enabled: r.enabled != 0,
            retry_interval_secs: r.retry_interval_secs,
            continuation_prompt: r.continuation_prompt,
            max_attempts: r.max_attempts,
            attempts_used: r.attempts_used,
            updated_at: r.updated_at,
        }))
    }

    /// Create or replace a workspace's policy. Editing the settings re-arms the
    /// loop (attempts_used reset to 0).
    pub async fn upsert(
        pool: &SqlitePool,
        workspace_id: Uuid,
        enabled: bool,
        retry_interval_secs: i64,
        continuation_prompt: &str,
        max_attempts: i64,
    ) -> Result<Self, sqlx::Error> {
        let enabled_i = i64::from(enabled);
        sqlx::query!(
            r#"INSERT INTO loop_automations
                 (workspace_id, enabled, retry_interval_secs, continuation_prompt,
                  max_attempts, attempts_used, updated_at)
               VALUES ($1, $2, $3, $4, $5, 0, datetime('now', 'subsec'))
               ON CONFLICT(workspace_id) DO UPDATE SET
                 enabled = excluded.enabled,
                 retry_interval_secs = excluded.retry_interval_secs,
                 continuation_prompt = excluded.continuation_prompt,
                 max_attempts = excluded.max_attempts,
                 attempts_used = 0,
                 updated_at = excluded.updated_at"#,
            workspace_id,
            enabled_i,
            retry_interval_secs,
            continuation_prompt,
            max_attempts
        )
        .execute(pool)
        .await?;

        Ok(Self::get(pool, workspace_id).await?.expect("just upserted"))
    }

    /// All workspaces with the loop enabled (the supervisor's detection set).
    pub async fn list_enabled(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT
                 workspace_id as "workspace_id!: Uuid",
                 enabled,
                 retry_interval_secs,
                 continuation_prompt,
                 max_attempts,
                 attempts_used,
                 updated_at as "updated_at!: DateTime<Utc>"
               FROM loop_automations
               WHERE enabled = 1"#
        )
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|r| Self {
                workspace_id: r.workspace_id,
                enabled: r.enabled != 0,
                retry_interval_secs: r.retry_interval_secs,
                continuation_prompt: r.continuation_prompt,
                max_attempts: r.max_attempts,
                attempts_used: r.attempts_used,
                updated_at: r.updated_at,
            })
            .collect())
    }

    /// Whether the attempt cap has been reached (0 = uncapped).
    pub fn cap_reached(&self) -> bool {
        self.max_attempts > 0 && self.attempts_used >= self.max_attempts
    }

    /// Record one delivered automatic re-prompt; returns the new count.
    pub async fn increment_attempts(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        let row = sqlx::query!(
            r#"UPDATE loop_automations
               SET attempts_used = attempts_used + 1,
                   updated_at = datetime('now', 'subsec')
               WHERE workspace_id = $1
               RETURNING attempts_used"#,
            workspace_id
        )
        .fetch_one(pool)
        .await?;
        Ok(row.attempts_used)
    }
}

/// A pending or delivered re-prompt for a workspace's CLI agent.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct ScheduledWakeup {
    pub id: Uuid,
    pub workspace_id: Uuid,
    #[ts(type = "string")]
    pub fire_at: DateTime<Utc>,
    pub kind: WakeupKind,
    pub prompt: Option<String>,
    pub attempt: i64,
    #[ts(type = "string | null")]
    pub fired_at: Option<DateTime<Utc>>,
    #[ts(type = "string")]
    pub created_at: DateTime<Utc>,
}

impl ScheduledWakeup {
    pub async fn create(
        pool: &SqlitePool,
        workspace_id: Uuid,
        fire_at: DateTime<Utc>,
        kind: WakeupKind,
        prompt: Option<&str>,
        attempt: i64,
    ) -> Result<Self, sqlx::Error> {
        let id = Uuid::new_v4();
        let kind_s = kind.as_str();
        sqlx::query!(
            r#"INSERT INTO scheduled_wakeups
                 (id, workspace_id, fire_at, kind, prompt, attempt, created_at)
               VALUES ($1, $2, $3, $4, $5, $6, datetime('now', 'subsec'))"#,
            id,
            workspace_id,
            fire_at,
            kind_s,
            prompt,
            attempt
        )
        .execute(pool)
        .await?;

        Ok(Self::find_by_id(pool, id).await?.expect("just inserted"))
    }

    pub async fn find_by_id(pool: &SqlitePool, id: Uuid) -> Result<Option<Self>, sqlx::Error> {
        let row = sqlx::query!(
            r#"SELECT
                 id as "id!: Uuid",
                 workspace_id as "workspace_id!: Uuid",
                 fire_at as "fire_at!: DateTime<Utc>",
                 kind,
                 prompt,
                 attempt,
                 fired_at as "fired_at?: DateTime<Utc>",
                 created_at as "created_at!: DateTime<Utc>"
               FROM scheduled_wakeups WHERE id = $1"#,
            id
        )
        .fetch_optional(pool)
        .await?;
        Ok(row.map(|r| Self {
            id: r.id,
            workspace_id: r.workspace_id,
            fire_at: r.fire_at,
            kind: WakeupKind::parse(&r.kind),
            prompt: r.prompt,
            attempt: r.attempt,
            fired_at: r.fired_at,
            created_at: r.created_at,
        }))
    }

    /// Pending wake-ups whose fire time has passed (the supervisor's deliver set).
    pub async fn list_due(pool: &SqlitePool) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT
                 id as "id!: Uuid",
                 workspace_id as "workspace_id!: Uuid",
                 fire_at as "fire_at!: DateTime<Utc>",
                 kind,
                 prompt,
                 attempt,
                 fired_at as "fired_at?: DateTime<Utc>",
                 created_at as "created_at!: DateTime<Utc>"
               FROM scheduled_wakeups
               WHERE fired_at IS NULL AND datetime(fire_at) <= datetime('now')
               ORDER BY fire_at ASC"#
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Self {
                id: r.id,
                workspace_id: r.workspace_id,
                fire_at: r.fire_at,
                kind: WakeupKind::parse(&r.kind),
                prompt: r.prompt,
                attempt: r.attempt,
                fired_at: r.fired_at,
                created_at: r.created_at,
            })
            .collect())
    }

    /// Whether a pending wake-up of this kind already exists (scheduling dedupe).
    pub async fn has_pending(
        pool: &SqlitePool,
        workspace_id: Uuid,
        kind: WakeupKind,
    ) -> Result<bool, sqlx::Error> {
        let kind_s = kind.as_str();
        let row = sqlx::query!(
            r#"SELECT COUNT(*) as "count!: i64"
               FROM scheduled_wakeups
               WHERE workspace_id = $1 AND kind = $2 AND fired_at IS NULL"#,
            workspace_id,
            kind_s
        )
        .fetch_one(pool)
        .await?;
        Ok(row.count > 0)
    }

    /// Wake-ups for a workspace (for the UI / status). Pending first.
    pub async fn list_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
        include_fired: bool,
    ) -> Result<Vec<Self>, sqlx::Error> {
        let rows = sqlx::query!(
            r#"SELECT
                 id as "id!: Uuid",
                 workspace_id as "workspace_id!: Uuid",
                 fire_at as "fire_at!: DateTime<Utc>",
                 kind,
                 prompt,
                 attempt,
                 fired_at as "fired_at?: DateTime<Utc>",
                 created_at as "created_at!: DateTime<Utc>"
               FROM scheduled_wakeups
               WHERE workspace_id = $1 AND ($2 OR fired_at IS NULL)
               ORDER BY fired_at IS NOT NULL, fire_at ASC"#,
            workspace_id,
            include_fired
        )
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| Self {
                id: r.id,
                workspace_id: r.workspace_id,
                fire_at: r.fire_at,
                kind: WakeupKind::parse(&r.kind),
                prompt: r.prompt,
                attempt: r.attempt,
                fired_at: r.fired_at,
                created_at: r.created_at,
            })
            .collect())
    }

    pub async fn mark_fired(pool: &SqlitePool, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"UPDATE scheduled_wakeups SET fired_at = datetime('now', 'subsec') WHERE id = $1"#,
            id
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    pub async fn delete(pool: &SqlitePool, id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query!(r#"DELETE FROM scheduled_wakeups WHERE id = $1"#, id)
            .execute(pool)
            .await?;
        Ok(())
    }

    /// Drop all pending wake-ups for a workspace (e.g. when the loop is disabled).
    pub async fn delete_pending_for_workspace(
        pool: &SqlitePool,
        workspace_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"DELETE FROM scheduled_wakeups WHERE workspace_id = $1 AND fired_at IS NULL"#,
            workspace_id
        )
        .execute(pool)
        .await?;
        Ok(())
    }
}

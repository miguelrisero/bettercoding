//! Bounded, restart-safe Input availability. OS delivery is best effort: a crash
//! after display and before token completion can repeat a notification.
use std::time::Duration;

use db::models::workspace_input::{InputError, WorkspaceInput};
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use super::notification::NotificationService;

pub fn spawn(pool: SqlitePool, notifications: NotificationService, shutdown: CancellationToken) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        result = tick(&pool, &notifications) => {
                            if let Err(error) = result {
                                tracing::warn!(%error, "Input availability check failed");
                            }
                        }
                    }
                }
            }
        }
    });
}

async fn tick(pool: &SqlitePool, notifications: &NotificationService) -> Result<(), InputError> {
    WorkspaceInput::release_due(pool).await?;
    // Bound both the batch and elapsed delivery time; the next tick continues it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    for _ in 0..100 {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let Some(claim) = WorkspaceInput::claim_notification(pool).await? else {
            break;
        };
        if WorkspaceInput::notification_is_current(pool, &claim).await? {
            let result = tokio::time::timeout_at(
                deadline,
                notifications.notify(
                    "BetterCoding Input available",
                    "An Input is ready for your review.",
                    Some(claim.workspace_id),
                ),
            )
            .await;
            if result.is_err() {
                tracing::warn!(generation = %claim.generation, "Input notification attempt timed out");
            }
        }
        WorkspaceInput::finish_notification(pool, &claim).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use db::models::workspace_input::{CreateInput, InputActorKind, InputWrite};
    use tokio::sync::RwLock;
    use uuid::Uuid;

    use super::*;
    use crate::services::{config::Config, notification::PushNotifier};

    #[derive(Default)]
    struct Capture(Mutex<Vec<(String, String, Option<Uuid>)>>);
    #[async_trait]
    impl PushNotifier for Capture {
        async fn send(&self, title: &str, message: &str, workspace: Option<Uuid>) {
            self.0
                .lock()
                .unwrap()
                .push((title.into(), message.into(), workspace));
        }
    }

    #[tokio::test]
    async fn immediate_and_due_use_the_same_preferences_and_destination() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE workspaces(id BLOB PRIMARY KEY);")
            .execute(&pool)
            .await
            .unwrap();
        for migration in [
            include_str!("../../../db/migrations/20261027090000_workspace_inputs.sql"),
            include_str!("../../../db/migrations/20261028120000_workspace_input_priority.sql"),
            include_str!("../../../db/migrations/20261030120000_workspace_input_schedule.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        let workspace = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces VALUES (?)")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        let capture = Arc::new(Capture::default());
        let mut config = Config::default();
        config.notifications.sound_enabled = false;
        config.notifications.push_enabled = true;
        let config = Arc::new(RwLock::new(config));
        let notifications =
            NotificationService::with_push_notifier(config.clone(), capture.clone());
        for (key, show_at) in [
            ("immediate", serde_json::Value::Null),
            ("due", serde_json::json!("2099-01-01T00:00:00Z")),
        ] {
            let request: CreateInput = serde_json::from_value(serde_json::json!({
                "external_key":key,"request_id":key,"expected_sequence":0,"show_at":show_at,
                "content":{"title":"private title","html":"private body","contract":{"choices":[],"allow_text":true,"allow_no_answer_yet":false}}
            })).unwrap();
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Create(request),
            )
            .await
            .unwrap();
        }
        tick(&pool, &notifications).await.unwrap();
        assert_eq!(capture.0.lock().unwrap().len(), 1);
        sqlx::query("UPDATE workspace_inputs SET show_at='2000-01-01T00:00:00.000000000Z' WHERE external_key='due'").execute(&pool).await.unwrap();
        tick(&pool, &notifications).await.unwrap();
        tick(&pool, &notifications).await.unwrap();
        let notices = capture.0.lock().unwrap().clone();
        assert_eq!(notices.len(), 2);
        assert!(notices.iter().all(|notice| notice
            == &(
                "BetterCoding Input available".into(),
                "An Input is ready for your review.".into(),
                Some(workspace)
            )));
        config.write().await.notifications.push_enabled = false;
        notifications
            .notify("disabled", "disabled", Some(workspace))
            .await;
        assert_eq!(capture.0.lock().unwrap().len(), 2);
    }
}

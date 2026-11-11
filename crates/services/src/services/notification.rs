use std::{
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use async_trait::async_trait;
use db::models::workspace_input::{InputError, InputNotificationClaim, WorkspaceInput};
use sqlx::SqlitePool;
use tokio::sync::RwLock;
use utils::{self, command_ext::NoWindowExt};
use uuid::Uuid;

use crate::services::config::{Config, SoundFile};

/// Trait for sending push notifications. Implementations can use
/// platform-specific OS commands, Tauri's notification plugin, etc.
#[async_trait]
pub trait PushNotifier: Send + Sync + 'static {
    async fn send(&self, title: &str, message: &str, workspace_id: Option<Uuid>);

    /// Check Input availability at application handoff. OS display is best effort.
    async fn send_input(
        &self,
        pool: &SqlitePool,
        claim: &InputNotificationClaim,
        title: &str,
        message: &str,
    ) -> Result<(), InputError> {
        if WorkspaceInput::notification_is_current(pool, claim).await? {
            self.send(title, message, Some(claim.workspace_id)).await;
        }
        Ok(())
    }
}

/// Global push notifier set before server startup (e.g., by the Tauri app).
/// Falls back to `DefaultPushNotifier` if not set.
static GLOBAL_PUSH_NOTIFIER: OnceLock<Arc<dyn PushNotifier>> = OnceLock::new();

/// Register a custom push notifier globally. Must be called before the server
/// starts (i.e., before `LocalDeployment::new()`). Typically called from the
/// Tauri app to inject a `TauriNotifier` that uses the native notification API.
pub fn set_global_push_notifier(notifier: Arc<dyn PushNotifier>) {
    let _ = GLOBAL_PUSH_NOTIFIER.set(notifier);
}

/// Get the global push notifier, or `DefaultPushNotifier` if none was set.
pub fn get_global_push_notifier() -> Arc<dyn PushNotifier> {
    GLOBAL_PUSH_NOTIFIER
        .get()
        .cloned()
        .unwrap_or_else(|| Arc::new(DefaultPushNotifier))
}

/// Default push notifier using platform-specific OS commands.
/// Used as a fallback when no Tauri app handle is available.
pub struct DefaultPushNotifier;

/// Cache for WSL root path from PowerShell
static WSL_ROOT_PATH_CACHE: OnceLock<Option<String>> = OnceLock::new();

#[async_trait]
impl PushNotifier for DefaultPushNotifier {
    async fn send(&self, title: &str, message: &str, _workspace_id: Option<Uuid>) {
        if cfg!(target_os = "macos") {
            send_macos_notification(title, message).await;
        } else if cfg!(target_os = "linux") && !utils::is_wsl2() {
            send_linux_notification(title, message).await;
        } else if cfg!(target_os = "windows") || (cfg!(target_os = "linux") && utils::is_wsl2()) {
            send_windows_notification(title, message).await;
        }
    }

    async fn send_input(
        &self,
        pool: &SqlitePool,
        claim: &InputNotificationClaim,
        title: &str,
        message: &str,
    ) -> Result<(), InputError> {
        if cfg!(target_os = "windows") || (cfg!(target_os = "linux") && utils::is_wsl2()) {
            send_prepared_input(pool, claim, prepare_windows_notification(), |script_path| {
                send_prepared_windows_notification(&script_path, title, message)
            })
            .await
        } else {
            if WorkspaceInput::notification_is_current(pool, claim).await? {
                self.send(title, message, Some(claim.workspace_id)).await;
            }
            Ok(())
        }
    }
}

/// Service for handling cross-platform notifications including sound alerts and push notifications
#[derive(Clone)]
pub struct NotificationService {
    config: Arc<RwLock<Config>>,
    push_notifier: Arc<dyn PushNotifier>,
}

impl std::fmt::Debug for NotificationService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotificationService")
            .field("config", &self.config)
            .finish()
    }
}

impl NotificationService {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        Self {
            config,
            push_notifier: get_global_push_notifier(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_push_notifier(
        config: Arc<RwLock<Config>>,
        push_notifier: Arc<dyn PushNotifier>,
    ) -> Self {
        Self {
            config,
            push_notifier,
        }
    }

    /// Send both sound and push notifications if enabled.
    /// `workspace_id` is forwarded to the push notifier so Tauri can emit a
    /// navigation event when the notification is clicked.
    pub async fn notify(&self, title: &str, message: &str, workspace_id: Option<Uuid>) {
        let config = self.config.read().await.notifications.clone();

        if config.sound_enabled {
            Self::play_sound_notification(&config.sound_file).await;
        }

        if config.push_enabled {
            self.push_notifier.send(title, message, workspace_id).await;
        }
    }

    /// Validate Input availability at each application handoff. OS display is best effort.
    pub(crate) async fn notify_input(
        &self,
        pool: &SqlitePool,
        claim: &InputNotificationClaim,
        title: &str,
        message: &str,
    ) -> Result<(), InputError> {
        let config = self.config.read().await.notifications.clone();
        if config.sound_enabled
            && let Some(file_path) = Self::prepare_sound_notification(&config.sound_file).await
        {
            if !WorkspaceInput::notification_is_current(pool, claim).await? {
                return Ok(());
            }
            Self::play_prepared_sound(&file_path);
        }
        if config.push_enabled {
            self.push_notifier
                .send_input(pool, claim, title, message)
                .await?;
        }
        Ok(())
    }

    /// Play a system sound notification across platforms
    async fn play_sound_notification(sound_file: &SoundFile) {
        if let Some(file_path) = Self::prepare_sound_notification(sound_file).await {
            Self::play_prepared_sound(&file_path);
        }
    }

    async fn prepare_sound_notification(sound_file: &SoundFile) -> Option<PathBuf> {
        let file_path = match sound_file.get_path().await {
            Ok(path) => path,
            Err(e) => {
                tracing::error!("Failed to create cached sound file: {}", e);
                return None;
            }
        };
        if utils::is_wsl2()
            && let Some(windows_path) = wsl_to_windows_path(&file_path).await
        {
            return Some(PathBuf::from(windows_path));
        }
        Some(file_path)
    }

    fn play_prepared_sound(file_path: &Path) {
        // Use platform-specific sound notification
        // Note: spawn() calls are intentionally not awaited - sound notifications should be fire-and-forget
        if cfg!(target_os = "macos") {
            let _ = tokio::process::Command::new("afplay")
                .arg(file_path)
                .spawn();
        } else if cfg!(target_os = "linux") && !utils::is_wsl2() {
            // Try different Linux audio players
            if tokio::process::Command::new("paplay")
                .arg(file_path)
                .spawn()
                .is_ok()
            {
                // Success with paplay
            } else if tokio::process::Command::new("aplay")
                .arg(file_path)
                .spawn()
                .is_ok()
            {
                // Success with aplay
            } else {
                // Try system bell as fallback
                let _ = tokio::process::Command::new("echo")
                    .arg("-e")
                    .arg("\\a")
                    .spawn();
            }
        } else if cfg!(target_os = "windows") || (cfg!(target_os = "linux") && utils::is_wsl2()) {
            let file_path = file_path.to_string_lossy();
            let _ = tokio::process::Command::new("powershell.exe")
                .arg("-c")
                .arg(format!(
                    r#"(New-Object Media.SoundPlayer "{file_path}").PlaySync()"#
                ))
                .no_window()
                .spawn();
        }
    }
}

// --- Platform-specific push notification helpers (used by DefaultPushNotifier) ---

/// Send macOS notification using osascript
async fn send_macos_notification(title: &str, message: &str) {
    let script = format!(
        r#"display notification "{message}" with title "{title}" sound name "Glass""#,
        message = message.replace('"', r#"\""#),
        title = title.replace('"', r#"\""#)
    );

    let _ = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn();
}

/// Send Linux notification using notify-rust
async fn send_linux_notification(title: &str, message: &str) {
    use notify_rust::Notification;

    let title = title.to_string();
    let message = message.to_string();

    let _handle = tokio::task::spawn_blocking(move || {
        match Notification::new()
            .summary(&title)
            .body(&message)
            .timeout(10000)
            .show()
        {
            Ok(_) => {}
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("ServiceUnknown")
                    || err_str.contains("org.freedesktop.Notifications")
                {
                    tracing::warn!("Linux notification daemon not available: {}", e);
                } else {
                    tracing::warn!("Failed to send Linux notification: {}", e);
                }
            }
        }
    });
    drop(_handle); // Don't await, fire-and-forget
}

async fn send_prepared_input<T>(
    pool: &SqlitePool,
    claim: &InputNotificationClaim,
    preparation: impl std::future::Future<Output = Option<T>>,
    send: impl FnOnce(T),
) -> Result<(), InputError> {
    if !WorkspaceInput::notification_is_current(pool, claim).await? {
        return Ok(());
    }
    if let Some(prepared) = preparation.await {
        send(prepared);
    }
    Ok(())
}

/// Send Windows/WSL notification using PowerShell toast script
async fn send_windows_notification(title: &str, message: &str) {
    if let Some(script_path) = prepare_windows_notification().await {
        send_prepared_windows_notification(&script_path, title, message);
    }
}

async fn prepare_windows_notification() -> Option<String> {
    let script_path = match utils::get_powershell_script().await {
        Ok(path) => path,
        Err(e) => {
            tracing::error!("Failed to get PowerShell script: {}", e);
            return None;
        }
    };

    // Convert WSL path to Windows path if in WSL2
    let script_path_str = if utils::is_wsl2() {
        if let Some(windows_path) = wsl_to_windows_path(&script_path).await {
            windows_path
        } else {
            script_path.to_string_lossy().to_string()
        }
    } else {
        script_path.to_string_lossy().to_string()
    };

    Some(script_path_str)
}

fn send_prepared_windows_notification(script_path: &str, title: &str, message: &str) {
    let _ = tokio::process::Command::new("powershell.exe")
        .arg("-NoProfile")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(script_path)
        .arg("-Title")
        .arg(title)
        .arg("-Message")
        .arg(message)
        .no_window()
        .spawn();
}

/// Get WSL root path via PowerShell (cached)
async fn get_wsl_root_path() -> Option<String> {
    if let Some(cached) = WSL_ROOT_PATH_CACHE.get() {
        return cached.clone();
    }

    match tokio::process::Command::new("powershell.exe")
        .arg("-c")
        .arg("(Get-Location).Path -replace '^.*::', ''")
        .current_dir("/")
        .no_window()
        .output()
        .await
    {
        Ok(output) => {
            match String::from_utf8(output.stdout) {
                Ok(pwd_str) => {
                    let pwd = pwd_str.trim();
                    tracing::info!("WSL root path detected: {}", pwd);

                    // Cache the result
                    let _ = WSL_ROOT_PATH_CACHE.set(Some(pwd.to_string()));
                    return Some(pwd.to_string());
                }
                Err(e) => {
                    tracing::error!("Failed to parse PowerShell pwd output as UTF-8: {}", e);
                }
            }
        }
        Err(e) => {
            tracing::error!("Failed to execute PowerShell pwd command: {}", e);
        }
    }

    // Cache the failure result
    let _ = WSL_ROOT_PATH_CACHE.set(None);
    None
}

/// Convert WSL path to Windows UNC path for PowerShell
async fn wsl_to_windows_path(wsl_path: &std::path::Path) -> Option<String> {
    let path_str = wsl_path.to_string_lossy();

    // Relative paths work fine as-is in PowerShell
    if !path_str.starts_with('/') {
        tracing::debug!("Using relative path as-is: {}", path_str);
        return Some(path_str.to_string());
    }

    // Get cached WSL root path from PowerShell
    if let Some(wsl_root) = get_wsl_root_path().await {
        // Simply concatenate WSL root with the absolute path - PowerShell doesn't mind /
        let windows_path = format!("{wsl_root}{path_str}");
        tracing::debug!("WSL path converted: {} -> {}", path_str, windows_path);
        Some(windows_path)
    } else {
        tracing::error!(
            "Failed to determine WSL root path for conversion: {}",
            path_str
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use db::models::workspace_input::{CreateInput, InputActorKind, InputWrite};

    use super::*;

    #[tokio::test]
    async fn expiry_during_push_preparation_suppresses_handoff() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE workspaces(id BLOB PRIMARY KEY);")
            .execute(&pool)
            .await
            .unwrap();
        for migration in [
            include_str!("../../../db/migrations/20261027090000_workspace_inputs.sql"),
            include_str!("../../../db/migrations/20261028120000_workspace_input_priority.sql"),
            include_str!("../../../db/migrations/20261030120000_workspace_input_schedule.sql"),
            include_str!("../../../db/migrations/20261111090000_workspace_input_expiry.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        let workspace = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces VALUES (?)")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        let request: CreateInput = serde_json::from_value(serde_json::json!({
            "external_key":"push-preparation","request_id":"push-preparation","expected_sequence":0,
            "expires_at":"2099-01-01T00:00:00Z",
            "content":{"title":"private title","html":"private body","contract":{"choices":[],"allow_text":true,"allow_no_answer_yet":false}}
        })).unwrap();
        let created = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request),
        )
        .await
        .unwrap();
        let claim = WorkspaceInput::claim_notification(&pool)
            .await
            .unwrap()
            .unwrap();
        let (started, preparation_started) = tokio::sync::oneshot::channel();
        let (resume, preparation_resumed) = tokio::sync::oneshot::channel();
        let mut handoffs = Vec::new();
        let preparation = async {
            started.send(()).unwrap();
            preparation_resumed.await.unwrap();
            Some("prepared-script")
        };
        let delivery = send_prepared_input(&pool, &claim, preparation, |prepared| {
            handoffs.push(prepared);
        });
        let expire = async {
            preparation_started.await.unwrap();
            sqlx::query(
                "UPDATE workspace_inputs SET expires_at='2000-01-01T00:00:00.000000000Z' WHERE id=?",
            )
            .bind(&created.input_id)
            .execute(&pool)
            .await
            .unwrap();
            resume.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(delivery, expire);
        result.unwrap();
        assert!(
            handoffs.is_empty(),
            "expired Input reached push handoff after asynchronous preparation"
        );
        let (status,): (String,) = sqlx::query_as("SELECT status FROM workspace_inputs WHERE id=?")
            .bind(&created.input_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(status, "expired");
        for _ in 0..2 {
            let events = WorkspaceInput::events(&pool, workspace, Some(&created.input_id), 0, 100)
                .await
                .unwrap()
                .events;
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.event_type == "expired")
                    .count(),
                1
            );
        }
    }
}

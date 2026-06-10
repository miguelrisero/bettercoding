use api_types::{
    AuthMethodsResponse, HandoffInitRequest, HandoffRedeemRequest, LocalLoginRequest,
    ProfileResponse, StatusResponse,
};
use axum::{
    Router,
    extract::{Json, Query, State},
    http::{Response, StatusCode},
    response::Json as ResponseJson,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use deployment::Deployment;
use rand::{Rng, distributions::Alphanumeric};
use serde::{Deserialize, Serialize};
use services::services::{
    config::save_config_to_file, oauth_credentials::Credentials, remote_sync,
};
use sha2::{Digest, Sha256};
use ts_rs::TS;
use utils::{assets::config_path, jwt::extract_expiration, response::ApiResponse};
use uuid::Uuid;

use crate::{DeploymentImpl, error::ApiError, runtime::relay_registration};

/// Base64-encoded 32x32 app icon (from `crates/tauri-app/icons/32x32.png`).
const APP_ICON_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAACAAAAAgCAYAAABzenr0AAACc0lEQVR42u2X0UtTcRTHP7973VXnIG+ipW0ssAgDq41CgkSkYMQgWG/F6KWgv6AHffFlT3sMMaggMqTeksj2JPbgy/ayCANFpFyj4XS6zUzdbff2sHY1kT043XrY9+13z7m/87nnnN+PewT7yG63GxyB4vG42PtMVCJwKRCp0sH3xpIqHXwvhESVJarx9btV9QzUAKoOUFfKOD09jdPpNNf5fJ7l5WXC4TDBYJBYLGbazrfIPLig4GqTaawTxLI6b2ZzvJ3XMA56CooAa2trJBIJFEWhs7MTIQSLi4v09vZiGAbuEzIjN6wocuE9TQfL39w++5zjyaft8kowMTGBx+Ohv7+foaEhAJxOJx0dHQA8utKAIsPKpsGd9xtcHVvnxUwOgPvdCu1N0uH0gMViwW63A7C0tEQymaS9SeLc8cI2r77kmFvV0Q0YiW7zLavz8ftv1AZxsB4oyu/34/f7zfXk5CSBQABN0zjdurPFzEp+p18MuD2+UV4TFlXsAUmScDgc9PX1kc1mGRwcpEHeMv3WNeNwT8HuHhgYGACgubmZUCiEz+cjlUoRehow/dR6cfT3QDqdJhqNAtDT08NCWjdtl9rkf3wfX29kzGvFd9ZyeAA2mw2Xy2XCpDYNoslC7e92KXS3FiBunbFw7VQdXS0yP37q5ZXA6/XidrsRQuBwOLDZbACMjo4CEIxs8dxj5Vi94OVNK7k85p3wbkEjnMiXB6CqKqqqApDJZIhEIgwPDzM1NQXA3KrOvQ+/eHixnssnZWwWwdeMzvi8xuvZXO1/oAZQAygNsN+4VCnF43Hxf5SgGlkoxpRKTa6VGE5FtcfzPzdv5SaGHcH2AAAAAElFTkSuQmCC";

/// Shared CSS styles for standalone OAuth HTML pages (success & error).
/// Colors and typography match the app's design system (light mode defaults
/// from `packages/web-core/src/app/styles/new/index.css`).
const AUTH_PAGE_STYLES: &str = r#"<style>
  @import url('https://fonts.googleapis.com/css2?family=IBM+Plex+Sans:wght@400;500;600&display=swap');
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: 'IBM Plex Sans', -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    background: #f2f2f2;
    color: #333;
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .container {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 24px;
    padding: 24px;
  }
  .logo { width: 40px; height: 40px; }
  .content {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 4px;
  }
  .title {
    font-size: 13px;
    font-weight: 500;
    color: #0d0d0d;
  }
  .subtitle {
    font-size: 12px;
    color: #636363;
  }
</style>"#;

/// Response from GET /api/auth/token - returns the current access token
#[derive(Debug, Serialize, TS)]
pub struct TokenResponse {
    pub access_token: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response from GET /api/auth/user - returns the current user ID
#[derive(Debug, Serialize, TS)]
pub struct CurrentUserResponse {
    pub user_id: String,
}

pub fn router() -> Router<DeploymentImpl> {
    Router::new()
        .route("/auth/methods", get(auth_methods))
        .route("/auth/handoff/init", post(handoff_init))
        .route("/auth/handoff/complete", get(handoff_complete))
        .route("/auth/local/login", post(local_login))
        .route("/auth/logout", post(logout))
        .route("/auth/status", get(status))
        .route("/auth/token", get(get_token))
        .route("/auth/user", get(get_current_user))
}

async fn auth_methods(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<AuthMethodsResponse>>, ApiError> {
    let client = deployment.remote_client()?;
    let methods = client.auth_methods().await?;
    Ok(ResponseJson(ApiResponse::success(methods)))
}

#[derive(Debug, Deserialize)]
struct HandoffInitPayload {
    provider: String,
    return_to: String,
}

#[derive(Debug, Serialize)]
struct HandoffInitResponseBody {
    handoff_id: Uuid,
    authorize_url: String,
}

async fn handoff_init(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<HandoffInitPayload>,
) -> Result<ResponseJson<ApiResponse<HandoffInitResponseBody>>, ApiError> {
    let client = deployment.remote_client()?;

    let app_verifier = generate_secret();
    let app_challenge = hash_sha256_hex(&app_verifier);

    let request = HandoffInitRequest {
        provider: payload.provider.clone(),
        return_to: payload.return_to.clone(),
        app_challenge,
    };

    let response = client.handoff_init(&request).await?;

    deployment
        .store_oauth_handoff(response.handoff_id, payload.provider, app_verifier)
        .await;

    Ok(ResponseJson(ApiResponse::success(
        HandoffInitResponseBody {
            handoff_id: response.handoff_id,
            authorize_url: response.authorize_url,
        },
    )))
}

#[derive(Debug, Deserialize)]
struct HandoffCompleteQuery {
    handoff_id: Uuid,
    #[serde(default)]
    app_code: Option<String>,
    #[serde(default)]
    error: Option<String>,
    /// When set to "desktop", the callback page will not auto-close so the user
    /// can see the success message (e.g. when opened from the Tauri desktop app).
    #[serde(default)]
    source: Option<String>,
}

async fn handoff_complete(
    State(deployment): State<DeploymentImpl>,
    Query(query): Query<HandoffCompleteQuery>,
) -> Result<Response<String>, ApiError> {
    if let Some(error) = query.error {
        return Ok(simple_html_response(
            StatusCode::BAD_REQUEST,
            format!("OAuth authorization failed: {error}"),
        ));
    }

    let Some(app_code) = query.app_code.clone() else {
        return Ok(simple_html_response(
            StatusCode::BAD_REQUEST,
            "Missing app_code in callback".to_string(),
        ));
    };

    let (provider, app_verifier) = match deployment.take_oauth_handoff(&query.handoff_id).await {
        Some(state) => state,
        None => {
            tracing::warn!(
                handoff_id = %query.handoff_id,
                "received callback for unknown handoff"
            );
            return Ok(simple_html_response(
                StatusCode::BAD_REQUEST,
                "OAuth handoff not found or already completed".to_string(),
            ));
        }
    };

    let client = deployment.remote_client()?;

    let redeem_request = HandoffRedeemRequest {
        handoff_id: query.handoff_id,
        app_code,
        app_verifier,
    };

    let redeem = client.handoff_redeem(&redeem_request).await?;

    finalize_login(
        &deployment,
        Credentials {
            access_token: Some(redeem.access_token.clone()),
            refresh_token: redeem.refresh_token.clone(),
            expires_at: None,
        },
    )
    .await?;

    let is_desktop = query.source.as_deref() == Some("desktop");
    Ok(close_window_response(
        format!("Signed in with {provider}. You can return to the app."),
        is_desktop,
    ))
}

async fn local_login(
    State(deployment): State<DeploymentImpl>,
    Json(payload): Json<LocalLoginRequest>,
) -> Result<ResponseJson<ApiResponse<ProfileResponse>>, ApiError> {
    let client = deployment.remote_client()?;
    let response = client.local_login(&payload).await?;
    let profile = finalize_login(
        &deployment,
        Credentials {
            access_token: Some(response.access_token),
            refresh_token: response.refresh_token,
            expires_at: None,
        },
    )
    .await?;

    Ok(ResponseJson(ApiResponse::success(profile)))
}

async fn logout(State(deployment): State<DeploymentImpl>) -> Result<StatusCode, ApiError> {
    let auth_context = deployment.auth_context();

    if let Ok(client) = deployment.remote_client() {
        let _ = client.logout().await;
    }

    auth_context.clear_credentials().await.map_err(|e| {
        tracing::error!(?e, "failed to clear credentials");
        ApiError::Io(e)
    })?;

    auth_context.clear_profile().await;

    relay_registration::stop_relay(&deployment).await;

    Ok(StatusCode::NO_CONTENT)
}

async fn status(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<StatusResponse>>, ApiError> {
    use api_types::LoginStatus;

    let login_status = deployment.get_login_status().await;
    let degraded = deployment
        .auth_context()
        .remote_auth_degraded_slug()
        .await
        .map(|_| true);

    match login_status {
        LoginStatus::LoggedOut => Ok(ResponseJson(ApiResponse::success(StatusResponse {
            logged_in: false,
            profile: None,
            degraded,
        }))),
        LoginStatus::LoggedIn { profile } => {
            Ok(ResponseJson(ApiResponse::success(StatusResponse {
                logged_in: true,
                profile,
                degraded,
            })))
        }
    }
}

/// Returns the current access token (auto-refreshes if needed)
async fn get_token(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<TokenResponse>>, ApiError> {
    let remote_client = deployment.remote_client()?;

    // This will auto-refresh the token if expired
    let access_token = remote_client.access_token().await.map_err(ApiError::from)?;

    let creds = deployment.auth_context().get_credentials().await;
    let expires_at = creds.and_then(|c| c.expires_at);

    Ok(ResponseJson(ApiResponse::success(TokenResponse {
        access_token,
        expires_at,
    })))
}

async fn get_current_user(
    State(deployment): State<DeploymentImpl>,
) -> Result<ResponseJson<ApiResponse<CurrentUserResponse>>, ApiError> {
    let remote_client = deployment.remote_client()?;

    // Get the access token from remote client
    let access_token = remote_client.access_token().await.map_err(ApiError::from)?;

    // Extract user ID from the JWT token's 'sub' claim
    let user_id = utils::jwt::extract_subject(&access_token)
        .map_err(|e| {
            tracing::error!("Failed to extract user ID from token: {}", e);
            ApiError::Unauthorized
        })?
        .to_string();

    Ok(ResponseJson(ApiResponse::success(CurrentUserResponse {
        user_id,
    })))
}

fn generate_secret() -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect()
}

async fn finalize_login(
    deployment: &DeploymentImpl,
    mut credentials: Credentials,
) -> Result<ProfileResponse, ApiError> {
    let access_token = credentials
        .access_token
        .as_ref()
        .ok_or_else(|| ApiError::BadRequest("Missing access token".to_string()))?;
    let expires_at = extract_expiration(access_token)
        .map_err(|err| ApiError::BadRequest(format!("Invalid access token: {err}")))?;
    credentials.expires_at = Some(expires_at);

    deployment
        .auth_context()
        .save_credentials(&credentials)
        .await
        .map_err(|e| {
            tracing::error!(?e, "failed to save credentials");
            ApiError::Io(e)
        })?;

    let config_guard = deployment.config().read().await;
    if !config_guard.analytics_enabled {
        let mut new_config = config_guard.clone();
        drop(config_guard);

        new_config.analytics_enabled = true;

        let config_path = config_path();
        if let Err(e) = save_config_to_file(&new_config, &config_path).await {
            tracing::warn!(
                ?e,
                "failed to save config after enabling analytics on login"
            );
        } else {
            let mut config = deployment.config().write().await;
            *config = new_config;
            drop(config);

            tracing::info!("analytics automatically enabled after successful login");

            if let Some(analytics) = deployment.analytics() {
                analytics.track_event(
                    deployment.user_id(),
                    "analytics_session_start",
                    Some(serde_json::json!({})),
                );
            }
        }
    } else {
        drop(config_guard);
    }

    let profile = match deployment.get_login_status().await {
        api_types::LoginStatus::LoggedIn {
            profile: Some(profile),
        } => profile,
        api_types::LoginStatus::LoggedIn { profile: None } | api_types::LoginStatus::LoggedOut => {
            return Err(ApiError::Unauthorized);
        }
    };

    if let Ok(client) = deployment.remote_client() {
        let pool = deployment.db().pool.clone();
        let git = deployment.git().clone();
        tokio::spawn(async move {
            remote_sync::sync_all_linked_workspaces(&client, &pool, &git).await;
        });
    }

    deployment.trigger_pr_sync();

    if let Some(analytics) = deployment.analytics() {
        analytics.track_event(
            deployment.user_id(),
            "$identify",
            Some(serde_json::json!({
                "email": profile.email,
            })),
        );
        analytics.track_event(
            &profile.user_id.to_string(),
            "$merge_dangerously",
            Some(serde_json::json!({
                "alias": deployment.user_id(),
            })),
        );
    }

    let relay_deployment = deployment.clone();
    tokio::spawn(async move {
        relay_registration::spawn_relay(&relay_deployment).await;
    });

    Ok(profile)
}

fn hash_sha256_hex(input: &str) -> String {
    let mut output = String::with_capacity(64);
    let digest = Sha256::digest(input.as_bytes());
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(output, "{:02x}", byte);
    }
    output
}

fn simple_html_response(status: StatusCode, message: String) -> Response<String> {
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>OAuth Error</title>
    {AUTH_PAGE_STYLES}
  </head>
  <body>
    <div class="container">
      <img class="logo" src="data:image/png;base64,{APP_ICON_BASE64}" alt="BetterCoding">
      <div class="content">
        <p class="title">{message}</p>
        <p class="subtitle">Please close this tab and try again.</p>
      </div>
    </div>
  </body>
</html>"#
    );
    Response::builder()
        .status(status)
        .header("content-type", "text/html; charset=utf-8")
        .body(body)
        .unwrap()
}

fn close_window_response(message: String, skip_auto_close: bool) -> Response<String> {
    let script = if skip_auto_close {
        "" // Desktop app: leave the tab open so the user sees the message
    } else {
        "<script>\
           window.addEventListener('load', () => {\
             try { window.close(); } catch (err) {}\
             setTimeout(() => { window.close(); }, 150);\
           });\
         </script>"
    };
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>Authentication Complete</title>
    {script}
    {AUTH_PAGE_STYLES}
  </head>
  <body>
    <div class="container">
      <img class="logo" src="data:image/png;base64,{APP_ICON_BASE64}" alt="BetterCoding">
      <div class="content">
        <p class="title">{message}</p>
        <p class="subtitle">You can close this tab and return to the app.</p>
      </div>
    </div>
  </body>
</html>"#
    );

    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(body)
        .unwrap()
}

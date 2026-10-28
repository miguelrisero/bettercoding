use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use db::models::workspace_input::*;
use deployment::Deployment;
use serde::Deserialize;
use sqlx::SqlitePool;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::DeploymentImpl;

pub const INPUT_CONTENT_CSP: &str = "sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'; navigate-to 'none'; frame-ancestors *";

struct RouteError(InputError);
impl From<InputError> for RouteError {
    fn from(error: InputError) -> Self {
        Self(error)
    }
}
impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            InputError::Sequence(_) => StatusCode::CONFLICT,
            InputError::Code("NOT_FOUND") => StatusCode::NOT_FOUND,
            InputError::Code("FORBIDDEN" | "WORKSPACE_SCOPE") => StatusCode::FORBIDDEN,
            InputError::Code("VERSION_CONFLICT" | "IDEMPOTENCY_CONFLICT") => StatusCode::CONFLICT,
            InputError::Code("PAYLOAD_TOO_LARGE") => StatusCode::PAYLOAD_TOO_LARGE,
            InputError::Database(_) | InputError::Json(_) => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        };
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self.0, "Input request failed");
            "Input storage error".to_string()
        } else {
            self.0.to_string()
        };
        (status, Json(ApiResponse::<()>::error(&message))).into_response()
    }
}
type RouteResult<T> = Result<Json<ApiResponse<T>>, RouteError>;

#[derive(Default, Deserialize)]
struct ReadQuery {
    version: Option<i64>,
    #[serde(default)]
    include_html: bool,
    status: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
    input_id: Option<String>,
}
fn agent(headers: &HeaderMap) -> Result<&str, RouteError> {
    headers
        .get("x-bc-input-requester")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or_else(|| InputError::Code("FORBIDDEN").into())
}

async fn create(
    State(pool): State<SqlitePool>,
    Path(workspace): Path<Uuid>,
    headers: HeaderMap,
    Json(body): Json<CreateInput>,
) -> RouteResult<InputReceipt> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            agent(&headers)?,
            InputWrite::Create(body),
        )
        .await?,
    )))
}
async fn publish(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    headers: HeaderMap,
    Json(body): Json<PublishInput>,
) -> RouteResult<InputReceipt> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&id),
            InputActorKind::AgentMcp,
            agent(&headers)?,
            InputWrite::Publish(body),
        )
        .await?,
    )))
}
async fn agent_status(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    headers: HeaderMap,
    Json(body): Json<InputStatusWrite>,
) -> RouteResult<InputReceipt> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&id),
            InputActorKind::AgentMcp,
            agent(&headers)?,
            InputWrite::Status(body),
        )
        .await?,
    )))
}
async fn answer(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Json(body): Json<AnswerInput>,
) -> RouteResult<InputReceipt> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Answer(body),
        )
        .await?,
    )))
}
async fn close(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Json(body): Json<InputStatusWrite>,
) -> RouteResult<InputReceipt> {
    if body.status != "closed" {
        return Err(InputError::Code("INVALID_TRANSITION").into());
    }
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Status(body),
        )
        .await?,
    )))
}
async fn undo(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Json(body): Json<InputStatusWrite>,
) -> RouteResult<InputReceipt> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Undo(body),
        )
        .await?,
    )))
}
async fn list(
    State(pool): State<SqlitePool>,
    Path(workspace): Path<Uuid>,
    Query(query): Query<ReadQuery>,
) -> RouteResult<InputPage> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::list(
            &pool,
            workspace,
            query.status.as_deref(),
            query.cursor.as_deref().unwrap_or(""),
            query.limit.unwrap_or(50),
        )
        .await?,
    )))
}
async fn detail(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Query(query): Query<ReadQuery>,
) -> RouteResult<InputDetail> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::detail(&pool, workspace, &id, query.version, query.include_html).await?,
    )))
}
async fn events(
    State(pool): State<SqlitePool>,
    Path(workspace): Path<Uuid>,
    Query(query): Query<ReadQuery>,
) -> RouteResult<InputEventPage> {
    let cursor = query
        .cursor
        .as_deref()
        .unwrap_or("0")
        .parse()
        .map_err(|_| InputError::Code("SCHEMA_INVALID"))?;
    Ok(Json(ApiResponse::success(
        WorkspaceInput::events(
            &pool,
            workspace,
            query.input_id.as_deref(),
            cursor,
            query.limit.unwrap_or(100),
        )
        .await?,
    )))
}
async fn verify(
    State(pool): State<SqlitePool>,
    Path((workspace, id)): Path<(Uuid, String)>,
) -> RouteResult<InputVerification> {
    Ok(Json(ApiResponse::success(
        WorkspaceInput::verify(&pool, workspace, &id).await?,
    )))
}
/// Readable defaults for agent-written Input pages. The styles sit in a
/// cascade layer, so any style the page sets itself wins over them. They go
/// at the end of the document so a leading doctype keeps standards mode.
const INPUT_BASE_STYLE: &str = "<style>@layer bc-base{:root{color-scheme:light;background:#fff;color:#1f2328;font:16px/1.65 \"IBM Plex Sans\",system-ui,-apple-system,\"Segoe UI\",Roboto,sans-serif;-webkit-text-size-adjust:100%}body{max-width:46rem;margin:0 auto;padding:2rem 1.5rem 4rem;overflow-wrap:break-word}h1,h2,h3,h4{color:#0d1117;font-weight:600;line-height:1.3;margin:2em 0 .5em;text-wrap:balance}h1{font-size:1.6rem}h2{font-size:1.3rem}h3{font-size:1.1rem}h4{font-size:1rem}body>:first-child{margin-top:0}p,ul,ol,dl,table,pre,blockquote{margin:0 0 1em}p,li{text-wrap:pretty}ul,ol{padding-left:1.4em}li{margin:.3em 0}li::marker{color:#8c959f}b,strong{color:#0d1117;font-weight:600}a{color:#b4530a;text-decoration-thickness:1px;text-underline-offset:2px}a:hover{color:#8a3d06}code,kbd,pre{font-family:\"IBM Plex Mono\",ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.875em}:not(pre)>code{background:#f2f3f5;border-radius:4px;padding:.1em .35em;overflow-wrap:anywhere}pre{background:#f6f8fa;border:1px solid #e4e7eb;border-radius:6px;padding:.85em 1em;overflow-x:auto;line-height:1.5}table{display:block;width:max-content;max-width:100%;overflow-x:auto;border:1px solid #e4e7eb;border-radius:6px;border-collapse:separate;border-spacing:0;font-size:.9rem;line-height:1.45}th,td{border:0;border-bottom:1px solid #e4e7eb;padding:.55em .8em;text-align:left;vertical-align:top}th{background:#f6f8fa;color:#0d1117;font-weight:600}tr:last-child>td{border-bottom:0}blockquote{border-left:3px solid #e4e7eb;padding-left:1em;color:#57606a}hr{border:0;border-top:1px solid #e4e7eb;margin:2em 0}img,video,svg{max-width:100%;height:auto}}</style>";

fn content_response(html: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, INPUT_CONTENT_CSP)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::REFERRER_POLICY, "no-referrer")
        .body(Body::from(html + INPUT_BASE_STYLE))
        .unwrap()
}
async fn content(
    State(pool): State<SqlitePool>,
    Path((workspace, id, version)): Path<(Uuid, String, i64)>,
) -> Result<Response, RouteError> {
    let detail = WorkspaceInput::detail(&pool, workspace, &id, Some(version), true).await?;
    Ok(content_response(detail.version.html.unwrap_or_default()))
}

async fn require_live_workspace(
    State(pool): State<SqlitePool>,
    Path(params): Path<std::collections::HashMap<String, String>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Result<Response, RouteError> {
    let workspace = params
        .get("id")
        .or_else(|| params.get("workspace_id"))
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or(InputError::Code("WORKSPACE_SCOPE"))?;
    let live: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM workspaces WHERE id=?")
        .bind(workspace)
        .fetch_optional(&pool)
        .await
        .map_err(InputError::from)?;
    if live.is_none() {
        return Err(InputError::Code("NOT_FOUND").into());
    }
    Ok(next.run(request).await)
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    let pool = deployment.db().pool.clone();
    input_routes()
        .layer(axum::middleware::from_fn_with_state(
            pool.clone(),
            require_live_workspace,
        ))
        .with_state(pool)
}

fn input_routes() -> Router<SqlitePool> {
    Router::new()
        .route("/", get(list))
        .route("/events", get(events))
        .route("/agent", post(create))
        .route("/{input_id}", get(detail))
        .route("/{input_id}/agent/versions", post(publish))
        .route("/{input_id}/agent/status", post(agent_status))
        .route(
            "/{input_id}/answer",
            post(answer).layer(DefaultBodyLimit::max(super::file_policy::MAX_INPUT_BYTES)),
        )
        .route("/{input_id}/close", post(close))
        .route("/{input_id}/undo-close", post(undo))
        .route("/{input_id}/verify", get(verify))
        .route("/{input_id}/versions/{version}/content", get(content))
        .layer(DefaultBodyLimit::max(MAX_HTML_BYTES + 64 * 1024))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_content_has_exact_isolated_headers() {
        let response = content_response("<script>fetch('/api')</script>".into());
        assert_eq!(
            response.headers()[header::CONTENT_SECURITY_POLICY],
            INPUT_CONTENT_CSP
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(!INPUT_CONTENT_CSP.contains("allow-same-origin"));
        assert!(!INPUT_CONTENT_CSP.contains("allow-modals"));
    }

    #[tokio::test]
    async fn input_content_keeps_the_page_and_appends_layered_base_style() {
        let page = "<!doctype html><p>Choose</p>";
        let body = axum::body::to_bytes(content_response(page.into()).into_body(), usize::MAX)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.starts_with(page));
        assert!(body.ends_with("}</style>"));
        assert!(body[page.len()..].starts_with("<style>@layer bc-base{"));
    }

    async fn test_app() -> (axum::Router, Uuid, SqlitePool) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE workspaces (id BLOB PRIMARY KEY);")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../../db/migrations/20261027090000_workspace_inputs.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let workspace = Uuid::new_v4();
        sqlx::query("INSERT INTO workspaces (id) VALUES (?)")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        (
            axum::Router::new()
                .nest(
                    "/api/workspaces/{workspace_id}/inputs",
                    input_routes().layer(axum::middleware::from_fn_with_state(
                        pool.clone(),
                        require_live_workspace,
                    )),
                )
                .with_state(pool.clone()),
            workspace,
            pool,
        )
    }

    async fn request(
        app: axum::Router,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        use tower::ServiceExt;
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .header("x-bc-input-requester", "test-requester")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            headers,
            serde_json::from_slice(&bytes).unwrap_or_default(),
        )
    }

    #[tokio::test]
    async fn routes_scope_content_answers_and_server_provenance() {
        use serde_json::json;
        let (app, workspace, pool) = test_app().await;
        let root = format!("/api/workspaces/{workspace}/inputs");
        let (status, _, created) = request(app.clone(), "POST", &format!("{root}/agent"), json!({
            "request_id":"create", "expected_sequence":0,"external_key":"decision",
            "content":{"title":"Decision","html":"<p>Choose</p>","contract":{"choices":[],"allow_text":true,"allow_no_answer_yet":true}}
        })).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        let id = created["data"]["input_id"].as_str().unwrap();
        assert_eq!(created["data"]["event"]["actor_kind"], "agent_mcp");
        let (status, headers, _) = request(
            app.clone(),
            "GET",
            &format!("{root}/{id}/versions/1/content"),
            json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_SECURITY_POLICY], INPUT_CONTENT_CSP);
        assert_eq!(headers[header::CACHE_CONTROL], "no-store");
        let foreign = format!("/api/workspaces/{}/inputs/{id}", Uuid::new_v4());
        assert_eq!(
            request(app.clone(), "GET", &foreign, json!(null)).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(
                app.clone(),
                "GET",
                &format!("{foreign}/versions/1/content"),
                json!(null)
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let body = json!({"request_id":"answer","expected_sequence":1,"version":1,"answer":{"text":"yes"}});
        assert_eq!(
            request(
                app.clone(),
                "POST",
                &format!("{root}/{id}/agent/answer"),
                body.clone()
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let mut forged = body.clone();
        forged["actor_kind"] = json!("agent_mcp");
        assert_eq!(
            request(app.clone(), "POST", &format!("{root}/{id}/answer"), forged)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let (status, _, answer) =
            request(app.clone(), "POST", &format!("{root}/{id}/answer"), body).await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(answer["data"]["event"]["actor_kind"], "owner_ui");
        assert_eq!(answer["data"]["event"]["payload"]["answer"]["text"], "yes");
        let (_, _, verified) = request(
            app.clone(),
            "GET",
            &format!("{root}/{id}/verify"),
            json!(null),
        )
        .await;
        assert_eq!(verified["data"]["valid"], true);
        sqlx::query("DELETE FROM workspaces WHERE id=?")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        for suffix in [
            String::new(),
            "/events".into(),
            format!("/{id}"),
            format!("/{id}/verify"),
            format!("/{id}/versions/1/content"),
        ] {
            assert_eq!(
                request(app.clone(), "GET", &format!("{root}{suffix}"), json!(null))
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
        }
        assert!(
            WorkspaceInput::verify(&pool, workspace, id)
                .await
                .unwrap()
                .valid
        );
    }

    #[test]
    fn write_bodies_refuse_actor_kind() {
        assert!(
            serde_json::from_value::<AnswerInput>(serde_json::json!({
                "request_id":"a", "expected_sequence":1, "version":1,
                "answer":{"text":"yes"}, "actor_kind":"owner_ui"
            }))
            .is_err()
        );
    }
}

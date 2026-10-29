use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use chrono::{DateTime, Utc};
use db::models::workspace_report::{
    MAX_REPORT_BODY_BYTES, ReportError, ReportTab, ReportTabs, SetReportTab, WorkspaceReport,
};
use deployment::Deployment;
use serde::{Deserialize, Serialize};
use services::services::events::EventService;
use sqlx::SqlitePool;
use utils::response::ApiResponse;
use uuid::Uuid;

use crate::DeploymentImpl;

struct RouteError(ReportError);
impl From<ReportError> for RouteError {
    fn from(error: ReportError) -> Self {
        Self(error)
    }
}
#[derive(Serialize)]
struct ConflictData {
    current_updated_at: Option<DateTime<Utc>>,
}
impl IntoResponse for RouteError {
    fn into_response(self) -> Response {
        match self.0 {
            ReportError::Conflict(current_updated_at) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"success":false,"data":null,"error_data":ConflictData { current_updated_at },"message":format!("REPORT_CONFLICT: current_updated_at={current_updated_at:?}")})),
            ).into_response(),
            ReportError::Invalid(message) => (StatusCode::BAD_REQUEST, Json(ApiResponse::<()>::error(message))).into_response(),
            error => {
                tracing::error!(%error, "Report storage request failed");
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ApiResponse::<()>::error("Report storage error"))).into_response()
            }
        }
    }
}
type RouteResult<T> = Result<Json<ApiResponse<T>>, RouteError>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteQuery {
    expected_updated_at: Option<DateTime<Utc>>,
}

async fn list(
    State(pool): State<SqlitePool>,
    Path(workspace): Path<Uuid>,
) -> RouteResult<ReportTabs> {
    Ok(Json(ApiResponse::success(
        WorkspaceReport::list(&pool, workspace).await?,
    )))
}
async fn set(
    State(pool): State<SqlitePool>,
    Extension(events): Extension<EventService>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Json(body): Json<SetReportTab>,
) -> RouteResult<ReportTab> {
    let tab = WorkspaceReport::set(&pool, workspace, &id, body).await?;
    events
        .publish_workspace_reports(workspace)
        .await
        .map_err(ReportError::from)?;
    Ok(Json(ApiResponse::success(tab)))
}
async fn delete(
    State(pool): State<SqlitePool>,
    Extension(events): Extension<EventService>,
    Path((workspace, id)): Path<(Uuid, String)>,
    Query(query): Query<DeleteQuery>,
) -> RouteResult<ReportTabs> {
    let state = WorkspaceReport::delete(&pool, workspace, &id, query.expected_updated_at).await?;
    events
        .publish_workspace_reports(workspace)
        .await
        .map_err(ReportError::from)?;
    Ok(Json(ApiResponse::success(state)))
}

pub fn router(deployment: &DeploymentImpl) -> Router<DeploymentImpl> {
    report_routes()
        .layer(Extension(deployment.events().clone()))
        .with_state(deployment.db().pool.clone())
}
fn report_routes() -> Router<SqlitePool> {
    Router::new()
        .route("/", get(list))
        .route("/{tab_id}", put(set).delete(delete))
        .layer(DefaultBodyLimit::max(MAX_REPORT_BODY_BYTES))
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::*;

    async fn request(app: Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), MAX_REPORT_BODY_BYTES * 2)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn report_http_contract_validation_conflict_and_isolation() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../../../db/migrations/20251120000001_refactor_to_scratch.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let workspace = Uuid::new_v4();
        let events = EventService::new(
            db::DBService { pool: pool.clone() },
            std::sync::Arc::new(utils::msg_store::MsgStore::new()),
            std::sync::Arc::new(tokio::sync::RwLock::new(0)),
        );
        let app = Router::new()
            .nest("/api/workspaces/{id}/report-tabs", report_routes())
            .layer(Extension(events))
            .with_state(pool);
        let root = format!("/api/workspaces/{workspace}/report-tabs");
        let path = format!("{root}/build");
        let body = json!({"title":"Build", "columns":["State"], "rows":[[{"text":"<script>x</script>","tone":"ok","url":"https://example.com"}]]});
        let (status, result) = request(app.clone(), "PUT", &path, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["data"]["id"], "build");
        assert_eq!(result["data"]["rows"][0][0]["text"], "<script>x</script>");
        let stamp = result["data"]["updated_at"].as_str().unwrap();
        let (status, list) = request(app.clone(), "GET", &root, json!(null)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["data"]["tabs"][0], result["data"]);
        let foreign = format!("/api/workspaces/{}/report-tabs", Uuid::new_v4());
        assert_eq!(
            request(app.clone(), "GET", &foreign, json!(null)).await.1["data"]["tabs"],
            json!([])
        );
        let mut changed = body.clone();
        changed["title"] = json!("Changed");
        changed["expected_updated_at"] = json!("2000-01-01T00:00:00Z");
        let (status, conflict) = request(app.clone(), "PUT", &path, changed).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(conflict["error_data"]["current_updated_at"], stamp);
        let mut unknown = body.clone();
        unknown["extra"] = json!(true);
        assert_eq!(
            request(app.clone(), "PUT", &path, unknown).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut wrong = body.clone();
        wrong["rows"][0][0]["text"] = json!(3);
        assert_eq!(
            request(app.clone(), "PUT", &path, wrong).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut unsafe_url = body.clone();
        unsafe_url["rows"][0][0]["url"] = json!("javascript:alert(1)");
        assert_eq!(
            request(app.clone(), "PUT", &path, unsafe_url).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(app.clone(), "PUT", &format!("{root}/UPPER"), body)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                app.clone(),
                "PUT",
                &path,
                json!({"title":"x".repeat(MAX_REPORT_BODY_BYTES)})
            )
            .await
            .0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            request(
                app.clone(),
                "DELETE",
                &format!("{path}?unknown=x"),
                json!(null)
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            request(
                app.clone(),
                "DELETE",
                &format!("{path}?expected_updated_at=2000-01-01T00:00:00Z"),
                json!(null)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let encoded = stamp.replace('+', "%2B");
        let (status, deleted) = request(
            app.clone(),
            "DELETE",
            &format!("{path}?expected_updated_at={encoded}"),
            json!(null),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deleted["data"]["tabs"], json!([]));
        assert_eq!(
            request(app, "GET", &root, json!(null)).await.1["data"],
            deleted["data"]
        );
    }
}

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use ts_rs::TS;
use uuid::Uuid;

use super::scratch::{Scratch, ScratchError, ScratchPayload, ScratchType};

// A full tab with JSON-escaped text, URLs and metadata fits within this bound.
pub const MAX_REPORT_BODY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportTone {
    Ok,
    Warn,
    Bad,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReportCell {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tone: Option<ReportTone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetReportTab {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ReportCell>>,
    pub expected_updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReportTab {
    pub id: String,
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<ReportCell>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ReportTabs {
    pub tabs: Vec<ReportTab>,
    // Retain this clock even when the final tab is deleted, preventing ABA retries.
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("REPORT_CONFLICT: current_updated_at={0:?}")]
    Conflict(Option<DateTime<Utc>>),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Scratch(#[from] ScratchError),
}
type Result<T> = std::result::Result<T, ReportError>;

pub fn validate_tab_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(ReportError::Invalid(
            "tab_id must contain 1..64 lowercase ASCII letters, digits, _ or -",
        ));
    }
    Ok(())
}

fn visible(value: &str, max: usize) -> bool {
    value.chars().count() <= max
        && value.chars().any(|c| !c.is_whitespace() && !c.is_control() && !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}'))
        && !value.chars().any(char::is_control)
}

impl SetReportTab {
    pub fn validate(&self, id: &str) -> Result<()> {
        validate_tab_id(id)?;
        if !visible(&self.title, 80) {
            return Err(ReportError::Invalid(
                "title must contain 1..80 characters with visible text and no controls",
            ));
        }
        if self.columns.is_empty()
            || self.columns.len() > 6
            || self.columns.iter().any(|h| !visible(h, 40))
        {
            return Err(ReportError::Invalid(
                "columns must contain 1..6 labels of 1..40 characters with visible text and no controls",
            ));
        }
        if self.rows.len() > 30 || self.rows.iter().any(|row| row.len() != self.columns.len()) {
            return Err(ReportError::Invalid(
                "rows must contain at most 30 rows with one cell per header",
            ));
        }
        for cell in self.rows.iter().flatten() {
            if cell.text.chars().count() > 200 {
                return Err(ReportError::Invalid(
                    "cell text must contain at most 200 Unicode characters",
                ));
            }
            if let Some(raw) = &cell.url {
                let invalid = || {
                    ReportError::Invalid(
                        "cell URL must be absolute HTTP(S), at most 2048 bytes, with a host and no credentials, whitespace or backslashes",
                    )
                };
                if raw.len() > 2048
                    || raw
                        .chars()
                        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
                {
                    return Err(invalid());
                }
                let url = url::Url::parse(raw).map_err(|_| invalid())?;
                let authority = raw
                    .split_once("://")
                    .map(|(_, rest)| rest.split(['/', '?', '#']).next().unwrap_or(""));
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || authority.is_none_or(|a| a.is_empty() || a.contains('@'))
                {
                    return Err(invalid());
                }
            }
        }
        Ok(())
    }
}

pub struct WorkspaceReport;
impl WorkspaceReport {
    pub async fn list(pool: &SqlitePool, workspace: Uuid) -> Result<ReportTabs> {
        match Scratch::find_by_id(pool, workspace, &ScratchType::WorkspaceReports).await? {
            Some(Scratch {
                payload: ScratchPayload::WorkspaceReports(tabs),
                ..
            }) => Ok(tabs),
            None => Ok(ReportTabs::default()),
            _ => Err(ReportError::Invalid("Invalid report storage type")),
        }
    }

    pub async fn set(
        pool: &SqlitePool,
        workspace: Uuid,
        id: &str,
        body: SetReportTab,
    ) -> Result<ReportTab> {
        body.validate(id)?;
        let state = Self::write(
            pool,
            workspace,
            id,
            Some(body.clone()),
            body.expected_updated_at,
        )
        .await?;
        Ok(state
            .tabs
            .into_iter()
            .find(|tab| tab.id == id)
            .expect("set retains the tab"))
    }

    pub async fn delete(
        pool: &SqlitePool,
        workspace: Uuid,
        id: &str,
        expected: Option<DateTime<Utc>>,
    ) -> Result<ReportTabs> {
        validate_tab_id(id)?;
        Self::write(pool, workspace, id, None, expected).await
    }

    async fn write(
        pool: &SqlitePool,
        workspace: Uuid,
        id: &str,
        body: Option<SetReportTab>,
        expected: Option<DateTime<Utc>>,
    ) -> Result<ReportTabs> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let payload: Option<(String,)> = sqlx::query_as(
            "SELECT payload FROM scratch WHERE id=? AND scratch_type='WORKSPACE_REPORTS'",
        )
        .bind(workspace)
        .fetch_optional(&mut *tx)
        .await?;
        let mut state = match payload {
            Some((payload,)) => match serde_json::from_str::<ScratchPayload>(&payload)? {
                ScratchPayload::WorkspaceReports(state) => state,
                _ => return Err(ReportError::Invalid("Invalid report storage type")),
            },
            None => ReportTabs::default(),
        };
        let index = state.tabs.iter().position(|tab| tab.id == id);
        // Equal retries succeed even with the timestamp from before the first write.
        let equal = match (&body, index) {
            (Some(body), Some(i)) => {
                let tab = &state.tabs[i];
                tab.title == body.title && tab.columns == body.columns && tab.rows == body.rows
            }
            (None, None) => expected.is_none(),
            _ => false,
        };
        if equal {
            tx.commit().await?;
            return Ok(state);
        }
        let current = index.map(|i| state.tabs[i].updated_at);
        if expected.is_some() && expected != current {
            return Err(ReportError::Conflict(current));
        }
        if body.is_some() && index.is_none() && state.tabs.len() >= 8 {
            return Err(ReportError::Invalid(
                "A workspace can contain at most 8 report tabs",
            ));
        }
        let now = Utc::now();
        let stamp = state
            .updated_at
            .map_or(now, |last| now.max(last + Duration::microseconds(1)));
        match body {
            Some(body) => {
                let tab = ReportTab {
                    id: id.into(),
                    title: body.title,
                    columns: body.columns,
                    rows: body.rows,
                    created_at: index.map_or(stamp, |i| state.tabs[i].created_at),
                    updated_at: stamp,
                };
                match index {
                    Some(i) => state.tabs[i] = tab,
                    None => state.tabs.push(tab),
                }
            }
            None => {
                if let Some(i) = index {
                    state.tabs.remove(i);
                }
            }
        }
        state.updated_at = Some(stamp);
        let payload = serde_json::to_string(&ScratchPayload::WorkspaceReports(state.clone()))?;
        sqlx::query("INSERT INTO scratch (id,scratch_type,payload,created_at,updated_at) VALUES (?,'WORKSPACE_REPORTS',?,?,?) ON CONFLICT(id,scratch_type) DO UPDATE SET payload=excluded.payload,updated_at=excluded.updated_at")
            .bind(workspace).bind(payload).bind(stamp).bind(stamp).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn body() -> SetReportTab {
        serde_json::from_value(json!({"title":"Build", "columns":["Check"], "rows":[[{"text":"Ready", "tone":"ok", "url":"https://example.com/check"}]]})).unwrap()
    }

    #[test]
    fn report_boundaries_and_unicode() {
        let mut value = body();
        assert!(value.validate("a_0-z").is_ok());
        assert!(value.validate(&"a".repeat(64)).is_ok());
        for id in [
            "".to_string(),
            "a".repeat(65),
            "A".into(),
            "é".into(),
            "a/b".into(),
        ] {
            assert!(value.validate(&id).is_err());
        }
        value.title = "界".repeat(80);
        value.columns = vec!["界".repeat(40); 6];
        value.rows = vec![
            vec![
                ReportCell {
                    text: "😀".repeat(200),
                    tone: None,
                    url: None
                };
                6
            ];
            30
        ];
        assert!(value.validate("a").is_ok());
        value.title.push('界');
        assert!(value.validate("a").is_err());
        value.title.pop();
        value.columns[0].push('界');
        assert!(value.validate("a").is_err());
        value.columns[0].pop();
        value.rows[0][0].text.push('😀');
        assert!(value.validate("a").is_err());
        value.rows[0][0].text.pop();
        value.rows.push(value.rows[0].clone());
        assert!(value.validate("a").is_err());
        value.rows.pop();
        value.columns.push("x".into());
        assert!(value.validate("a").is_err());
        value.columns.pop();
        value.rows[0].pop();
        assert!(value.validate("a").is_err());
        for title in ["", "   ", "\n", "x\0", "\u{200b}"] {
            let mut value = body();
            value.title = title.into();
            assert!(value.validate("a").is_err());
            value = body();
            value.columns[0] = title.into();
            assert!(value.validate("a").is_err());
        }
        value = body();
        value.rows.clear();
        assert!(value.validate("a").is_ok());
        value.columns.clear();
        assert!(value.validate("a").is_err());
    }

    #[test]
    fn report_url_safety_and_byte_limit() {
        for url in [
            "https://example.com",
            "http://localhost:8080/a?q=b#c",
            "https://[::1]/",
        ] {
            let mut value = body();
            value.rows[0][0].url = Some(url.into());
            assert!(value.validate("a").is_ok(), "{url}");
        }
        for url in [
            "javascript:alert(1)",
            "data:text/html,x",
            "file:///x",
            "/relative",
            "//example.com",
            "https://user:pass@example.com",
            "https://@example.com",
            "https:///example.com",
            "https:example.com",
            "https://",
            "https://example.com\n",
            " https://example.com",
            "https://example.com/\\x",
        ] {
            let mut value = body();
            value.rows[0][0].url = Some(url.into());
            assert!(value.validate("a").is_err(), "{url}");
        }
        let mut value = body();
        let prefix = "https://example.com/";
        value.rows[0][0].url = Some(format!("{prefix}{}é", "x".repeat(2048 - prefix.len() - 2)));
        assert!(value.validate("a").is_ok());
        value.rows[0][0].url.as_mut().unwrap().push('x');
        assert!(value.validate("a").is_err());
    }

    #[test]
    fn report_json_rejects_unknown_fields_and_wrong_types() {
        for value in [
            json!({"title":1,"columns":["h"],"rows":[]}),
            json!({"title":"t","columns":[1],"rows":[]}),
            json!({"title":"t","columns":["h"],"rows":[["text"]]}),
            json!({"title":"t","columns":["h"],"rows":[[{"text":1}]]}),
            json!({"title":"t","columns":["h"],"rows":[[{"text":"x","tone":"green"}]]}),
            json!({"title":"t","columns":["h"],"rows":[[{"text":"x","url":1}]]}),
            json!({"title":"t","columns":["h"],"rows":[[{"text":"x","html":"<b>x</b>"}]]}),
            json!({"title":"t","columns":["h"],"rows":[],"html":"x"}),
            json!({"title":"t","columns":["h"],"rows":[],"expected_updated_at":"bad"}),
        ] {
            assert!(serde_json::from_value::<SetReportTab>(value).is_err());
        }
    }

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20251120000001_refactor_to_scratch.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn reports_isolate_workspaces_and_preserve_monotonic_cas_across_delete() {
        let pool = pool().await;
        let workspace = Uuid::new_v4();
        let other = Uuid::new_v4();
        let first = WorkspaceReport::set(&pool, workspace, "build", body())
            .await
            .unwrap();
        assert!(
            WorkspaceReport::list(&pool, other)
                .await
                .unwrap()
                .tabs
                .is_empty()
        );
        assert_eq!(
            WorkspaceReport::set(&pool, workspace, "build", body())
                .await
                .unwrap(),
            first
        );
        let mut next = body();
        next.title = "Changed".into();
        next.expected_updated_at = Some(first.updated_at);
        let second = WorkspaceReport::set(&pool, workspace, "build", next.clone())
            .await
            .unwrap();
        assert!(second.updated_at > first.updated_at);
        assert_eq!(
            WorkspaceReport::set(&pool, workspace, "build", next.clone())
                .await
                .unwrap(),
            second
        );
        next.title = "Stale".into();
        assert!(
            matches!(WorkspaceReport::set(&pool, workspace, "build", next).await, Err(ReportError::Conflict(Some(ts))) if ts == second.updated_at)
        );
        assert!(matches!(
            WorkspaceReport::delete(&pool, workspace, "build", Some(first.updated_at)).await,
            Err(ReportError::Conflict(Some(_)))
        ));
        let empty = WorkspaceReport::delete(&pool, workspace, "build", Some(second.updated_at))
            .await
            .unwrap();
        assert!(empty.tabs.is_empty());
        assert!(empty.updated_at.unwrap() > second.updated_at);
        assert_eq!(
            WorkspaceReport::delete(&pool, workspace, "build", None)
                .await
                .unwrap(),
            empty
        );
        assert!(matches!(
            WorkspaceReport::set(
                &pool,
                workspace,
                "build",
                SetReportTab {
                    expected_updated_at: Some(second.updated_at),
                    ..body()
                }
            )
            .await,
            Err(ReportError::Conflict(None))
        ));
        let recreated = WorkspaceReport::set(&pool, workspace, "build", body())
            .await
            .unwrap();
        assert!(recreated.updated_at > empty.updated_at.unwrap());
        assert!(matches!(
            WorkspaceReport::delete(&pool, other, "build", Some(recreated.updated_at)).await,
            Err(ReportError::Conflict(None))
        ));
    }

    #[tokio::test]
    async fn reports_limit_and_atomic_concurrent_writes() {
        let pool = pool().await;
        let workspace = Uuid::new_v4();
        for i in 0..8 {
            WorkspaceReport::set(&pool, workspace, &format!("tab{i}"), body())
                .await
                .unwrap();
        }
        assert!(matches!(
            WorkspaceReport::set(&pool, workspace, "overflow", body()).await,
            Err(ReportError::Invalid(_))
        ));
        let first = WorkspaceReport::list(&pool, workspace).await.unwrap().tabs[0].clone();
        let mut a = body();
        a.title = "A".into();
        a.expected_updated_at = Some(first.updated_at);
        let mut b = a.clone();
        b.title = "B".into();
        let (a, b) = tokio::join!(
            WorkspaceReport::set(&pool, workspace, &first.id, a),
            WorkspaceReport::set(&pool, workspace, &first.id, b)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(matches!(
            a.err().or_else(|| b.err()),
            Some(ReportError::Conflict(Some(_)))
        ));
        WorkspaceReport::delete(&pool, workspace, &first.id, None)
            .await
            .unwrap();
        WorkspaceReport::set(&pool, workspace, "replacement", body())
            .await
            .unwrap();
        assert_eq!(
            WorkspaceReport::list(&pool, workspace)
                .await
                .unwrap()
                .tabs
                .len(),
            8
        );
        let other = Uuid::new_v4();
        let (a, b) = tokio::join!(
            WorkspaceReport::set(&pool, other, "a", body()),
            WorkspaceReport::set(&pool, other, "b", body())
        );
        assert!(a.is_ok() && b.is_ok());
        assert_eq!(
            WorkspaceReport::list(&pool, other)
                .await
                .unwrap()
                .tabs
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn reports_persist_after_pool_restart_and_serialize_multiple_connections() {
        use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
        let scratch = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../.scratch");
        std::fs::create_dir_all(&scratch).unwrap();
        let directory = tempfile::tempdir_in(scratch).unwrap();
        let options = SqliteConnectOptions::new()
            .filename(directory.path().join("reports.sqlite"))
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20251120000001_refactor_to_scratch.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let workspace = Uuid::new_v4();
        let initial = WorkspaceReport::set(&pool, workspace, "build", body())
            .await
            .unwrap();
        let mut a = body();
        a.title = "A".into();
        a.expected_updated_at = Some(initial.updated_at);
        let mut b = a.clone();
        b.title = "B".into();
        let (a, b) = tokio::join!(
            WorkspaceReport::set(&pool, workspace, "build", a),
            WorkspaceReport::set(&pool, workspace, "build", b)
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(matches!(
            a.err().or_else(|| b.err()),
            Some(ReportError::Conflict(Some(_)))
        ));
        let expected = WorkspaceReport::list(&pool, workspace).await.unwrap();
        pool.close().await;
        let reopened = SqlitePoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        assert_eq!(
            WorkspaceReport::list(&reopened, workspace).await.unwrap(),
            expected
        );
        assert!(
            WorkspaceReport::list(&reopened, Uuid::new_v4())
                .await
                .unwrap()
                .tabs
                .is_empty()
        );
        let empty = WorkspaceReport::delete(&reopened, workspace, "build", None)
            .await
            .unwrap();
        reopened.close().await;
        let reopened = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(
                SqliteConnectOptions::new().filename(directory.path().join("reports.sqlite")),
            )
            .await
            .unwrap();
        assert_eq!(
            WorkspaceReport::list(&reopened, workspace).await.unwrap(),
            empty
        );
        let tab = WorkspaceReport::set(&reopened, workspace, "build", body())
            .await
            .unwrap();
        assert!(tab.updated_at > empty.updated_at.unwrap());
        reopened.close().await;
    }

    #[tokio::test]
    async fn generic_scratch_writes_refuse_reserved_reports() {
        use crate::models::scratch::{
            CreateScratch, Scratch, ScratchPayload, ScratchType, UpdateScratch,
        };
        let pool = pool().await;
        let id = Uuid::new_v4();
        let payload = ScratchPayload::WorkspaceReports(ReportTabs::default());
        assert!(
            Scratch::create(
                &pool,
                id,
                &CreateScratch {
                    payload: payload.clone()
                }
            )
            .await
            .is_err()
        );
        assert!(
            Scratch::update(
                &pool,
                id,
                &ScratchType::WorkspaceReports,
                &UpdateScratch {
                    payload: payload.clone()
                }
            )
            .await
            .is_err()
        );
        assert!(
            Scratch::update(
                &pool,
                id,
                &ScratchType::WorkspaceNotes,
                &UpdateScratch { payload }
            )
            .await
            .is_err()
        );
        WorkspaceReport::set(&pool, id, "a", body()).await.unwrap();
        assert!(
            Scratch::delete(&pool, id, &ScratchType::WorkspaceReports)
                .await
                .is_err()
        );
        assert_eq!(
            WorkspaceReport::list(&pool, id).await.unwrap().tabs.len(),
            1
        );
    }
}

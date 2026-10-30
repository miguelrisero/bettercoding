//! Application-enforced history. A process with SQLite write access can rewrite this ledger.
use chrono::{DateTime, Datelike, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;

fn required_nullable<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}
fn utc_string(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Nanos, true)
}
fn normalize_show_at(value: Option<&str>) -> Result<Option<String>> {
    value
        .map(|value| {
            let parsed = DateTime::parse_from_rfc3339(value)
                .map_err(|_| InputError::Code("SCHEMA_INVALID"))?
                .with_timezone(&Utc);
            // Fixed four-digit years and ordinary seconds keep SQLite text ordering exact.
            if !(1..=9999).contains(&parsed.year()) || parsed.nanosecond() >= 1_000_000_000 {
                return Err(InputError::Code("SCHEMA_INVALID"));
            }
            Ok(utc_string(parsed))
        })
        .transpose()
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputContract {
    pub choices: Vec<InputChoice>,
    pub allow_text: bool,
    pub allow_no_answer_yet: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputAnswer {
    pub choice_id: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputContent {
    pub title: String,
    pub html: String,
    pub contract: InputContract,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateInput {
    /// Omit to preserve the schedule; null clears it. Timestamps require RFC3339 with a timezone.
    #[serde(
        default,
        with = "serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    #[ts(optional, type = "string | null")]
    pub show_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    pub external_key: String,
    pub request_id: String,
    #[ts(type = "number")]
    pub expected_sequence: i64,
    pub content: InputContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishInput {
    /// Omit to preserve the schedule; null clears it. Timestamps require RFC3339 with a timezone.
    #[serde(
        default,
        with = "serde_with::rust::double_option",
        skip_serializing_if = "Option::is_none"
    )]
    #[schemars(with = "Option<String>")]
    #[ts(optional, type = "string | null")]
    pub show_at: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    pub request_id: String,
    #[ts(type = "number")]
    pub expected_sequence: i64,
    pub content: InputContent,
    pub request_answer: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SnoozeInput {
    pub request_id: String,
    #[ts(type = "number")]
    pub expected_sequence: i64,
    #[serde(deserialize_with = "required_nullable")]
    pub show_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    pub request_id: String,
    #[ts(type = "number")]
    pub expected_sequence: i64,
    #[ts(type = "number")]
    pub version: i64,
    pub answer: InputAnswer,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputStatusWrite {
    pub request_id: String,
    #[ts(type = "number")]
    pub expected_sequence: i64,
    pub status: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
pub struct WorkspaceInput {
    pub show_at: Option<String>,
    pub priority: String,
    pub id: String,
    pub workspace_id: Uuid,
    pub external_key: String,
    pub creator: String,
    #[ts(type = "number")]
    pub current_version: i64,
    pub status: String,
    #[ts(type = "number")]
    pub sequence: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InputVersion {
    #[ts(type = "number")]
    pub version: i64,
    pub title: String,
    pub html: Option<String>,
    pub contract: InputContract,
    pub content_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct InputDetail {
    pub input: WorkspaceInput,
    pub version: InputVersion,
    pub answers: Vec<InputEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, TS)]
pub struct InputEvent {
    #[ts(type = "number")]
    pub cursor: i64,
    pub id: String,
    pub workspace_id: Uuid,
    pub input_id: String,
    #[ts(type = "number")]
    pub sequence: i64,
    pub actor_kind: String,
    pub actor: String,
    pub event_type: String,
    #[ts(type = "number")]
    pub version: i64,
    #[sqlx(json)]
    pub payload: Value,
    pub request_id: String,
    pub request_hash: String,
    pub created_at: String,
    pub prev_hash: String,
    pub hash: String,
}

#[derive(Debug, Serialize, Deserialize, TS)]
pub struct InputReceipt {
    pub input_id: String,
    #[ts(type = "number")]
    pub version: i64,
    #[ts(type = "number")]
    pub sequence: i64,
    pub status: String,
    pub event: InputEvent,
    pub owner_url: String,
}

#[derive(Debug, Serialize, TS)]
pub struct InputPage {
    pub inputs: Vec<InputDetail>,
    pub next_cursor: Option<String>,
    #[ts(type = "number")]
    pub pending_count: i64,
}

#[derive(Debug, Serialize, TS)]
pub struct InputEventPage {
    pub events: Vec<InputEvent>,
    #[ts(type = "number")]
    pub next_cursor: i64,
}

#[derive(Debug, Serialize, TS)]
pub struct InputVerification {
    pub valid: bool,
    #[ts(type = "number")]
    pub checked: i64,
    #[ts(type = "number | null")]
    pub first_break: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("{0}")]
    Code(&'static str),
    #[error("SEQUENCE_CONFLICT: current_sequence={0}")]
    Sequence(i64),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
type Result<T> = std::result::Result<T, InputError>;

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputActorKind {
    OwnerUi,
    AgentMcp,
}
impl InputActorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::OwnerUi => "owner_ui",
            Self::AgentMcp => "agent_mcp",
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "operation", content = "body", rename_all = "snake_case")]
pub enum InputWrite {
    Create(CreateInput),
    Publish(PublishInput),
    Answer(AnswerInput),
    Status(InputStatusWrite),
    Undo(InputStatusWrite),
    Snooze(SnoozeInput),
}
impl InputWrite {
    fn write_key(&self) -> (&str, i64) {
        match self {
            Self::Create(v) => (&v.request_id, v.expected_sequence),
            Self::Publish(v) => (&v.request_id, v.expected_sequence),
            Self::Answer(v) => (&v.request_id, v.expected_sequence),
            Self::Snooze(v) => (&v.request_id, v.expected_sequence),
            Self::Status(v) | Self::Undo(v) => (&v.request_id, v.expected_sequence),
        }
    }
}

fn canonical(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut entries: Vec<_> = object.iter().collect();
                entries.sort_by_key(|(key, _)| *key);
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), sorted(v)))
                        .collect(),
                )
            }
            Value::Array(values) => Value::Array(values.iter().map(sorted).collect()),
            value => value.clone(),
        }
    }
    sorted(value).to_string()
}
fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}
fn event_hash(event: &InputEvent) -> String {
    let data = json!({
        "id": event.id, "workspace_id": event.workspace_id, "input_id": event.input_id,
        "sequence": event.sequence, "actor_kind": event.actor_kind, "actor": event.actor,
        "event_type": event.event_type, "version": event.version,
        "payload": event.payload, "request_id": event.request_id,
        "request_hash": event.request_hash, "created_at": event.created_at,
    });
    digest(&format!("{}{}", event.prev_hash, canonical(&data)))
}

impl InputContract {
    fn validate(&self) -> Result<()> {
        let mut ids = std::collections::HashSet::new();
        if self.choices.len() > 100
            || (!self.allow_text && self.choices.is_empty() && !self.allow_no_answer_yet)
        {
            return Err(InputError::Code("SCHEMA_INVALID"));
        }
        for choice in &self.choices {
            if choice.id.is_empty()
                || choice.id.len() > 128
                || choice.label.trim().is_empty()
                || choice.label.len() > 1024
                || !ids.insert(&choice.id)
            {
                return Err(InputError::Code("SCHEMA_INVALID"));
            }
        }
        Ok(())
    }
    pub fn validate_answer(&self, answer: &InputAnswer) -> Result<bool> {
        if serde_json::to_vec(answer)?.len() > MAX_ANSWER_BYTES {
            return Err(InputError::Code("PAYLOAD_TOO_LARGE"));
        }
        if answer
            .choice_id
            .as_ref()
            .is_some_and(|id| !self.choices.iter().any(|c| &c.id == id))
            || (answer.text.as_ref().is_some_and(|text| !text.is_empty()) && !self.allow_text)
        {
            return Err(InputError::Code("ANSWER_INVALID"));
        }
        let empty =
            answer.choice_id.is_none() && answer.text.as_ref().is_none_or(|s| s.trim().is_empty());
        if empty && !self.allow_no_answer_yet {
            return Err(InputError::Code("ANSWER_INVALID"));
        }
        Ok(empty)
    }
}
impl InputContent {
    fn validate(&self) -> Result<()> {
        if self.html.len() > MAX_HTML_BYTES {
            return Err(InputError::Code("PAYLOAD_TOO_LARGE"));
        }
        if self.title.trim().is_empty() || self.title.len() > 512 {
            return Err(InputError::Code("SCHEMA_INVALID"));
        }
        self.contract.validate()
    }
}

fn validate_priority(priority: &str) -> Result<()> {
    if !["P0", "P1", "P2", "P3", "P4"].contains(&priority) {
        return Err(InputError::Code("SCHEMA_INVALID"));
    }
    Ok(())
}

/// Reads a leading "P<n> · " title prefix, the level agents wrote into titles before priority existed.
fn title_priority(title: &str) -> Option<String> {
    let rest = title.strip_prefix('P')?;
    let level = rest.chars().next().filter(|c| ('0'..='4').contains(c))?;
    rest[1..].starts_with(" · ").then(|| format!("P{level}"))
}

impl WorkspaceInput {
    pub async fn set_priority(
        pool: &SqlitePool,
        workspace: Uuid,
        id: &str,
        priority: &str,
    ) -> Result<Self> {
        validate_priority(priority)?;
        let input = sqlx::query_as("UPDATE workspace_inputs SET priority=? WHERE workspace_id=? AND id=? AND status IN ('open','answered') RETURNING *")
            .bind(priority).bind(workspace).bind(id).fetch_optional(pool).await?;
        match input {
            Some(input) => Ok(input),
            None => {
                Self::get(pool, workspace, id).await?;
                Err(InputError::Code("INVALID_TRANSITION"))
            }
        }
    }

    pub async fn get(pool: &SqlitePool, workspace: Uuid, id: &str) -> Result<Self> {
        sqlx::query_as("SELECT * FROM workspace_inputs WHERE workspace_id=? AND id=?")
            .bind(workspace)
            .bind(id)
            .fetch_optional(pool)
            .await?
            .ok_or(InputError::Code("NOT_FOUND"))
    }
    pub async fn detail(
        pool: &SqlitePool,
        workspace: Uuid,
        id: &str,
        version: Option<i64>,
        html: bool,
    ) -> Result<InputDetail> {
        let mut tx = pool.begin().await?;
        let input = sqlx::query_as("SELECT * FROM workspace_inputs WHERE workspace_id=? AND id=?")
            .bind(workspace)
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(InputError::Code("NOT_FOUND"))?;
        let detail = Self::detail_on(&mut tx, input, version, html).await?;
        tx.commit().await?;
        Ok(detail)
    }

    async fn detail_on(
        connection: &mut sqlx::SqliteConnection,
        input: Self,
        version: Option<i64>,
        html: bool,
    ) -> Result<InputDetail> {
        let workspace = input.workspace_id;
        let id = &input.id;
        let version = version.unwrap_or(input.current_version);
        let (title, body, contract, content_hash): (String, String, String, String) = sqlx::query_as(
            "SELECT title,html,contract,content_hash FROM workspace_input_versions WHERE workspace_id=? AND input_id=? AND version=?")
            .bind(workspace).bind(id).bind(version).fetch_optional(&mut *connection).await?
            .ok_or(InputError::Code("NOT_FOUND"))?;
        let answers = sqlx::query_as("SELECT * FROM workspace_input_events WHERE workspace_id=? AND input_id=? AND event_type='answer_submitted' ORDER BY sequence")
            .bind(workspace).bind(id).fetch_all(&mut *connection).await?;
        Ok(InputDetail {
            input,
            version: InputVersion {
                version,
                title,
                html: html.then_some(body),
                contract: serde_json::from_str(&contract)?,
                content_hash,
            },
            answers,
        })
    }
    pub async fn list(
        pool: &SqlitePool,
        workspace: Uuid,
        status: Option<&str>,
        cursor: &str,
        limit: i64,
    ) -> Result<InputPage> {
        let statuses: Vec<_> = status.map(|s| s.split(',').collect()).unwrap_or_default();
        if statuses
            .iter()
            .any(|s| !["open", "answered", "closed", "cancelled"].contains(s))
        {
            return Err(InputError::Code("INVALID_TRANSITION"));
        }
        let filter = statuses.join(",");
        let now = utc_string(Utc::now());
        let mut tx = pool.begin().await?;
        let rows: Vec<WorkspaceInput> = sqlx::query_as("SELECT * FROM workspace_inputs WHERE workspace_id=? AND id>? AND (?='' OR instr(','||?||',', ','||status||',')>0) AND (status!='open' OR show_at IS NULL OR show_at<=?) ORDER BY id LIMIT ?")
            .bind(workspace).bind(cursor).bind(&filter).bind(&filter).bind(&now).bind(limit.clamp(1, 100)+1).fetch_all(&mut *tx).await?;
        let more = rows.len() > limit.clamp(1, 100) as usize;
        let mut inputs = Vec::new();
        for row in rows.into_iter().take(limit.clamp(1, 100) as usize) {
            inputs.push(Self::detail_on(&mut tx, row, None, false).await?);
        }
        let next_cursor = more.then(|| inputs.last().unwrap().input.id.clone());
        let (pending_count,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM workspace_inputs WHERE workspace_id=? AND status='open' AND (show_at IS NULL OR show_at<=?)",
        )
        .bind(workspace)
        .bind(&now)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(InputPage {
            inputs,
            next_cursor,
            pending_count,
        })
    }
    pub async fn events(
        pool: &SqlitePool,
        workspace: Uuid,
        id: Option<&str>,
        cursor: i64,
        limit: i64,
    ) -> Result<InputEventPage> {
        let events: Vec<InputEvent> = sqlx::query_as("SELECT * FROM workspace_input_events WHERE workspace_id=? AND cursor>? AND (? IS NULL OR input_id=?) ORDER BY cursor LIMIT ?")
            .bind(workspace).bind(cursor).bind(id).bind(id).bind(limit.clamp(1,100)).fetch_all(pool).await?;
        let next_cursor = events.last().map_or(cursor, |e| e.cursor);
        Ok(InputEventPage {
            events,
            next_cursor,
        })
    }
    pub async fn verify(pool: &SqlitePool, workspace: Uuid, id: &str) -> Result<InputVerification> {
        let projection = Self::get(pool, workspace, id).await?;
        let events: Vec<InputEvent> = sqlx::query_as("SELECT * FROM workspace_input_events WHERE workspace_id=? AND input_id=? ORDER BY sequence")
            .bind(workspace).bind(id).fetch_all(pool).await?;
        let mut prev = String::new();
        let mut checked = 0;
        for event in events {
            if event.sequence != checked + 1
                || event.prev_hash != prev
                || event.hash != event_hash(&event)
            {
                return Ok(InputVerification {
                    valid: false,
                    checked,
                    first_break: Some(event.sequence),
                });
            }
            checked += 1;
            prev = event.hash;
        }
        let valid = checked == projection.sequence && checked > 0;
        Ok(InputVerification {
            valid,
            checked,
            first_break: (!valid).then_some(checked + 1),
        })
    }
    pub async fn write(
        pool: &SqlitePool,
        workspace: Uuid,
        id: Option<&str>,
        actor_kind: InputActorKind,
        actor: &str,
        write: InputWrite,
    ) -> Result<InputReceipt> {
        let (request_id, expected) = write.write_key();
        if request_id.is_empty() || request_id.len() > 128 || actor.is_empty() || actor.len() > 256
        {
            return Err(InputError::Code("SCHEMA_INVALID"));
        }
        let request_hash = digest(&canonical(
            &json!({"input_id": id, "actor_kind":actor_kind, "actor":actor, "write":write}),
        ));
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let live_workspace: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM workspaces WHERE id=?")
            .bind(workspace)
            .fetch_optional(&mut *tx)
            .await?;
        if live_workspace.is_none() {
            return Err(InputError::Code("NOT_FOUND"));
        }
        if let Some(event) = sqlx::query_as::<_, InputEvent>(
            "SELECT * FROM workspace_input_events WHERE workspace_id=? AND request_id=?",
        )
        .bind(workspace)
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            if event.request_hash != request_hash {
                return Err(InputError::Code("IDEMPOTENCY_CONFLICT"));
            }
            let status = event.payload["status"]
                .as_str()
                .ok_or(InputError::Code("NOT_FOUND"))?
                .to_string();
            return Ok(receipt(event, status));
        }
        let now = utc_string(Utc::now());
        let mut content = None;
        let mut new_generation = false;
        let (mut input, event_type, payload) = match &write {
            InputWrite::Create(request) => {
                if !matches!(actor_kind, InputActorKind::AgentMcp) {
                    return Err(InputError::Code("FORBIDDEN"));
                }
                if expected != 0 {
                    return Err(InputError::Sequence(0));
                }
                if request.external_key.is_empty() || request.external_key.len() > 256 {
                    return Err(InputError::Code("SCHEMA_INVALID"));
                }
                request.content.validate()?;
                if let Some(priority) = &request.priority {
                    validate_priority(priority)?;
                }
                let exists: Option<(String,)> = sqlx::query_as(
                    "SELECT id FROM workspace_inputs WHERE workspace_id=? AND external_key=?",
                )
                .bind(workspace)
                .bind(&request.external_key)
                .fetch_optional(&mut *tx)
                .await?;
                if exists.is_some() {
                    return Err(InputError::Code("IDEMPOTENCY_CONFLICT"));
                }
                new_generation = true;
                let input = Self {
                    show_at: normalize_show_at(
                        request.show_at.as_ref().and_then(|v| v.as_deref()),
                    )?,
                    priority: request
                        .priority
                        .clone()
                        .or_else(|| title_priority(&request.content.title))
                        .unwrap_or_else(|| "P2".into()),
                    id: Uuid::new_v4().to_string(),
                    workspace_id: workspace,
                    external_key: request.external_key.clone(),
                    creator: actor.to_string(),
                    current_version: 1,
                    status: "open".into(),
                    sequence: 0,
                    created_at: now.clone(),
                    updated_at: now.clone(),
                };
                sqlx::query("INSERT INTO workspace_inputs (id,workspace_id,external_key,creator,current_version,status,sequence,created_at,updated_at,priority,show_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)")
                    .bind(&input.id).bind(workspace).bind(&input.external_key).bind(actor).bind(1).bind("open").bind(0).bind(&now).bind(&now).bind(&input.priority).bind(&input.show_at).execute(&mut *tx).await?;
                content = Some(request.content.clone());
                (
                    input,
                    "created",
                    json!({"external_key":request.external_key,"creator":actor,"content":request.content}),
                )
            }
            _ => {
                let id = id.ok_or(InputError::Code("NOT_FOUND"))?;
                let mut input: Self =
                    sqlx::query_as("SELECT * FROM workspace_inputs WHERE workspace_id=? AND id=?")
                        .bind(workspace)
                        .bind(id)
                        .fetch_optional(&mut *tx)
                        .await?
                        .ok_or(InputError::Code("NOT_FOUND"))?;
                if input.sequence != expected {
                    return Err(InputError::Sequence(input.sequence));
                }
                if matches!(actor_kind, InputActorKind::AgentMcp) && input.creator != actor {
                    return Err(InputError::Code("FORBIDDEN"));
                }
                let (event_type, payload) = match &write {
                    InputWrite::Publish(request) => {
                        if !matches!(actor_kind, InputActorKind::AgentMcp) {
                            return Err(InputError::Code("FORBIDDEN"));
                        }
                        if !["open", "answered"].contains(&input.status.as_str()) {
                            return Err(InputError::Code("INVALID_TRANSITION"));
                        }
                        request.content.validate()?;
                        if let Some(priority) = request
                            .priority
                            .clone()
                            .or_else(|| title_priority(&request.content.title))
                        {
                            validate_priority(&priority)?;
                            input.priority = priority;
                        }
                        if let Some(show_at) = &request.show_at {
                            input.show_at = normalize_show_at(show_at.as_deref())?;
                            new_generation = true;
                        }
                        new_generation |= request.request_answer && input.status == "answered";
                        input.current_version += 1;
                        if request.request_answer {
                            input.status = "open".into();
                        }
                        content = Some(request.content.clone());
                        (
                            "version_published",
                            json!({"content":request.content,"request_answer":request.request_answer}),
                        )
                    }
                    InputWrite::Snooze(request) => {
                        if !matches!(actor_kind, InputActorKind::OwnerUi) {
                            return Err(InputError::Code("FORBIDDEN"));
                        }
                        if input.status != "open" {
                            return Err(InputError::Code("INVALID_TRANSITION"));
                        }
                        input.show_at = normalize_show_at(request.show_at.as_deref())?;
                        new_generation = true;
                        ("snoozed", json!({"show_at":input.show_at}))
                    }
                    InputWrite::Answer(request) => {
                        if !matches!(actor_kind, InputActorKind::OwnerUi) {
                            return Err(InputError::Code("FORBIDDEN"));
                        }
                        if request.version != input.current_version {
                            return Err(InputError::Code("VERSION_CONFLICT"));
                        }
                        if input.status != "open" {
                            return Err(InputError::Code("INVALID_TRANSITION"));
                        }
                        let (contract, hash): (String, String) = sqlx::query_as("SELECT contract,content_hash FROM workspace_input_versions WHERE workspace_id=? AND input_id=? AND version=?").bind(workspace).bind(id).bind(input.current_version).fetch_one(&mut *tx).await?;
                        let no_answer = serde_json::from_str::<InputContract>(&contract)?
                            .validate_answer(&request.answer)?;
                        if !no_answer {
                            input.status = "answered".into();
                        }
                        (
                            "answer_submitted",
                            json!({"answer":request.answer,"no_answer_yet":no_answer,"content_hash":hash}),
                        )
                    }
                    InputWrite::Status(request) => {
                        if !["closed", "cancelled"].contains(&request.status.as_str())
                            || !["open", "answered"].contains(&input.status.as_str())
                        {
                            return Err(InputError::Code("INVALID_TRANSITION"));
                        }
                        if request.status == "cancelled"
                            && !matches!(actor_kind, InputActorKind::AgentMcp)
                        {
                            return Err(InputError::Code("FORBIDDEN"));
                        }
                        let prior = input.status.clone();
                        input.status = request.status.clone();
                        (
                            if input.status == "closed" {
                                "closed"
                            } else {
                                "cancelled"
                            },
                            json!({"prior_status":prior,"reason":request.reason}),
                        )
                    }
                    InputWrite::Undo(_) => {
                        if !matches!(actor_kind, InputActorKind::OwnerUi)
                            || input.status != "closed"
                        {
                            return Err(InputError::Code("INVALID_TRANSITION"));
                        }
                        let last: InputEvent = sqlx::query_as("SELECT * FROM workspace_input_events WHERE workspace_id=? AND input_id=? ORDER BY sequence DESC LIMIT 1").bind(workspace).bind(id).fetch_one(&mut *tx).await?;
                        if last.event_type != "closed"
                            || last.actor_kind != "owner_ui"
                            || last.actor != actor
                        {
                            return Err(InputError::Code("FORBIDDEN"));
                        }
                        let payload = last.payload;
                        input.status = payload["prior_status"]
                            .as_str()
                            .filter(|s| ["open", "answered"].contains(s))
                            .ok_or(InputError::Code("INVALID_TRANSITION"))?
                            .to_string();
                        let (title, html, contract): (String, String, String) = sqlx::query_as("SELECT title,html,contract FROM workspace_input_versions WHERE workspace_id=? AND input_id=? AND version=?").bind(workspace).bind(id).bind(input.current_version).fetch_one(&mut *tx).await?;
                        new_generation = input.status == "open";
                        let snapshot = InputContent {
                            title,
                            html,
                            contract: serde_json::from_str(&contract)?,
                        };
                        input.current_version += 1;
                        content = Some(snapshot.clone());
                        (
                            "version_published",
                            json!({"undo_event_id":last.id,"content":snapshot}),
                        )
                    }
                    InputWrite::Create(_) => unreachable!(),
                };
                (input, event_type, payload)
            }
        };
        input.sequence += 1;
        let mut payload = payload;
        payload["status"] = json!(input.status);
        payload["show_at"] = json!(input.show_at);
        if let Some(content) = content {
            let hash = digest(&canonical(&serde_json::to_value(&content)?));
            sqlx::query("INSERT INTO workspace_input_versions (workspace_id,input_id,version,title,html,contract,content_hash) VALUES (?,?,?,?,?,?,?)")
                .bind(workspace).bind(&input.id).bind(input.current_version).bind(&content.title).bind(&content.html).bind(canonical(&serde_json::to_value(&content.contract)?)).bind(hash).execute(&mut *tx).await?;
        }
        let previous: Option<(String,)> = sqlx::query_as("SELECT hash FROM workspace_input_events WHERE workspace_id=? AND input_id=? ORDER BY sequence DESC LIMIT 1").bind(workspace).bind(&input.id).fetch_optional(&mut *tx).await?;
        let mut event = InputEvent {
            cursor: 0,
            id: Uuid::new_v4().to_string(),
            workspace_id: workspace,
            input_id: input.id.clone(),
            sequence: input.sequence,
            actor_kind: actor_kind.as_str().into(),
            actor: actor.into(),
            event_type: event_type.into(),
            version: input.current_version,
            payload,
            request_id: request_id.into(),
            request_hash,
            created_at: now.clone(),
            prev_hash: previous.map_or_else(String::new, |p| p.0),
            hash: String::new(),
        };
        insert_event(&mut tx, &mut event).await?;
        sqlx::query("UPDATE workspace_inputs SET current_version=?,status=?,sequence=?,updated_at=?,priority=?,show_at=? WHERE workspace_id=? AND id=?")
            .bind(input.current_version).bind(&input.status).bind(input.sequence).bind(&now).bind(&input.priority).bind(&input.show_at).bind(workspace).bind(&input.id).execute(&mut *tx).await?;
        if new_generation || input.status != "open" {
            sqlx::query("UPDATE workspace_input_availability SET finished_at=? WHERE workspace_id=? AND input_id=? AND finished_at IS NULL")
                .bind(&now).bind(workspace).bind(&input.id).execute(&mut *tx).await?;
        }
        if new_generation {
            sqlx::query("UPDATE workspace_inputs SET availability_generation=? WHERE workspace_id=? AND id=?")
                .bind(&event.id).bind(workspace).bind(&input.id).execute(&mut *tx).await?;
            if input.status == "open" {
                let available = input
                    .show_at
                    .as_deref()
                    .is_none_or(|time| time <= now.as_str());
                sqlx::query("INSERT INTO workspace_input_availability(generation,workspace_id,input_id,available_event_id) VALUES (?,?,?,?)")
                    .bind(&event.id).bind(workspace).bind(&input.id).bind(available.then_some(&event.id)).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(receipt(event, input.status))
    }
}
#[derive(Debug, FromRow)]
pub struct InputNotificationClaim {
    pub generation: String,
    pub workspace_id: Uuid,
    pub token: String,
}

async fn insert_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    event: &mut InputEvent,
) -> Result<()> {
    event.hash = event_hash(event);
    event.cursor = sqlx::query("INSERT INTO workspace_input_events (id,workspace_id,input_id,sequence,actor_kind,actor,event_type,version,payload,request_id,request_hash,created_at,prev_hash,hash) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
        .bind(&event.id).bind(event.workspace_id).bind(&event.input_id).bind(event.sequence).bind(&event.actor_kind).bind(&event.actor).bind(&event.event_type).bind(event.version).bind(canonical(&event.payload)).bind(&event.request_id).bind(&event.request_hash).bind(&event.created_at).bind(&event.prev_hash).bind(&event.hash).execute(&mut **tx).await?.last_insert_rowid();
    Ok(())
}

impl WorkspaceInput {
    /// Release at most 100 generations per tick, including overdue work after restart.
    pub async fn release_due(pool: &SqlitePool) -> Result<usize> {
        let now = utc_string(Utc::now());
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let due: Vec<(String, Uuid, String, i64, i64)> = sqlx::query_as("SELECT a.generation,i.workspace_id,i.id,i.sequence,i.current_version FROM workspace_input_availability a JOIN workspace_inputs i ON i.id=a.input_id AND i.workspace_id=a.workspace_id JOIN workspaces w ON w.id=i.workspace_id WHERE a.generation=i.availability_generation AND a.available_event_id IS NULL AND a.finished_at IS NULL AND i.status='open' AND (i.show_at IS NULL OR i.show_at<=?) ORDER BY i.show_at,i.id LIMIT 100")
            .bind(&now).fetch_all(&mut *tx).await?;
        for (generation, workspace, id, sequence, version) in &due {
            let (previous,): (String,) = sqlx::query_as("SELECT hash FROM workspace_input_events WHERE workspace_id=? AND input_id=? ORDER BY sequence DESC LIMIT 1")
                .bind(workspace).bind(id).fetch_one(&mut *tx).await?;
            let mut event = InputEvent {
                cursor: 0,
                id: Uuid::new_v4().to_string(),
                workspace_id: *workspace,
                input_id: id.clone(),
                sequence: sequence + 1,
                actor_kind: "system".into(),
                actor: "input_availability".into(),
                event_type: "available".into(),
                version: *version,
                payload: json!({"status":"open", "generation":generation}),
                request_id: format!("availability:{}", Uuid::new_v4()),
                request_hash: digest(&canonical(&json!({"generation":generation}))),
                created_at: now.clone(),
                prev_hash: previous,
                hash: String::new(),
            };
            insert_event(&mut tx, &mut event).await?;
            sqlx::query(
                "UPDATE workspace_inputs SET sequence=?,updated_at=? WHERE workspace_id=? AND id=?",
            )
            .bind(event.sequence)
            .bind(&now)
            .bind(workspace)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE workspace_input_availability SET available_event_id=? WHERE generation=?",
            )
            .bind(&event.id)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(due.len())
    }

    /// One atomic, expiring claim. Competing workers cannot own the same token.
    pub async fn claim_notification(pool: &SqlitePool) -> Result<Option<InputNotificationClaim>> {
        let time = Utc::now();
        let now = utc_string(time);
        let until = utc_string(time + chrono::Duration::seconds(60));
        let token = Uuid::new_v4().to_string();
        let claim = sqlx::query_as("UPDATE workspace_input_availability SET claim_token=?,claim_until=? WHERE generation=(SELECT a.generation FROM workspace_input_availability a JOIN workspace_inputs i ON i.id=a.input_id AND i.workspace_id=a.workspace_id JOIN workspaces w ON w.id=i.workspace_id WHERE a.generation=i.availability_generation AND a.available_event_id IS NOT NULL AND a.finished_at IS NULL AND (a.claim_until IS NULL OR a.claim_until<=?) AND i.status='open' AND (i.show_at IS NULL OR i.show_at<=?) ORDER BY a.generation LIMIT 1) RETURNING generation,workspace_id,claim_token AS token")
            .bind(&token).bind(until).bind(&now).bind(&now).fetch_optional(pool).await?;
        Ok(claim)
    }

    /// Recheck immediately before the external call. OS display remains best effort.
    pub async fn notification_is_current(
        pool: &SqlitePool,
        claim: &InputNotificationClaim,
    ) -> Result<bool> {
        let now = utc_string(Utc::now());
        let live: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM workspace_input_availability a JOIN workspace_inputs i ON i.id=a.input_id AND i.workspace_id=a.workspace_id JOIN workspaces w ON w.id=i.workspace_id WHERE a.generation=? AND a.claim_token=? AND a.claim_until>? AND a.finished_at IS NULL AND a.available_event_id IS NOT NULL AND a.generation=i.availability_generation AND i.status='open' AND (i.show_at IS NULL OR i.show_at<=?)")
            .bind(&claim.generation).bind(&claim.token).bind(&now).bind(&now).fetch_optional(pool).await?;
        Ok(live.is_some())
    }

    pub async fn finish_notification(
        pool: &SqlitePool,
        claim: &InputNotificationClaim,
    ) -> Result<bool> {
        let now = utc_string(Utc::now());
        Ok(sqlx::query("UPDATE workspace_input_availability SET finished_at=?,claim_token=NULL,claim_until=NULL WHERE generation=? AND claim_token=? AND claim_until>? AND finished_at IS NULL")
            .bind(&now).bind(&claim.generation).bind(&claim.token).bind(&now).execute(pool).await?.rows_affected()==1)
    }
}

fn receipt(event: InputEvent, status: String) -> InputReceipt {
    InputReceipt {
        input_id: event.input_id.clone(),
        version: event.version,
        sequence: event.sequence,
        status,
        owner_url: format!(
            "/workspaces/{}#input={}",
            event.workspace_id, event.input_id
        ),
        event,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_presence_and_timestamp_contract() {
        let legacy =
            json!({"external_key":"k","request_id":"r","expected_sequence":0,"content":content()});
        let request: CreateInput = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), legacy);
        let mut explicit = legacy;
        explicit["show_at"] = Value::Null;
        let request: CreateInput = serde_json::from_value(explicit.clone()).unwrap();
        assert_eq!(request.show_at, Some(None));
        assert_eq!(serde_json::to_value(request).unwrap(), explicit);
        assert_eq!(
            normalize_show_at(Some("2026-11-02T09:00:00+01:00"))
                .unwrap()
                .as_deref(),
            Some("2026-11-02T08:00:00.000000000Z")
        );
        for value in [
            "2026-11-02T08:00:00",
            "invalid",
            "2026-02-30T08:00:00Z",
            "2026-11-02T08:00:60Z",
            "9999-12-31T23:59:59-01:00",
        ] {
            assert!(normalize_show_at(Some(value)).is_err(), "{value}");
        }
        assert!(
            serde_json::from_value::<SnoozeInput>(json!({"request_id":"s","expected_sequence":1}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<SnoozeInput>(
                json!({"request_id":"s","expected_sequence":1,"show_at":null})
            )
            .is_ok()
        );
    }

    #[tokio::test]
    async fn scheduled_visibility_snooze_and_exact_retry() {
        let (pool, workspace) = setup().await;
        let mut request = match create() {
            InputWrite::Create(r) => r,
            _ => unreachable!(),
        };
        request.show_at = Some(Some("2099-11-02T09:00:00+01:00".into()));
        let created = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request.clone()),
        )
        .await
        .unwrap();
        let retry = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(&created).unwrap(),
            serde_json::to_value(retry).unwrap()
        );
        let detail = WorkspaceInput::detail(&pool, workspace, &created.input_id, None, true)
            .await
            .unwrap();
        assert_eq!(
            detail.input.show_at.as_deref(),
            Some("2099-11-02T08:00:00.000000000Z")
        );
        for status in [None, Some("open"), Some("open,answered")] {
            let page = WorkspaceInput::list(&pool, workspace, status, "", 1)
                .await
                .unwrap();
            assert!(page.inputs.is_empty());
            assert_eq!(page.pending_count, 0);
        }
        // An omitted publish field preserves the schedule and its delivery generation.
        let published = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            publish(1),
        )
        .await
        .unwrap();
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .show_at,
            detail.input.show_at
        );
        let published_hash =
            WorkspaceInput::detail(&pool, workspace, &created.input_id, None, true)
                .await
                .unwrap()
                .version
                .content_hash;
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::OwnerUi,
                "owner",
                InputWrite::Snooze(SnoozeInput {
                    request_id: "stale".into(),
                    expected_sequence: 1,
                    show_at: None
                })
            )
            .await,
            Err(InputError::Sequence(2))
        ));
        let snooze = || {
            InputWrite::Snooze(SnoozeInput {
                request_id: "snooze".into(),
                expected_sequence: published.sequence,
                show_at: None,
            })
        };
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::AgentMcp,
                "requester",
                snooze()
            )
            .await,
            Err(InputError::Code("FORBIDDEN"))
        ));
        let shown = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            snooze(),
        )
        .await
        .unwrap();
        assert_eq!(shown.version, published.version);
        assert_eq!(shown.event.actor_kind, "owner_ui");
        assert_eq!(shown.event.event_type, "snoozed");
        assert!(shown.event.payload.get("answer").is_none());
        assert_eq!(
            WorkspaceInput::list(&pool, workspace, None, "", 1)
                .await
                .unwrap()
                .pending_count,
            1
        );
        assert_eq!(
            WorkspaceInput::detail(&pool, workspace, &created.input_id, None, true)
                .await
                .unwrap()
                .version
                .content_hash,
            published_hash
        );
        let claim = WorkspaceInput::claim_notification(&pool)
            .await
            .unwrap()
            .unwrap();
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            answer(shown.sequence, shown.version),
        )
        .await
        .unwrap();
        assert!(
            !WorkspaceInput::notification_is_current(&pool, &claim)
                .await
                .unwrap()
        );
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::OwnerUi,
                "owner",
                InputWrite::Snooze(SnoozeInput {
                    request_id: "answered-snooze".into(),
                    expected_sequence: shown.sequence + 1,
                    show_at: None
                })
            )
            .await,
            Err(InputError::Code("INVALID_TRANSITION"))
        ));
        let retry = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            snooze(),
        )
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(shown).unwrap(),
            serde_json::to_value(retry).unwrap()
        );
        assert!(
            WorkspaceInput::verify(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .valid
        );
    }

    #[tokio::test]
    async fn future_no_answer_preserves_due_notification() {
        let (pool, workspace) = setup().await;
        let mut request = match create() {
            InputWrite::Create(r) => r,
            _ => unreachable!(),
        };
        request.show_at = Some(Some("2099-01-01T00:00:00Z".into()));
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
        let receipt = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Answer(AnswerInput {
                request_id: "no-answer".into(),
                expected_sequence: 1,
                version: 1,
                answer: InputAnswer::default(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(receipt.status, "open");
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 0);
        sqlx::query("UPDATE workspace_inputs SET show_at='2000-01-01T00:00:00.000000000Z'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 1);
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_some()
        );
        let detail = WorkspaceInput::detail(&pool, workspace, &created.input_id, None, false)
            .await
            .unwrap();
        assert_eq!(detail.input.current_version, 1);
        assert_eq!(detail.answers.len(), 1);
        assert_eq!(detail.answers[0].payload["no_answer_yet"], true);
        assert!(
            WorkspaceInput::verify(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .valid
        );
    }

    #[tokio::test]
    async fn concurrent_snooze_lists_keep_rows_and_count_in_one_snapshot() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let mut sequence = 1;
        for n in 0..20 {
            let show_at = (n % 2 == 0).then(|| "2099-01-01T00:00:00Z".into());
            let (page, receipt) = tokio::join!(
                WorkspaceInput::list(&pool, workspace, None, "", 1),
                WorkspaceInput::write(
                    &pool,
                    workspace,
                    Some(&created.input_id),
                    InputActorKind::OwnerUi,
                    "owner",
                    InputWrite::Snooze(SnoozeInput {
                        request_id: format!("snooze-{n}"),
                        expected_sequence: sequence,
                        show_at,
                    })
                )
            );
            let page = page.unwrap();
            sequence = receipt.unwrap().sequence;
            assert_eq!(page.inputs.len() as i64, page.pending_count);
            assert!(
                page.inputs
                    .iter()
                    .all(|detail| detail.input.show_at.is_none())
            );
        }
    }

    #[tokio::test]
    async fn publish_null_legacy_hash_and_invalid_schedule_are_atomic() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let legacy = json!({"request_id":"publish-legacy","expected_sequence":1,"content":content(),"request_answer":false});
        let published = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(serde_json::from_value(legacy.clone()).unwrap()),
        )
        .await
        .unwrap();
        assert_eq!(
            published.event.request_hash,
            digest(&canonical(
                &json!({"input_id":created.input_id,"actor_kind":"agent_mcp","actor":"requester","write":{"operation":"publish","body":legacy}})
            ))
        );
        let mut request: PublishInput = serde_json::from_value(json!({"request_id":"scheduled","expected_sequence":2,"content":content(),"request_answer":false,"show_at":"2099-01-01T01:00:00+01:00"})).unwrap();
        let scheduled = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(request.clone()),
        )
        .await
        .unwrap();
        request.show_at = Some(Some("2099-01-01T00:00:00Z".into()));
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Publish(request.clone())
            )
            .await,
            Err(InputError::Code("IDEMPOTENCY_CONFLICT"))
        ));
        request.request_id = "invalid".into();
        request.expected_sequence = scheduled.sequence;
        request.show_at = Some(Some("invalid".into()));
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Publish(request.clone())
            )
            .await,
            Err(InputError::Code("SCHEMA_INVALID"))
        ));
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .sequence,
            scheduled.sequence
        );
        request.request_id = "clear".into();
        request.show_at = Some(None);
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(request),
        )
        .await
        .unwrap();
        assert!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .show_at
                .is_none()
        );
    }

    #[tokio::test]
    async fn availability_release_claim_recovery_and_suppression() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let first = WorkspaceInput::claim_notification(&pool)
            .await
            .unwrap()
            .unwrap();
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        // A new worker sees durable expired work and receives a different claim token.
        sqlx::query(
            "UPDATE workspace_input_availability SET claim_until='2000-01-01T00:00:00.000000000Z'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let recovered = WorkspaceInput::claim_notification(&pool)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(first.token, recovered.token);
        assert!(
            !WorkspaceInput::finish_notification(&pool, &first)
                .await
                .unwrap()
        );
        assert!(
            WorkspaceInput::finish_notification(&pool, &recovered)
                .await
                .unwrap()
        );
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        let scheduled = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Snooze(SnoozeInput {
                request_id: "later".into(),
                expected_sequence: 1,
                show_at: Some("2099-01-01T00:00:00Z".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 0);
        // Advance only the retained in-memory fixture's schedule to simulate downtime.
        sqlx::query("UPDATE workspace_inputs SET show_at='2000-01-01T00:00:00.000000000Z'")
            .execute(&pool)
            .await
            .unwrap();
        let (a, b) = tokio::join!(
            WorkspaceInput::release_due(&pool),
            WorkspaceInput::release_due(&pool)
        );
        assert_eq!(a.unwrap() + b.unwrap(), 1);
        let input = WorkspaceInput::get(&pool, workspace, &created.input_id)
            .await
            .unwrap();
        assert_eq!(input.sequence, scheduled.sequence + 1);
        assert_eq!(input.current_version, 1);
        let events = WorkspaceInput::events(&pool, workspace, Some(&created.input_id), 0, 100)
            .await
            .unwrap()
            .events;
        assert_eq!(events.last().unwrap().actor_kind, "system");
        assert_eq!(events.last().unwrap().event_type, "available");
        assert!(events.last().unwrap().payload.get("answer").is_none());
        let (a, b) = tokio::join!(
            WorkspaceInput::claim_notification(&pool),
            WorkspaceInput::claim_notification(&pool)
        );
        assert_eq!(
            usize::from(a.unwrap().is_some()) + usize::from(b.unwrap().is_some()),
            1
        );
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            status(input.sequence, "closed"),
        )
        .await
        .unwrap();
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            WorkspaceInput::verify(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .valid
        );
    }

    #[tokio::test]
    async fn rescheduling_cancellation_and_workspace_loss_suppress_claims() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let old = WorkspaceInput::claim_notification(&pool)
            .await
            .unwrap()
            .unwrap();
        let rescheduled = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Snooze(SnoozeInput {
                request_id: "reschedule".into(),
                expected_sequence: 1,
                show_at: Some("2099-01-01T00:00:00Z".into()),
            }),
        )
        .await
        .unwrap();
        assert!(
            !WorkspaceInput::notification_is_current(&pool, &old)
                .await
                .unwrap()
        );
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            status(rescheduled.sequence, "cancelled"),
        )
        .await
        .unwrap();
        sqlx::query("UPDATE workspace_inputs SET show_at=NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 0);
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        let mut request = match create() {
            InputWrite::Create(r) => r,
            _ => unreachable!(),
        };
        request.external_key = "second".into();
        request.request_id = "second".into();
        let second = WorkspaceInput::write(
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
        sqlx::query("DELETE FROM workspaces WHERE id=?")
            .bind(workspace)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            !WorkspaceInput::notification_is_current(&pool, &claim)
                .await
                .unwrap()
        );
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 0);
        assert!(
            WorkspaceInput::verify(&pool, workspace, &second.input_id)
                .await
                .unwrap()
                .valid
        );
    }

    #[tokio::test]
    async fn migration_preserves_history_high_water_and_has_no_backlog() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE workspaces(id BLOB PRIMARY KEY); INSERT INTO workspaces VALUES (X'01');").execute(&pool).await.unwrap();
        for migration in [
            include_str!("../../migrations/20261027090000_workspace_inputs.sql"),
            include_str!("../../migrations/20261028120000_workspace_input_priority.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        }
        sqlx::raw_sql("INSERT INTO workspace_inputs VALUES ('legacy',X'01','key','agent',1,'open',1,'old','old','P2'); INSERT INTO workspace_input_events VALUES (7,'event',X'01','legacy',1,'agent_mcp','agent','created',1,'{}','request','request-hash','old','','hash'); UPDATE sqlite_sequence SET seq=77 WHERE name='workspace_input_events';").execute(&pool).await.unwrap();
        let before: (i64, String, String, String) =
            sqlx::query_as("SELECT cursor,payload,request_hash,hash FROM workspace_input_events")
                .fetch_one(&pool)
                .await
                .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261030120000_workspace_input_schedule.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        let after: (i64, String, String, String) =
            sqlx::query_as("SELECT cursor,payload,request_hash,hash FROM workspace_input_events")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(before, after);
        let (high_water,): (i64,) =
            sqlx::query_as("SELECT seq FROM sqlite_sequence WHERE name='workspace_input_events'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(high_water, 77);
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM workspace_input_availability")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(WorkspaceInput::release_due(&pool).await.unwrap(), 0);
        assert!(
            WorkspaceInput::claim_notification(&pool)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn scheduling_preserves_answered_status_and_filters_before_pagination() {
        let (pool, workspace) = setup().await;
        for n in 0..4 {
            let mut request = match create() {
                InputWrite::Create(r) => r,
                _ => unreachable!(),
            };
            request.request_id = format!("create-{n}");
            request.external_key = request.request_id.clone();
            request.show_at = if n < 2 {
                Some(Some("2099-01-01T00:00:00Z".into()))
            } else {
                Some(Some("2000-01-01T00:00:00Z".into()))
            };
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
        let first = WorkspaceInput::list(&pool, workspace, None, "", 1)
            .await
            .unwrap();
        assert_eq!(first.inputs.len(), 1);
        assert_eq!(first.pending_count, 2);
        let second = WorkspaceInput::list(
            &pool,
            workspace,
            None,
            first.next_cursor.as_deref().unwrap(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(second.inputs.len(), 1);
        assert!(second.next_cursor.is_none());
        let id = &first.inputs[0].input.id;
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        let mut request = match publish(2) {
            InputWrite::Publish(r) => r,
            _ => unreachable!(),
        };
        request.request_answer = false;
        request.show_at = Some(Some("2099-01-01T00:00:00Z".into()));
        let receipt = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(request),
        )
        .await
        .unwrap();
        assert_eq!(receipt.status, "answered");
        assert_eq!(
            WorkspaceInput::detail(&pool, workspace, id, None, false)
                .await
                .unwrap()
                .answers
                .len(),
            1
        );
        assert_eq!(
            WorkspaceInput::list(&pool, workspace, Some("answered"), "", 100)
                .await
                .unwrap()
                .inputs
                .len(),
            1
        );
    }

    async fn setup() -> (SqlitePool, Uuid) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE workspaces (id BLOB PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261027090000_workspace_inputs.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261028120000_workspace_input_priority.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261030120000_workspace_input_schedule.sql"
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
        (pool, workspace)
    }
    #[tokio::test]
    async fn title_prefix_sets_priority_and_backfills_open_inputs() {
        assert_eq!(title_priority("P3 · Ship"), Some("P3".into()));
        assert_eq!(title_priority("P5 · Ship"), None);
        assert_eq!(title_priority("P1: Ship"), None);
        let (pool, workspace) = setup().await;
        let make = |key: &str, title: &str, priority: Option<&str>| {
            let mut content = content();
            content.title = title.into();
            InputWrite::Create(CreateInput {
                show_at: None,
                priority: priority.map(Into::into),
                external_key: key.into(),
                request_id: key.into(),
                expected_sequence: 0,
                content,
            })
        };
        let write = |write| {
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                write,
            )
        };
        let priority = |id: String| {
            let pool = pool.clone();
            async move {
                WorkspaceInput::get(&pool, workspace, &id)
                    .await
                    .unwrap()
                    .priority
            }
        };
        let derived = write(make("derived", "P3 · Ship", None)).await.unwrap();
        assert_eq!(priority(derived.input_id.clone()).await, "P3");
        let explicit = write(make("explicit", "P3 · Ship", Some("P1")))
            .await
            .unwrap();
        assert_eq!(priority(explicit.input_id).await, "P1");
        let mut content = content();
        content.title = "P0 · Ship now".into();
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&derived.input_id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(PublishInput {
                show_at: None,
                priority: None,
                request_id: "republish".into(),
                expected_sequence: 1,
                content,
                request_answer: false,
            }),
        )
        .await
        .unwrap();
        assert_eq!(priority(derived.input_id).await, "P0");

        // Inputs written before the fallback existed: stored P2 under a prefixed title.
        let open = write(make("open", "P4 · Later", Some("P2"))).await.unwrap();
        let closed = write(make("closed", "P0 · Done", Some("P2")))
            .await
            .unwrap();
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&closed.input_id),
            InputActorKind::AgentMcp,
            "requester",
            status(1, "closed"),
        )
        .await
        .unwrap();
        sqlx::raw_sql(include_str!(
            "../../migrations/20261028130000_workspace_input_priority_from_title.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(priority(open.input_id).await, "P4");
        assert_eq!(priority(closed.input_id).await, "P2");
    }
    #[tokio::test]
    async fn workspace_input_priority_defaults_updates_and_legacy_retry() {
        let (pool, workspace) = setup().await;
        let legacy = json!({"external_key":"legacy","request_id":"legacy-create","expected_sequence":0,"content":content()});
        let request: CreateInput = serde_json::from_value(legacy.clone()).unwrap();
        let created = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .priority,
            "P2"
        );
        let old_hash = digest(&canonical(
            &json!({"input_id":null,"actor_kind":"agent_mcp","actor":"requester","write":{"operation":"create","body":legacy}}),
        ));
        assert_eq!(created.event.request_hash, old_hash);
        let retry = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request),
        )
        .await
        .unwrap();
        assert_eq!(retry.event.id, created.event.id);

        let mut request = match create() {
            InputWrite::Create(request) => request,
            _ => unreachable!(),
        };
        request.priority = Some("P1".into());
        let created = WorkspaceInput::write(
            &pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Create(request.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .priority,
            "P1"
        );
        let legacy_publish = json!({"request_id":"publish-1","expected_sequence":1,"content":content(),"request_answer":true});
        let published = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            publish(1),
        )
        .await
        .unwrap();
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .priority,
            "P1"
        );
        assert_eq!(
            published.event.request_hash,
            digest(&canonical(
                &json!({"input_id":created.input_id,"actor_kind":"agent_mcp","actor":"requester","write":{"operation":"publish","body":legacy_publish}})
            ))
        );
        let retry = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            publish(1),
        )
        .await
        .unwrap();
        assert_eq!(retry.event.id, published.event.id);
        let mut update = match publish(2) {
            InputWrite::Publish(request) => request,
            _ => unreachable!(),
        };
        update.priority = Some("invalid".into());
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Publish(update.clone())
            )
            .await,
            Err(InputError::Code("SCHEMA_INVALID"))
        ));
        update.priority = Some("P0".into());
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            InputWrite::Publish(update),
        )
        .await
        .unwrap();
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, &created.input_id)
                .await
                .unwrap()
                .priority,
            "P0"
        );
        request.external_key = "invalid".into();
        request.request_id = "invalid".into();
        request.priority = Some("P5".into());
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Create(request)
            )
            .await,
            Err(InputError::Code("SCHEMA_INVALID"))
        ));
    }

    fn content() -> InputContent {
        InputContent {
            title: "Choose the rollout".into(),
            html: "<h1>Rollout</h1>".into(),
            contract: InputContract {
                choices: vec![InputChoice {
                    id: "ship".into(),
                    label: "Ship".into(),
                }],
                allow_text: true,
                allow_no_answer_yet: true,
            },
        }
    }
    fn create() -> InputWrite {
        InputWrite::Create(CreateInput {
            show_at: None,
            priority: None,
            external_key: "rollout".into(),
            request_id: "create".into(),
            expected_sequence: 0,
            content: content(),
        })
    }
    async fn seed(pool: &SqlitePool, workspace: Uuid) -> InputReceipt {
        WorkspaceInput::write(
            pool,
            workspace,
            None,
            InputActorKind::AgentMcp,
            "requester",
            create(),
        )
        .await
        .unwrap()
    }
    fn answer(sequence: i64, version: i64) -> InputWrite {
        InputWrite::Answer(AnswerInput {
            request_id: format!("answer-{sequence}-{version}"),
            expected_sequence: sequence,
            version,
            answer: InputAnswer {
                choice_id: Some("ship".into()),
                text: Some("Go".into()),
            },
        })
    }
    fn status(sequence: i64, status: &str) -> InputWrite {
        InputWrite::Status(InputStatusWrite {
            request_id: format!("{status}-{sequence}"),
            expected_sequence: sequence,
            status: status.into(),
            reason: None,
        })
    }
    fn publish(sequence: i64) -> InputWrite {
        InputWrite::Publish(PublishInput {
            show_at: None,
            priority: None,
            request_id: format!("publish-{sequence}"),
            expected_sequence: sequence,
            content: content(),
            request_answer: true,
        })
    }

    #[tokio::test]
    async fn atomic_append_rolls_back_projection_and_version_on_event_failure() {
        let (pool, workspace) = setup().await;
        sqlx::raw_sql("CREATE TRIGGER fail_event BEFORE INSERT ON workspace_input_events BEGIN SELECT RAISE(ABORT, 'failure'); END;").execute(&pool).await.unwrap();
        assert!(
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                create()
            )
            .await
            .is_err()
        );
        for table in [
            "workspace_inputs",
            "workspace_input_versions",
            "workspace_input_events",
        ] {
            let (count,): (i64,) = sqlx::query_as(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(count, 0);
        }
    }
    #[tokio::test]
    async fn hash_chain_detects_first_tampered_event_and_rows_are_immutable() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let id = &created.input_id;
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        assert!(
            WorkspaceInput::verify(&pool, workspace, id)
                .await
                .unwrap()
                .valid
        );
        assert!(
            sqlx::query("UPDATE workspace_input_events SET actor='forged'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM workspace_input_events")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("UPDATE workspace_input_versions SET html='forged'")
                .execute(&pool)
                .await
                .is_err()
        );
        assert!(
            sqlx::query("DELETE FROM workspace_input_versions")
                .execute(&pool)
                .await
                .is_err()
        );
        sqlx::query("DROP TRIGGER workspace_input_events_no_update")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspace_input_events SET actor='forged' WHERE sequence=2")
            .execute(&pool)
            .await
            .unwrap();
        let result = WorkspaceInput::verify(&pool, workspace, id).await.unwrap();
        assert!(!result.valid);
        assert_eq!(result.first_break, Some(2));
        assert_eq!(result.checked, 1);
    }
    #[tokio::test]
    async fn request_id_retries_return_original_receipt_and_reject_different_write() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let retry = seed(&pool, workspace).await;
        assert_eq!(created.event.id, retry.event.id);
        let id = &created.input_id;
        let first = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        let retry = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        assert_eq!(first.event.id, retry.event.id);
        let mut changed = match create() {
            InputWrite::Create(c) => c,
            _ => unreachable!(),
        };
        changed.content.title = "Different".into();
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Create(changed)
            )
            .await,
            Err(InputError::Code("IDEMPOTENCY_CONFLICT"))
        ));
        let mut duplicate = match create() {
            InputWrite::Create(c) => c,
            _ => unreachable!(),
        };
        duplicate.request_id = "different-id".into();
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                None,
                InputActorKind::AgentMcp,
                "requester",
                InputWrite::Create(duplicate)
            )
            .await,
            Err(InputError::Code("IDEMPOTENCY_CONFLICT"))
        ));
    }
    #[tokio::test]
    async fn expected_sequence_serializes_competing_answers() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let id = &created.input_id;
        let left = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        );
        let mut other = match answer(1, 1) {
            InputWrite::Answer(a) => a,
            _ => unreachable!(),
        };
        other.request_id = "other".into();
        let right = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            InputWrite::Answer(other),
        );
        let (a, b) = tokio::join!(left, right);
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert!(
            matches!(a, Err(InputError::Sequence(2))) || matches!(b, Err(InputError::Sequence(2)))
        );
    }
    #[tokio::test]
    async fn stale_version_and_agent_answers_are_refused() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let id = &created.input_id;
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::AgentMcp,
            "requester",
            publish(1),
        )
        .await
        .unwrap();
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(id),
                InputActorKind::OwnerUi,
                "owner",
                answer(2, 1)
            )
            .await,
            Err(InputError::Code("VERSION_CONFLICT"))
        ));
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(id),
                InputActorKind::AgentMcp,
                "requester",
                answer(2, 2)
            )
            .await,
            Err(InputError::Code("FORBIDDEN"))
        ));
        assert_eq!(
            WorkspaceInput::get(&pool, workspace, id)
                .await
                .unwrap()
                .sequence,
            2
        );
    }
    #[tokio::test]
    async fn transitions_close_undo_followup_cancel_preserve_history() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let id = &created.input_id;
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(id),
                InputActorKind::OwnerUi,
                "owner",
                answer(2, 1)
            )
            .await,
            Err(InputError::Code("INVALID_TRANSITION"))
        ));
        let closed = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            status(2, "closed"),
        )
        .await
        .unwrap();
        let undo = InputWrite::Undo(InputStatusWrite {
            request_id: "undo".into(),
            expected_sequence: closed.sequence,
            status: "closed".into(),
            reason: None,
        });
        let undone = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            undo,
        )
        .await
        .unwrap();
        assert_eq!(undone.status, "answered");
        assert_eq!(undone.version, 2);
        let published = WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::AgentMcp,
            "requester",
            publish(4),
        )
        .await
        .unwrap();
        assert_eq!(published.status, "open");
        assert_eq!(published.version, 3);
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(id),
                InputActorKind::OwnerUi,
                "owner",
                status(5, "cancelled")
            )
            .await,
            Err(InputError::Code("FORBIDDEN"))
        ));
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::AgentMcp,
            "requester",
            status(5, "cancelled"),
        )
        .await
        .unwrap();
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(id),
                InputActorKind::AgentMcp,
                "requester",
                publish(6)
            )
            .await,
            Err(InputError::Code("INVALID_TRANSITION"))
        ));
        assert_eq!(
            WorkspaceInput::detail(&pool, workspace, id, Some(1), false)
                .await
                .unwrap()
                .answers
                .len(),
            1
        );
        assert!(
            WorkspaceInput::verify(&pool, workspace, id)
                .await
                .unwrap()
                .valid
        );
    }
    #[tokio::test]
    async fn workspace_scope_requester_ownership_and_event_cursor_are_enforced() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let foreign = Uuid::new_v4();
        assert!(matches!(
            WorkspaceInput::detail(&pool, foreign, &created.input_id, None, true).await,
            Err(InputError::Code("NOT_FOUND"))
        ));
        assert!(
            WorkspaceInput::events(&pool, foreign, None, 0, 100)
                .await
                .unwrap()
                .events
                .is_empty()
        );
        assert!(
            WorkspaceInput::list(&pool, foreign, None, "", 50)
                .await
                .unwrap()
                .inputs
                .is_empty()
        );
        assert!(matches!(
            WorkspaceInput::write(
                &pool,
                workspace,
                Some(&created.input_id),
                InputActorKind::AgentMcp,
                "other",
                status(1, "closed")
            )
            .await,
            Err(InputError::Code("FORBIDDEN"))
        ));
        let first = WorkspaceInput::events(&pool, workspace, None, 0, 1)
            .await
            .unwrap();
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::AgentMcp,
            "requester",
            status(1, "closed"),
        )
        .await
        .unwrap();
        let second = WorkspaceInput::events(&pool, workspace, None, first.next_cursor, 1)
            .await
            .unwrap();
        assert_eq!(second.events.len(), 1);
        assert_eq!(second.events[0].event_type, "closed");
        assert!(second.next_cursor > first.next_cursor);
    }
    #[tokio::test]
    async fn answered_inputs_leave_pending_and_remain_in_answered_history() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let before = WorkspaceInput::list(&pool, workspace, Some("open"), "", 50)
            .await
            .unwrap();
        assert_eq!(before.pending_count, 1);
        assert_eq!(before.inputs.len(), 1);
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        let pending = WorkspaceInput::list(&pool, workspace, Some("open"), "", 50)
            .await
            .unwrap();
        assert_eq!(pending.pending_count, before.pending_count - 1);
        assert!(pending.inputs.is_empty());
        let answered = WorkspaceInput::list(&pool, workspace, Some("answered"), "", 50)
            .await
            .unwrap();
        assert_eq!(answered.pending_count, 0);
        assert_eq!(answered.inputs.len(), 1);
        assert_eq!(answered.inputs[0].input.id, created.input_id);
        assert_eq!(answered.inputs[0].input.status, "answered");
        assert_eq!(answered.inputs[0].answers.len(), 1);
    }
    #[tokio::test]
    async fn no_answer_yet_stays_open_and_invalid_contract_answers_fail() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let empty = InputWrite::Answer(AnswerInput {
            request_id: "empty".into(),
            expected_sequence: 1,
            version: 1,
            answer: InputAnswer::default(),
        });
        let result = WorkspaceInput::write(
            &pool,
            workspace,
            Some(&created.input_id),
            InputActorKind::OwnerUi,
            "owner",
            empty,
        )
        .await
        .unwrap();
        assert_eq!(result.status, "open");
        assert_eq!(result.event.payload["no_answer_yet"], true);
        let pending = WorkspaceInput::list(&pool, workspace, Some("open"), "", 50)
            .await
            .unwrap();
        assert_eq!(pending.pending_count, 1);
        assert_eq!(pending.inputs.len(), 1);
        assert_eq!(pending.inputs[0].input.id, created.input_id);
        let contract = InputContract {
            choices: vec![],
            allow_text: false,
            allow_no_answer_yet: false,
        };
        assert!(contract.validate_answer(&InputAnswer::default()).is_err());
        assert!(
            contract
                .validate_answer(&InputAnswer {
                    choice_id: Some("invented".into()),
                    text: None
                })
                .is_err()
        );
        assert!(
            contract
                .validate_answer(&InputAnswer {
                    choice_id: None,
                    text: Some("yes".into())
                })
                .is_err()
        );
    }
    #[tokio::test]
    async fn workspace_deletion_retains_verifiable_history_and_refuses_every_write() {
        let (pool, workspace) = setup().await;
        let created = seed(&pool, workspace).await;
        let id = &created.input_id;
        WorkspaceInput::write(
            &pool,
            workspace,
            Some(id),
            InputActorKind::OwnerUi,
            "owner",
            answer(1, 1),
        )
        .await
        .unwrap();
        let before = WorkspaceInput::detail(&pool, workspace, id, Some(1), true)
            .await
            .unwrap();
        let events_before = WorkspaceInput::events(&pool, workspace, None, 0, 100)
            .await
            .unwrap();
        assert_eq!(
            sqlx::query("DELETE FROM workspaces WHERE id=?")
                .bind(workspace)
                .execute(&pool)
                .await
                .unwrap()
                .rows_affected(),
            1
        );
        let retained = WorkspaceInput::detail(&pool, workspace, id, Some(1), true)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(retained).unwrap()
        );
        let events_after = WorkspaceInput::events(&pool, workspace, None, 0, 100)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(events_before).unwrap(),
            serde_json::to_value(events_after).unwrap()
        );
        assert!(
            WorkspaceInput::verify(&pool, workspace, id)
                .await
                .unwrap()
                .valid
        );
        let undo = InputWrite::Undo(InputStatusWrite {
            request_id: "deleted-undo".into(),
            expected_sequence: 2,
            status: "closed".into(),
            reason: None,
        });
        for write in [
            create(),
            publish(2),
            answer(2, 1),
            status(2, "closed"),
            undo,
        ] {
            assert!(matches!(
                WorkspaceInput::write(
                    &pool,
                    workspace,
                    Some(id),
                    InputActorKind::OwnerUi,
                    "owner",
                    write
                )
                .await,
                Err(InputError::Code("NOT_FOUND"))
            ));
        }
        assert!(
            sqlx::query("UPDATE workspace_inputs SET workspace_id=? WHERE id=?")
                .bind(Uuid::new_v4())
                .bind(id)
                .execute(&pool)
                .await
                .is_err()
        );
    }

    #[test]
    fn canonical_hash_ignores_object_insertion_order() {
        let a: Value = serde_json::from_str(r#"{"b":1,"a":{"y":2,"x":3}}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a":{"x":3,"y":2},"b":1}"#).unwrap();
        assert_eq!(canonical(&a), canonical(&b));
    }
}

//! Utilities for reading and writing external agent config files (not the server's own config).
//!
//! These helpers abstract over the JSON (Claude Code) and TOML (Codex) config formats.

use std::{collections::HashMap, sync::LazyLock};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::fs;
use ts_rs::TS;

use crate::executors::{CodingAgent, ExecutorError};

static DEFAULT_MCP_JSON: &str = include_str!("../default_mcp.json");
pub static PRECONFIGURED_MCP_SERVERS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str::<Value>(DEFAULT_MCP_JSON).expect("Failed to parse default MCP JSON")
});

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
pub struct McpConfig {
    servers: HashMap<String, serde_json::Value>,
    pub servers_path: Vec<String>,
    pub template: serde_json::Value,
    pub preconfigured: serde_json::Value,
    pub is_toml_config: bool,
}

impl McpConfig {
    pub fn new(
        servers_path: Vec<String>,
        template: serde_json::Value,
        preconfigured: serde_json::Value,
        is_toml_config: bool,
    ) -> Self {
        Self {
            servers: HashMap::new(),
            servers_path,
            template,
            preconfigured,
            is_toml_config,
        }
    }
    pub fn set_servers(&mut self, servers: HashMap<String, serde_json::Value>) {
        self.servers = servers;
    }
}

pub async fn read_agent_config(
    config_path: &std::path::Path,
    mcp_config: &McpConfig,
) -> Result<Value, ExecutorError> {
    if let Ok(file_content) = fs::read_to_string(config_path).await {
        if mcp_config.is_toml_config {
            if file_content.trim().is_empty() {
                return Ok(serde_json::json!({}));
            }
            let toml_val: toml::Value = toml::from_str(&file_content)?;
            let json_string = serde_json::to_string(&toml_val)?;
            Ok(serde_json::from_str(&json_string)?)
        } else {
            Ok(serde_json::from_str(&file_content)?)
        }
    } else {
        Ok(mcp_config.template.clone())
    }
}

pub async fn write_agent_config(
    config_path: &std::path::Path,
    mcp_config: &McpConfig,
    config: &Value,
) -> Result<(), ExecutorError> {
    if mcp_config.is_toml_config {
        let toml_value: toml::Value = serde_json::from_str(&serde_json::to_string(config)?)?;
        let toml_content = toml::to_string_pretty(&toml_value)?;
        fs::write(config_path, toml_content).await?;
    } else {
        let json_content = serde_json::to_string_pretty(config)?;
        fs::write(config_path, json_content).await?;
    }
    Ok(())
}

type ServerMap = Map<String, Value>;

fn is_http_server(s: &Map<String, Value>) -> bool {
    matches!(s.get("type").and_then(Value::as_str), Some("http"))
}

fn is_stdio(s: &Map<String, Value>) -> bool {
    !is_http_server(s) && s.get("command").is_some()
}

fn extract_meta(mut obj: ServerMap) -> (ServerMap, Option<Value>) {
    let meta = obj.remove("meta");
    (obj, meta)
}

fn attach_meta(mut obj: ServerMap, meta: Option<Value>) -> Value {
    if let Some(m) = meta {
        obj.insert("meta".to_string(), m);
    }
    Value::Object(obj)
}

// --- Adapters ---------------------------------------------------------------

fn adapt_passthrough(servers: ServerMap, meta: Option<Value>) -> Value {
    attach_meta(servers, meta)
}

fn adapt_codex(mut servers: ServerMap, mut meta: Option<Value>) -> Value {
    servers.retain(|_, v| v.as_object().map(is_stdio).unwrap_or(false));

    if let Some(Value::Object(ref mut m)) = meta {
        m.retain(|k, _| servers.contains_key(k));
        servers.insert("meta".to_string(), Value::Object(std::mem::take(m)));
        meta = None; // already attached above
    }
    attach_meta(servers, meta)
}

enum Adapter {
    Passthrough,
    Codex,
}

fn apply_adapter(adapter: Adapter, canonical: Value) -> Value {
    let (servers_only, meta) = match canonical.as_object() {
        Some(map) => extract_meta(map.clone()),
        None => (ServerMap::new(), None),
    };

    match adapter {
        Adapter::Passthrough => adapt_passthrough(servers_only, meta),
        Adapter::Codex => adapt_codex(servers_only, meta),
    }
}

impl CodingAgent {
    pub fn preconfigured_mcp(&self) -> Value {
        use Adapter::*;

        let adapter = match self {
            CodingAgent::ClaudeCode(_) => Passthrough,
            CodingAgent::Codex(_) => Codex,
            #[cfg(feature = "qa-mode")]
            CodingAgent::QaMock(_) => Passthrough, // QA mock doesn't need MCP
        };

        let canonical = PRECONFIGURED_MCP_SERVERS.clone();
        apply_adapter(adapter, canonical)
    }
}

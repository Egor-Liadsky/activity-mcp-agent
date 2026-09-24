//! MCP-обработчик: инструменты `activity_*` поверх хранилища.
//!
//! Ответы — JSON-текст: его одинаково удобно разбирать клиенту (показ
//! сводки, пометка прочитанной) и читать модели в чате. Описания
//! инструментов и схем — на английском, как у `git-mcp`: их читает модель.

use crate::collector::Collector;
use crate::digest::Digester;
use crate::store::{DigestRow, Store};
use crate::time;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

pub type ToolResult = Result<String, String>;

/// Сводок в ответе по умолчанию.
const DEFAULT_DIGEST_LIMIT: u32 = 5;
/// Событий в ответе по умолчанию и предел.
const DEFAULT_CHANGES_LIMIT: u32 = 50;
const MAX_CHANGES_LIMIT: u32 = 500;

fn default_true() -> bool {
    true
}

fn default_digest_limit() -> u32 {
    DEFAULT_DIGEST_LIMIT
}

fn default_changes_limit() -> u32 {
    DEFAULT_CHANGES_LIMIT
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DigestArgs {
    /// Return only digests that have not been acknowledged yet.
    #[serde(default = "default_true")]
    unread_only: bool,
    /// Maximum number of digests, newest first.
    #[serde(default = "default_digest_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AckArgs {
    /// Digest id from activity_digest.
    id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ChangesArgs {
    /// Project name (as in activity_projects) or absolute path; all projects if omitted.
    #[serde(default)]
    project: Option<String>,
    /// Start of the period: relative ("24h", "7d", "2w"), a date ("2026-09-01") or RFC 3339. Default: 7d.
    #[serde(default)]
    since: Option<String>,
    /// Maximum number of events, newest first.
    #[serde(default = "default_changes_limit")]
    limit: u32,
}

/// Общие части демона, которые нужны обработчику. Обработчик создаётся на
/// каждую HTTP-сессию, поэтому всё тяжёлое — за `Arc`.
pub struct Shared {
    pub store: Store,
    pub digester: Digester,
    /// Нет у `--stdio`-экземпляра: он читает базу демона и репозитории не
    /// опрашивает.
    pub collector: Option<Arc<Collector>>,
}

#[derive(Clone)]
pub struct ActivityServer {
    shared: Arc<Shared>,
    tool_router: ToolRouter<Self>,
}

fn internal(err: impl std::fmt::Display) -> String {
    format!("ошибка хранилища: {err}")
}

fn digest_json(digest: &DigestRow) -> Value {
    json!({
        "id": digest.id,
        "period_from": time::rfc3339(digest.period_from),
        "period_to": time::rfc3339(digest.period_to),
        "created_at": time::rfc3339(digest.created_at),
        "acked": digest.acked_at.is_some(),
        "text": digest.text,
    })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[tool_router]
impl ActivityServer {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Returns scheduled activity digests of the watched projects directory, newest first")]
    async fn activity_digest(&self, Parameters(args): Parameters<DigestArgs>) -> ToolResult {
        let limit = args.limit.clamp(1, 50) as i64;
        let digests = self.shared.store.digests(args.unread_only, limit).await.map_err(internal)?;
        let list: Vec<Value> = digests.iter().map(digest_json).collect();
        Ok(pretty(&json!({ "digests": list })))
    }

    #[tool(description = "Marks a digest as read so it is no longer returned with unread_only")]
    async fn activity_ack(&self, Parameters(args): Parameters<AckArgs>) -> ToolResult {
        if self.shared.store.ack_digest(args.id, time::now()).await.map_err(internal)? {
            Ok(format!("Digest {} acknowledged", args.id))
        } else {
            Err(format!("сводки {} нет", args.id))
        }
    }

    #[tool(description = "Lists watched projects with current branch, uncommitted files and last activity")]
    async fn activity_projects(&self) -> ToolResult {
        let projects = self.shared.store.projects().await.map_err(internal)?;
        let list: Vec<Value> = projects
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "path": p.path,
                    "branch": p.head_branch,
                    "uncommitted_files": p.dirty_files,
                    "uncommitted_sample": p.dirty_sample,
                    "last_activity": p.last_activity.map(time::rfc3339),
                    "removed": p.removed_at.is_some(),
                })
            })
            .collect();
        Ok(pretty(&json!({ "projects": list })))
    }

    #[tool(description = "Lists recorded changes (commits, branches, checkouts, added or removed projects), newest first")]
    async fn activity_changes(&self, Parameters(args): Parameters<ChangesArgs>) -> ToolResult {
        let now = time::now();
        let since = time::parse_since(args.since.as_deref().unwrap_or("7d"), now)?;
        let project_id = match args.project.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
            Some(name) => Some(
                self.shared
                    .store
                    .find_project(name)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| format!("проект «{name}» не найден: список — activity_projects"))?
                    .id,
            ),
            None => None,
        };
        let limit = args.limit.clamp(1, MAX_CHANGES_LIMIT) as i64;
        let events = self
            .shared
            .store
            .recent_events(project_id, since, limit)
            .await
            .map_err(internal)?;
        let list: Vec<Value> = events
            .iter()
            .map(|event| {
                let mut item = json!({
                    "project": event.project,
                    "kind": event.kind,
                    "seen_at": time::rfc3339(event.seen_at),
                });
                if let (Some(item), Some(payload)) = (item.as_object_mut(), event.payload.as_object()) {
                    for (key, value) in payload {
                        item.entry(key.clone()).or_insert_with(|| value.clone());
                    }
                }
                item
            })
            .collect();
        Ok(pretty(&json!({ "since": time::rfc3339(since), "changes": list })))
    }

    #[tool(description = "Builds a digest right now, outside the schedule, covering everything since the previous digest")]
    async fn activity_build_digest(&self) -> ToolResult {
        let now = time::now();
        if let Some(collector) = &self.shared.collector {
            collector.scan_all(now).await;
        }
        match self.shared.digester.build(now).await.map_err(internal)? {
            Some(digest) => Ok(pretty(&json!({ "digest": digest_json(&digest) }))),
            None => Ok(pretty(&json!({ "digest": null, "note": "no changes since the previous digest" }))),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ActivityServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Activity of git projects in a watched directory: scheduled digests, projects and recorded changes.",
            )
    }
}

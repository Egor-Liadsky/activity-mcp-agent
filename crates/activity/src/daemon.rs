//! Демон: фоновые циклы (наблюдение, опрос, расписание, ретенция) и
//! HTTP-маршруты MCP.
//!
//! Сервер работает постоянно, а клиенты приходят и уходят, поэтому
//! транспорт — Streamable HTTP, а не stdio: при stdio процесс сервера
//! порождает клиент, и сервер живёт ровно столько, сколько клиент.

use crate::collector::Collector;
use crate::digest::Digester;
use crate::discovery::Walker;
use crate::schedule::Schedule;
use crate::server::{ActivityServer, Shared};
use crate::store::Store;
use crate::time;
use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use notify::{RecursiveMode, Watcher};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Изменения файлов копятся столько и опрашиваются пачкой: сохранение
/// файла в редакторе или `git commit` дают десятки событий подряд.
const DEBOUNCE: Duration = Duration::from_secs(10);
/// Сон планировщика порциями: монотонные часы на macOS стоят, пока машина
/// спит, и один длинный `sleep` проспал бы время запуска.
const SCHEDULER_STEP: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct Options {
    pub roots: Vec<PathBuf>,
    pub schedule: Schedule,
    pub rescan_interval: Duration,
    pub max_depth: usize,
    pub excludes: Vec<String>,
    pub retention: Duration,
    /// `false` — без `notify`, только опрос по таймеру.
    pub watch: bool,
    /// Требуемый bearer-токен для `/mcp`.
    pub token: Option<String>,
}

pub struct Daemon {
    shared: Arc<Shared>,
    collector: Arc<Collector>,
    options: Options,
}

impl Daemon {
    pub fn new(store: Store, options: Options) -> Arc<Self> {
        let collector = Arc::new(Collector::new(
            store.clone(),
            Walker::new(options.max_depth, &options.excludes),
            options.roots.clone(),
        ));
        let shared = Arc::new(Shared {
            digester: Digester::new(store.clone()),
            store,
            collector: Some(collector.clone()),
        });
        Arc::new(Self {
            shared,
            collector,
            options,
        })
    }

    /// Первый обход и фоновые циклы. Первый обход идёт до возврата:
    /// к моменту, когда HTTP начнёт принимать запросы, проекты уже известны.
    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let events = self.collector.rescan(time::now()).await?;
        let projects = self.shared.store.active_projects().await?.len();
        tracing::info!(projects, events, "первый обход завершён");
        tokio::spawn(self.clone().collect_loop());
        tokio::spawn(self.clone().schedule_loop());
        Ok(())
    }

    /// Файловые события и полный опрос по таймеру.
    async fn collect_loop(self: Arc<Self>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<PathBuf>();
        // Наблюдатель живёт, пока жив цикл: уничтожение останавливает его.
        let _watcher = if self.options.watch { self.watch(tx) } else { None };
        let mut pending: HashSet<PathBuf> = HashSet::new();
        let mut debounce = tokio::time::interval(DEBOUNCE);
        let mut rescan = tokio::time::interval(self.options.rescan_interval);
        // Первый тик срабатывает сразу, а первый обход уже сделан в `start`.
        rescan.tick().await;
        loop {
            tokio::select! {
                Some(path) = rx.recv() => {
                    if self.collector.walker().is_relevant(&path) {
                        pending.insert(path);
                    }
                }
                _ = debounce.tick(), if !pending.is_empty() => {
                    let paths = std::mem::take(&mut pending);
                    if let Err(err) = self.collector.scan_paths(&paths, time::now()).await {
                        tracing::warn!("опрос изменённых проектов не удался: {err:#}");
                    }
                }
                _ = rescan.tick() => {
                    let now = time::now();
                    if let Err(err) = self.collector.rescan(now).await {
                        tracing::warn!("полный обход не удался: {err:#}");
                    }
                    let before = now - self.options.retention.as_secs() as i64;
                    match self.shared.store.prune(before).await {
                        Ok(0) => {}
                        Ok(removed) => tracing::info!(removed, "удалены записи старше срока хранения"),
                        Err(err) => tracing::warn!("очистка не удалась: {err}"),
                    }
                }
            }
        }
    }

    /// Рекурсивное наблюдение за корнями. Неудача (на Linux — предел
    /// `inotify` на огромном дереве) не фатальна: остаётся опрос по таймеру.
    fn watch(&self, tx: mpsc::UnboundedSender<PathBuf>) -> Option<notify::RecommendedWatcher> {
        let handler = move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        };
        let mut watcher = match notify::recommended_watcher(handler) {
            Ok(watcher) => watcher,
            Err(err) => {
                tracing::warn!("наблюдение за файлами недоступно, только опрос по таймеру: {err}");
                return None;
            }
        };
        for root in &self.options.roots {
            if let Err(err) = watcher.watch(root, RecursiveMode::Recursive) {
                tracing::warn!(root = %root.display(), "наблюдение за корнем не включилось, только опрос: {err}");
            }
        }
        Some(watcher)
    }

    /// Сводки по расписанию; пропущенная за время простоя — сразу.
    async fn schedule_loop(self: Arc<Self>) {
        let schedule = &self.options.schedule;
        match self.shared.store.last_period_to().await {
            Ok(last) if schedule.missed(last, time::now()) => {
                tracing::info!("сводка по расписанию была пропущена, собираю сейчас");
                self.make_digest().await;
            }
            Ok(_) => {}
            Err(err) => tracing::warn!("не удалось прочитать последнюю сводку: {err}"),
        }
        loop {
            let Some(next) = schedule.next_after(time::now()) else {
                tracing::warn!(schedule = schedule.expression(), "у расписания больше нет запусков");
                return;
            };
            tracing::info!(at = %time::rfc3339(next), "следующая сводка");
            loop {
                let left = next - time::now();
                if left <= 0 {
                    break;
                }
                tokio::time::sleep(SCHEDULER_STEP.min(Duration::from_secs(left as u64))).await;
            }
            self.make_digest().await;
        }
    }

    /// Свежий опрос всех проектов (незакоммиченное — на момент сводки) и
    /// сводка.
    async fn make_digest(&self) {
        let now = time::now();
        self.collector.scan_all(now).await;
        match self.shared.digester.build(now).await {
            Ok(Some(digest)) => tracing::info!(id = digest.id, "сводка готова"),
            Ok(None) => tracing::info!("за период изменений нет, сводка не создана"),
            Err(err) => tracing::warn!("сводка не собрана: {err:#}"),
        }
    }

    /// `/mcp` — MCP Streamable HTTP, `/healthz` — живость без токена.
    pub fn router(self: &Arc<Self>) -> Router {
        let shared = self.shared.clone();
        let mcp = StreamableHttpService::new(
            move || Ok(ActivityServer::new(shared.clone())),
            Arc::new(LocalSessionManager::default()),
            // По умолчанию `rmcp` принимает только loopback-имена в `Host`:
            // защита от DNS-rebinding со страниц в браузере.
            StreamableHttpServerConfig::default(),
        );
        let token = Arc::new(self.options.token.clone());
        let mcp = Router::new()
            .nest_service("/mcp", mcp)
            .layer(middleware::from_fn_with_state(token, require_token));
        Router::new()
            .route("/healthz", get(healthz))
            .with_state(self.clone())
            .merge(mcp)
    }
}

async fn healthz(State(daemon): State<Arc<Daemon>>) -> Response {
    let projects = daemon.shared.store.active_projects().await.map(|p| p.len());
    match projects {
        Ok(projects) => Json(json!({
            "status": "ok",
            "projects": projects,
            "roots": daemon.options.roots,
            "schedule": daemon.options.schedule.expression(),
            "next_digest": daemon.options.schedule.next_after(time::now()).map(time::rfc3339),
        }))
        .into_response(),
        Err(err) => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "error", "error": err.to_string() })))
            .into_response(),
    }
}

async fn require_token(State(token): State<Arc<Option<String>>>, request: Request, next: Next) -> Response {
    let Some(expected) = token.as_deref() else {
        return next.run(request).await;
    };
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if presented == Some(expected) {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

/// `--stdio`: MCP через stdin/stdout поверх базы работающего демона — для
/// клиентов, которые умеют только запускать сервер процессом. Репозитории
/// не опрашиваются: этим занят демон.
pub async fn serve_stdio(store: Store) -> anyhow::Result<()> {
    use rmcp::ServiceExt;
    let shared = Arc::new(Shared {
        digester: Digester::new(store.clone()),
        store,
        collector: None,
    });
    let service = ActivityServer::new(shared).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}

//! Демон проверяется так, как его видит MCP-клиент: HTTP-демон поднимается в
//! процессе теста на случайном порту, `--stdio` — настоящим процессом.

use activity_mcp::daemon::{Daemon, Options};
use activity_mcp::schedule::Schedule;
use activity_mcp::store::Store;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const BINARY: &str = env!("CARGO_BIN_EXE_activity-mcp");

fn temp_dir(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("activity-mcp-{name}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

fn commit(dir: &Path, file: &str, message: &str) {
    std::fs::write(dir.join(file), format!("{message}\n")).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test"]);
    commit(dir, "README.md", "Initial commit");
}

/// Демон на `127.0.0.1:0` с токеном; расписание — раз в год, сводки в тесте
/// только внеплановые.
async fn start_daemon(root: &Path, db: &Path) -> (String, Arc<Daemon>) {
    let store = Store::open(db).await.unwrap();
    let options = Options {
        roots: vec![root.to_path_buf()],
        schedule: Schedule::parse("0 0 1 1 *").unwrap(),
        rescan_interval: Duration::from_secs(3600),
        max_depth: 2,
        excludes: vec![],
        retention: Duration::from_secs(90 * 86_400),
        watch: false,
        token: Some("test-token".into()),
    };
    let daemon = Daemon::new(store, options);
    daemon.start().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = daemon.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}"), daemon)
}

async fn connect_http(base: &str, token: &str) -> Result<RunningService<RoleClient, ()>, String> {
    let config = StreamableHttpClientTransportConfig::with_uri(format!("{base}/mcp")).auth_header(token);
    let transport = StreamableHttpClientTransport::from_config(config);
    ().serve(transport).await.map_err(|err| err.to_string())
}

async fn call(client: &RunningService<RoleClient, ()>, name: &str, arguments: Value) -> (String, bool) {
    let params = CallToolRequestParams::new(name.to_string())
        .with_arguments(arguments.as_object().cloned().unwrap_or_default());
    let result = client.peer().call_tool(params).await.expect("tools/call");
    let content = serde_json::to_value(&result.content).unwrap();
    let text = content
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    (text, result.is_error == Some(true))
}

async fn call_json(client: &RunningService<RoleClient, ()>, name: &str, arguments: Value) -> Value {
    let (text, is_error) = call(client, name, arguments).await;
    assert!(!is_error, "{name}: {text}");
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("{name}: не JSON ({err}): {text}"))
}

async fn tool_names(client: &RunningService<RoleClient, ()>) -> Vec<String> {
    let mut names: Vec<String> = client
        .peer()
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    names
}

const TOOLS: [&str; 5] = [
    "activity_ack",
    "activity_build_digest",
    "activity_changes",
    "activity_digest",
    "activity_projects",
];

#[tokio::test]
async fn http_daemon_records_commits_and_serves_digests() {
    let root = temp_dir("http");
    let app = root.join("app");
    init_repo(&app);
    let db = temp_dir("http-db").join("activity.db");
    let (base, _daemon) = start_daemon(&root, &db).await;

    let client = connect_http(&base, "test-token").await.expect("подключение");
    assert_eq!(tool_names(&client).await, TOOLS);

    let projects = call_json(&client, "activity_projects", json!({})).await;
    assert_eq!(projects["projects"][0]["name"], "app");
    assert_eq!(projects["projects"][0]["branch"], "main");

    // Первое знакомство — не изменения: сводки нет.
    let empty = call_json(&client, "activity_build_digest", json!({})).await;
    assert!(empty["digest"].is_null(), "{empty}");

    commit(&app, "parser.rs", "Add parser");
    let built = call_json(&client, "activity_build_digest", json!({})).await;
    let text = built["digest"]["text"].as_str().expect("текст сводки");
    assert!(text.contains("### app (main)"), "{text}");
    assert!(text.contains("Add parser"), "{text}");
    let id = built["digest"]["id"].as_i64().unwrap();

    let changes = call_json(&client, "activity_changes", json!({ "project": "app", "since": "1h" })).await;
    assert_eq!(changes["changes"][0]["kind"], "commit");
    assert_eq!(changes["changes"][0]["subject"], "Add parser");
    let (text, is_error) = call(&client, "activity_changes", json!({ "project": "нет-такого" })).await;
    assert!(is_error && text.contains("не найден"), "{text}");

    let unread = call_json(&client, "activity_digest", json!({})).await;
    assert_eq!(unread["digests"].as_array().unwrap().len(), 1);
    call(&client, "activity_ack", json!({ "id": id })).await;
    let unread = call_json(&client, "activity_digest", json!({})).await;
    assert!(unread["digests"].as_array().unwrap().is_empty());
    let all = call_json(&client, "activity_digest", json!({ "unread_only": false })).await;
    assert_eq!(all["digests"][0]["acked"], true);
    let (_, is_error) = call(&client, "activity_ack", json!({ "id": 9999 })).await;
    assert!(is_error);

    client.cancel().await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn http_requires_token_but_healthz_does_not() {
    let root = temp_dir("auth");
    init_repo(&root.join("app"));
    let db = temp_dir("auth-db").join("activity.db");
    let (base, _daemon) = start_daemon(&root, &db).await;

    assert!(connect_http(&base, "wrong").await.is_err());

    let address = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let request = format!("GET /healthz HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"projects\":1"), "{response}");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn stdio_instance_reads_the_daemon_database() {
    let root = temp_dir("stdio");
    let app = root.join("app");
    init_repo(&app);
    let db = temp_dir("stdio-db").join("activity.db");
    let (base, _daemon) = start_daemon(&root, &db).await;
    commit(&app, "a.rs", "Add a");
    let client = connect_http(&base, "test-token").await.unwrap();
    call_json(&client, "activity_build_digest", json!({})).await;

    let mut command = tokio::process::Command::new(BINARY);
    command.arg("--stdio").arg("--db").arg(&db);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .expect("запуск --stdio");
    let stdio = ().serve(transport).await.expect("рукопожатие MCP");
    assert_eq!(tool_names(&stdio).await, TOOLS);
    let digests = call_json(&stdio, "activity_digest", json!({})).await;
    assert!(digests["digests"][0]["text"].as_str().unwrap().contains("Add a"), "{digests}");
    stdio.cancel().await.unwrap();
    let _ = std::fs::remove_dir_all(root);
}

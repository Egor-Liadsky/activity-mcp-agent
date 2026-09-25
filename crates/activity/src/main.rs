//! Точка входа `activity-mcp`: разбор флагов и запуск демона или
//! `--stdio`-экземпляра.

use activity_mcp::daemon::{self, Daemon, Options};
use activity_mcp::schedule::Schedule;
use activity_mcp::store::Store;
use activity_mcp::time::parse_duration;
use anyhow::{bail, Context};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "activity-mcp",
    version,
    about = "MCP-сервер активности git-проектов: наблюдение за каталогом, журнал изменений и сводки по расписанию"
)]
struct Cli {
    /// Каталог с проектами; флаг можно повторить
    #[arg(long = "root", value_name = "КАТАЛОГ", required_unless_present = "stdio")]
    roots: Vec<PathBuf>,
    /// Адрес HTTP; только loopback
    #[arg(long, default_value = "127.0.0.1:7878")]
    listen: SocketAddr,
    /// Файл базы SQLite [по умолчанию: <data-dir ОС>/activity-mcp/activity.db]
    #[arg(long)]
    db: Option<PathBuf>,
    /// Расписание сводок: cron из 5 полей (или 6–7 с секундами), местное время
    #[arg(long, default_value = "0 9,18 * * *")]
    schedule: String,
    /// Полный обход проектов (страховка от потерянных файловых событий)
    #[arg(long, default_value = "1h", value_parser = duration)]
    rescan_interval: Duration,
    /// Глубина поиска репозиториев под корнем
    #[arg(long, default_value_t = 3)]
    max_depth: usize,
    /// Дополнительное имя каталога, который не обходится и не наблюдается
    #[arg(long = "exclude", value_name = "ИМЯ")]
    excludes: Vec<String>,
    /// Срок хранения событий и сводок
    #[arg(long, default_value = "90d", value_parser = duration)]
    retention: Duration,
    /// Не подписываться на файловые события, только опрос по таймеру
    #[arg(long)]
    no_watch: bool,
    /// Файл с bearer-токеном, который требуется от клиентов `/mcp`
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// MCP через stdin/stdout поверх базы работающего демона
    #[arg(long)]
    stdio: bool,
}

fn duration(text: &str) -> Result<Duration, String> {
    parse_duration(text)
}

fn default_db() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("activity-mcp")
        .join("activity.db")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Журнал — в stderr: в режиме `--stdio` stdout занят протоколом, а
    // launchd и systemd сами перенаправляют stderr в файл.
    tracing_subscriber::fmt()
        .with_env_filter(
            // `rmcp` пишет INFO на каждое подключение, а клиенты подключаются
            // на каждую операцию и опрос: журнал круглосуточного демона
            // утонул бы в них.
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,rmcp=warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let db = cli.db.clone().unwrap_or_else(default_db);
    let store = Store::open(&db).await.with_context(|| format!("база {}", db.display()))?;
    if cli.stdio {
        return daemon::serve_stdio(store).await;
    }

    // Инструменты отдают содержимое чужих репозиториев без авторизации
    // по умолчанию, поэтому наружу демон не слушает.
    if !cli.listen.ip().is_loopback() {
        bail!("--listen {} — не loopback-адрес: демон слушает только 127.0.0.1 или ::1", cli.listen);
    }
    let schedule = Schedule::parse(&cli.schedule).map_err(anyhow::Error::msg)?;
    let mut roots = Vec::new();
    for root in &cli.roots {
        let root = root
            .canonicalize()
            .with_context(|| format!("корень {} недоступен", root.display()))?;
        if !root.is_dir() {
            bail!("корень {} — не каталог", root.display());
        }
        roots.push(root);
    }
    let token = match &cli.token_file {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("не прочитан файл токена {}", path.display()))?
                .trim()
                .to_string(),
        )
        .filter(|token| !token.is_empty()),
        None => None,
    };

    let options = Options {
        roots,
        schedule,
        rescan_interval: cli.rescan_interval,
        max_depth: cli.max_depth,
        excludes: cli.excludes,
        retention: cli.retention,
        watch: !cli.no_watch,
        token,
    };
    tracing::info!(db = %db.display(), roots = ?options.roots, schedule = %cli.schedule, "запуск");
    let daemon = Daemon::new(store, options);
    daemon.start().await?;

    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("не удалось занять {}", cli.listen))?;
    tracing::info!("MCP: http://{}/mcp", listener.local_addr()?);
    axum::serve(listener, daemon.router())
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

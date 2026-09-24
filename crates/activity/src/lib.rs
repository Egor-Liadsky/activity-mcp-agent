//! `activity-mcp` — демон, который следит за каталогом с git-проектами,
//! записывает изменения в SQLite и по расписанию собирает сводки. Сводки и
//! журнал отдаются инструментами MCP через Streamable HTTP.
//!
//! Библиотека нужна интеграционным тестам: они поднимают демон в своём
//! процессе на случайном порту. Точка входа — `main.rs`.

pub mod collector;
pub mod daemon;
pub mod digest;
pub mod discovery;
pub mod git;
pub mod schedule;
pub mod server;
pub mod store;
pub mod time;

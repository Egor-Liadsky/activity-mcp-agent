//! Чтение состояния репозитория системным `git`.
//!
//! Как и у `git-mcp`, вызывается настоящий `git`, а не `libgit2`: без
//! C-зависимости, и поведение совпадает с тем, что человек видит в
//! терминале. Демон только читает — ни одна команда здесь не меняет
//! репозиторий, индекс или ссылки.

use serde::Serialize;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Предел одного вызова: зависший `git` (сетевая ФС, огромный репозиторий)
/// не должен останавливать опрос остальных проектов.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Коммитов за один опрос ветки. Больше — это импорт чужой истории
/// (`git pull` давно заброшенного проекта), а не работа за период.
pub const MAX_NEW_COMMITS: usize = 200;
/// Путей изменённых файлов, которые хранятся у коммита.
const MAX_COMMIT_FILES: usize = 10;
/// Путей незакоммиченного, которые хранятся в снимке проекта.
pub const MAX_DIRTY_SAMPLE: usize = 10;

pub type GitResult<T> = Result<T, String>;

/// `git -C <repo> <args>`; stdout при успехе, текст stderr при ошибке.
pub async fn run(repo: &Path, args: &[&str]) -> GitResult<String> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "core.quotepath=off"])
        .args(args)
        // Без этого `git status` обновляет индекс и берёт `index.lock`:
        // фоновый опрос мешал бы коммиту, который человек делает в тот же
        // момент («Unable to create index.lock: File exists»).
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(GIT_TIMEOUT, child)
        .await
        .map_err(|_| format!("git {} не уложился в {} с", args.first().unwrap_or(&""), GIT_TIMEOUT.as_secs()))?
        .map_err(|err| format!("не удалось запустить git: {err}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!("git {} завершился с ошибкой: {}", args.first().unwrap_or(&""), stderr.trim()))
    }
}

/// Локальные ветки и их вершины.
pub async fn heads(repo: &Path) -> GitResult<Vec<(String, String)>> {
    let output = run(repo, &["for-each-ref", "--format=%(objectname) %(refname:short)", "refs/heads"]).await?;
    Ok(parse_heads(&output))
}

fn parse_heads(output: &str) -> Vec<(String, String)> {
    output
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(sha, name)| (name.to_string(), sha.to_string()))
        .collect()
}

/// Текущая ветка; `None` — отделённый HEAD или репозиторий без коммитов.
pub async fn head_branch(repo: &Path) -> Option<String> {
    run(repo, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// Незакоммиченное: число путей и первые из них.
pub async fn dirty(repo: &Path) -> GitResult<(usize, Vec<String>)> {
    let output = run(repo, &["status", "--porcelain=v1", "-z", "--untracked-files=normal"]).await?;
    Ok(parse_status(&output))
}

fn parse_status(output: &str) -> (usize, Vec<String>) {
    let mut entries = output.split('\0').filter(|entry| entry.len() > 3);
    let mut count = 0;
    let mut sample = Vec::new();
    while let Some(entry) = entries.next() {
        let (code, path) = entry.split_at(3);
        count += 1;
        if sample.len() < MAX_DIRTY_SAMPLE {
            sample.push(format!("{} {path}", code.trim()));
        }
        // У переименования и копирования следом идёт исходный путь.
        if code.starts_with('R') || code.starts_with('C') {
            entries.next();
        }
    }
    (count, sample)
}

/// Коммит в том виде, в каком он хранится в журнале событий.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
pub struct Commit {
    pub sha: String,
    pub author: String,
    pub email: String,
    pub committed_at: i64,
    pub subject: String,
    pub files_changed: usize,
    pub insertions: u64,
    pub deletions: u64,
    pub files: Vec<String>,
}

/// Коммиты, достижимые от `tip` и недостижимые от `exclude`. Курсор,
/// которого уже нет в базе объектов (ветку переписали и собрали мусор),
/// пропускается `--ignore-missing`, а не роняет опрос.
pub async fn new_commits(repo: &Path, tip: &str, exclude: &[&str]) -> GitResult<Vec<Commit>> {
    let max = format!("--max-count={MAX_NEW_COMMITS}");
    let mut args = vec![
        "log",
        "--ignore-missing",
        "--no-renames",
        "--numstat",
        &max,
        "--format=%x1e%H%x1f%an%x1f%ae%x1f%ct%x1f%s",
        tip,
    ];
    if !exclude.is_empty() {
        args.push("--not");
        args.extend(exclude);
    }
    // Конец опций: ни вершина, ни курсоры не прочтутся как путь.
    args.push("--");
    Ok(parse_log(&run(repo, &args).await?))
}

fn parse_log(output: &str) -> Vec<Commit> {
    output
        .split('\x1e')
        .filter(|record| !record.trim().is_empty())
        .filter_map(|record| {
            let (header, stats) = record.split_once('\n').unwrap_or((record, ""));
            let mut fields = header.split('\x1f');
            let mut commit = Commit {
                sha: fields.next()?.to_string(),
                author: fields.next()?.to_string(),
                email: fields.next()?.to_string(),
                committed_at: fields.next()?.parse().ok()?,
                subject: fields.next().unwrap_or_default().to_string(),
                files_changed: 0,
                insertions: 0,
                deletions: 0,
                files: Vec::new(),
            };
            for line in stats.lines().filter(|line| !line.trim().is_empty()) {
                let mut parts = line.splitn(3, '\t');
                let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next()) else {
                    continue;
                };
                commit.files_changed += 1;
                // У двоичных файлов вместо чисел «-».
                commit.insertions += added.parse::<u64>().unwrap_or(0);
                commit.deletions += removed.parse::<u64>().unwrap_or(0);
                if commit.files.len() < MAX_COMMIT_FILES {
                    commit.files.push(path.to_string());
                }
            }
            Some(commit)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_are_parsed() {
        let heads = parse_heads("aaa main\nbbb feature/x\n\n");
        assert_eq!(heads, vec![("main".into(), "aaa".into()), ("feature/x".into(), "bbb".into())]);
    }

    #[test]
    fn status_counts_renames_once() {
        let output = " M src/a.rs\0R  new.rs\0old.rs\0?? заметки.txt\0";
        let (count, sample) = parse_status(output);
        assert_eq!(count, 3);
        assert_eq!(sample, vec!["M src/a.rs", "R new.rs", "?? заметки.txt"]);
        assert_eq!(parse_status(""), (0, vec![]));
    }

    #[test]
    fn log_with_numstat_is_parsed() {
        let output = "\x1eabc\x1fИван\x1fi@x\x1f100\x1fAdd parser\n\n3\t1\tsrc/a.rs\n-\t-\tlogo.png\n\
                      \x1edef\x1fAnn\x1fa@x\x1f90\x1fEmpty\n";
        let commits = parse_log(output);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].sha, "abc");
        assert_eq!(commits[0].author, "Иван");
        assert_eq!(commits[0].committed_at, 100);
        assert_eq!((commits[0].files_changed, commits[0].insertions, commits[0].deletions), (2, 3, 1));
        assert_eq!(commits[0].files, vec!["src/a.rs", "logo.png"]);
        assert_eq!(commits[1].subject, "Empty");
        assert_eq!(commits[1].files_changed, 0);
    }
}

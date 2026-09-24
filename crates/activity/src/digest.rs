//! Сводка за период: события журнала, сгруппированные по проектам.
//!
//! Сводка детерминированная и без LLM: у демона нет ключей провайдера и
//! связи с `agentcore`. Пересказ моделью, если он нужен, делает клиент при
//! показе — по готовому тексту или JSON.

use crate::store::{DigestRow, EventRow, ProjectRow, Store};
use crate::time;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use tokio::sync::Mutex;

/// Коммитов проекта, перечисляемых в тексте; остальные — числом.
const MAX_COMMITS_IN_TEXT: usize = 8;

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Digest {
    pub period_from: String,
    pub period_to: String,
    pub totals: Totals,
    pub projects: Vec<ProjectDigest>,
    pub added_projects: Vec<String>,
    pub removed_projects: Vec<String>,
    /// Незакоммиченное в проектах, где за период не было событий: сводка о
    /// нём напоминает, но сама по себе сводку не создаёт.
    pub uncommitted_elsewhere: Vec<Uncommitted>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Totals {
    pub projects: usize,
    pub commits: usize,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProjectDigest {
    pub name: String,
    pub branch: Option<String>,
    pub commits: Vec<CommitBrief>,
    pub authors: Vec<String>,
    pub insertions: u64,
    pub deletions: u64,
    pub branches_created: Vec<String>,
    pub branches_deleted: Vec<String>,
    pub checkouts: Vec<String>,
    pub uncommitted_files: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CommitBrief {
    pub sha: String,
    pub branch: String,
    pub author: String,
    pub subject: String,
    pub insertions: u64,
    pub deletions: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Uncommitted {
    pub name: String,
    pub files: i64,
}

/// Группирует события периода. Пустой список событий — пустая сводка.
pub fn summarize(events: &[EventRow], projects: &[ProjectRow], from: i64, to: i64) -> Digest {
    let mut by_project: BTreeMap<&str, ProjectDigest> = BTreeMap::new();
    let mut digest = Digest {
        period_from: time::rfc3339(from),
        period_to: time::rfc3339(to),
        ..Digest::default()
    };
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let number = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);

    for event in events {
        let payload = &event.payload;
        match event.kind.as_str() {
            "project_added" => digest.added_projects.push(event.project.clone()),
            "project_removed" => digest.removed_projects.push(event.project.clone()),
            kind => {
                let entry = by_project.entry(&event.project).or_insert_with(|| ProjectDigest {
                    name: event.project.clone(),
                    ..ProjectDigest::default()
                });
                match kind {
                    "commit" => {
                        let commit = CommitBrief {
                            sha: text(payload, "sha").chars().take(7).collect(),
                            branch: text(payload, "branch"),
                            author: text(payload, "author"),
                            subject: text(payload, "subject"),
                            insertions: number(payload, "insertions"),
                            deletions: number(payload, "deletions"),
                        };
                        entry.insertions += commit.insertions;
                        entry.deletions += commit.deletions;
                        if !entry.authors.contains(&commit.author) {
                            entry.authors.push(commit.author.clone());
                        }
                        entry.commits.push(commit);
                    }
                    "branch_created" => entry.branches_created.push(text(payload, "branch")),
                    "branch_deleted" => entry.branches_deleted.push(text(payload, "branch")),
                    "checkout" => entry.checkouts.push(text(payload, "to")),
                    _ => {}
                }
            }
        }
    }

    for project in projects.iter().filter(|p| p.removed_at.is_none()) {
        match by_project.get_mut(project.name.as_str()) {
            Some(entry) => {
                entry.branch = project.head_branch.clone();
                entry.uncommitted_files = project.dirty_files;
            }
            None if project.dirty_files > 0 => digest.uncommitted_elsewhere.push(Uncommitted {
                name: project.name.clone(),
                files: project.dirty_files,
            }),
            None => {}
        }
    }

    digest.projects = by_project.into_values().collect();
    // Самые активные проекты — первыми.
    digest.projects.sort_by(|a, b| b.commits.len().cmp(&a.commits.len()).then(a.name.cmp(&b.name)));
    digest.totals = Totals {
        projects: digest.projects.len(),
        commits: digest.projects.iter().map(|p| p.commits.len()).sum(),
        insertions: digest.projects.iter().map(|p| p.insertions).sum(),
        deletions: digest.projects.iter().map(|p| p.deletions).sum(),
    };
    digest
}

/// Русское согласование числительного: 1 коммит, 2 коммита, 5 коммитов.
fn plural(n: u64, one: &str, few: &str, many: &str) -> String {
    let word = match (n % 10, n % 100) {
        (1, rem) if rem != 11 => one,
        (2..=4, rem) if !(12..=14).contains(&rem) => few,
        _ => many,
    };
    format!("{n} {word}")
}

fn commits_word(n: usize) -> String {
    plural(n as u64, "коммит", "коммита", "коммитов")
}

fn files_word(n: i64) -> String {
    plural(n.max(0) as u64, "файл", "файла", "файлов")
}

/// Markdown для показа человеку.
pub fn render(digest: &Digest, from: i64, to: i64) -> String {
    let mut out = format!("## Активность {} — {}\n\n", time::short(from), time::short(to));
    let totals = &digest.totals;
    if totals.projects > 0 {
        out.push_str(&format!(
            "{}, {}, +{} −{}\n",
            plural(totals.projects as u64, "проект", "проекта", "проектов"),
            commits_word(totals.commits),
            totals.insertions,
            totals.deletions
        ));
    }
    for project in &digest.projects {
        let branch = project.branch.as_deref().map(|b| format!(" ({b})")).unwrap_or_default();
        out.push_str(&format!("\n### {}{branch}\n", project.name));
        if !project.commits.is_empty() {
            out.push_str(&format!(
                "- {}, +{} −{}; авторы: {}\n",
                commits_word(project.commits.len()),
                project.insertions,
                project.deletions,
                project.authors.join(", ")
            ));
            for commit in project.commits.iter().rev().take(MAX_COMMITS_IN_TEXT) {
                out.push_str(&format!("  - `{}` {} [{}]\n", commit.sha, commit.subject, commit.branch));
            }
            if project.commits.len() > MAX_COMMITS_IN_TEXT {
                out.push_str(&format!("  - …и ещё {}\n", project.commits.len() - MAX_COMMITS_IN_TEXT));
            }
        }
        if !project.branches_created.is_empty() {
            out.push_str(&format!("- новые ветки: {}\n", project.branches_created.join(", ")));
        }
        if !project.branches_deleted.is_empty() {
            out.push_str(&format!("- удалены ветки: {}\n", project.branches_deleted.join(", ")));
        }
        if !project.checkouts.is_empty() {
            out.push_str(&format!("- переключения на: {}\n", project.checkouts.join(" → ")));
        }
        if project.uncommitted_files > 0 {
            out.push_str(&format!("- незакоммичено: {}\n", files_word(project.uncommitted_files)));
        }
    }
    if !digest.added_projects.is_empty() {
        out.push_str(&format!("\nНовые проекты: {}\n", digest.added_projects.join(", ")));
    }
    if !digest.removed_projects.is_empty() {
        out.push_str(&format!("\nПропавшие проекты: {}\n", digest.removed_projects.join(", ")));
    }
    if !digest.uncommitted_elsewhere.is_empty() {
        let list: Vec<String> = digest
            .uncommitted_elsewhere
            .iter()
            .map(|u| format!("{} ({})", u.name, files_word(u.files)))
            .collect();
        out.push_str(&format!("\nНезакоммичено без новых событий: {}\n", list.join(", ")));
    }
    out
}

/// Собирает сводки: период — от конца предыдущей до `now`.
pub struct Digester {
    store: Store,
    /// Плановая сводка и внеплановая по инструменту не должны разделить
    /// один период на две одинаковые.
    busy: Mutex<()>,
}

impl Digester {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            busy: Mutex::new(()),
        }
    }

    /// Новая сводка или `None`, если за период ничего не произошло. Конец
    /// периода не сдвигается пустым периодом: следующая сводка охватит и
    /// его, так что события не теряются.
    pub async fn build(&self, now: i64) -> anyhow::Result<Option<DigestRow>> {
        let _guard = self.busy.lock().await;
        let from = match self.store.last_period_to().await? {
            Some(last) => last,
            // Период полуоткрытый, `(from, to]`: первое событие должно в
            // него попасть.
            None => match self.store.earliest_event().await? {
                Some(first) => first - 1,
                None => return Ok(None),
            },
        };
        if from >= now {
            return Ok(None);
        }
        let events = self.store.events_between(from, now).await?;
        if events.is_empty() {
            return Ok(None);
        }
        let projects = self.store.projects().await?;
        let digest = summarize(&events, &projects, from, now);
        let text = render(&digest, from, now);
        let body = serde_json::to_value(&digest)?;
        let id = self.store.insert_digest(from, now, now, &body, &text).await?;
        Ok(Some(DigestRow {
            id,
            period_from: from,
            period_to: now,
            created_at: now,
            body,
            text,
            acked_at: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(project: &str, kind: &str, payload: Value) -> EventRow {
        EventRow {
            id: 0,
            project_id: 0,
            project: project.into(),
            kind: kind.into(),
            seen_at: 0,
            payload,
        }
    }

    fn commit(project: &str, sha: &str, author: &str, subject: &str) -> EventRow {
        event(
            project,
            "commit",
            json!({ "sha": sha, "branch": "main", "author": author, "subject": subject, "insertions": 10, "deletions": 2 }),
        )
    }

    fn project(name: &str, dirty: i64) -> ProjectRow {
        ProjectRow {
            id: 0,
            path: format!("/w/{name}"),
            name: name.into(),
            head_branch: Some("main".into()),
            dirty_files: dirty,
            dirty_sample: vec![],
            first_seen: 0,
            last_scanned: None,
            last_activity: None,
            removed_at: None,
        }
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "коммит", "коммита", "коммитов"), "1 коммит");
        assert_eq!(plural(3, "коммит", "коммита", "коммитов"), "3 коммита");
        assert_eq!(plural(11, "коммит", "коммита", "коммитов"), "11 коммитов");
        assert_eq!(plural(21, "коммит", "коммита", "коммитов"), "21 коммит");
        assert_eq!(plural(14, "коммит", "коммита", "коммитов"), "14 коммитов");
    }

    #[test]
    fn events_are_grouped_by_project() {
        let events = vec![
            commit("cli", "aaaaaaaaaa", "Egor", "Add parser"),
            commit("server", "bbbbbbbbbb", "Ann", "Fix auth"),
            commit("cli", "cccccccccc", "Ann", "Add tests"),
            event("cli", "branch_created", json!({ "branch": "feature" })),
            event("new", "project_added", json!({})),
        ];
        let projects = vec![project("cli", 3), project("server", 0), project("idle", 2)];
        let digest = summarize(&events, &projects, 0, 100);
        assert_eq!(digest.totals, Totals { projects: 2, commits: 3, insertions: 30, deletions: 6 });
        assert_eq!(digest.projects[0].name, "cli");
        assert_eq!(digest.projects[0].authors, vec!["Egor", "Ann"]);
        assert_eq!(digest.projects[0].commits[0].sha, "aaaaaaa");
        assert_eq!(digest.projects[0].uncommitted_files, 3);
        assert_eq!(digest.added_projects, vec!["new"]);
        assert_eq!(digest.uncommitted_elsewhere, vec![Uncommitted { name: "idle".into(), files: 2 }]);

        let text = render(&digest, 0, 100);
        assert!(text.contains("2 проекта, 3 коммита, +30 −6"), "{text}");
        assert!(text.contains("### cli (main)"), "{text}");
        assert!(text.contains("`ccccccc` Add tests [main]"), "{text}");
        assert!(text.contains("новые ветки: feature"), "{text}");
        assert!(text.contains("незакоммичено: 3 файла"), "{text}");
        assert!(text.contains("idle (2 файла)"), "{text}");
    }

    #[tokio::test]
    async fn periods_follow_each_other_and_empty_ones_are_skipped() {
        let store = Store::open_in_memory().await.unwrap();
        let digester = Digester::new(store.clone());
        assert!(digester.build(100).await.unwrap().is_none());

        let (id, _) = store.upsert_project("/w/cli", "cli", 0).await.unwrap();
        store.insert_event(id, "commit", 50, Some("a1"), &json!({ "sha": "a1", "author": "E" })).await.unwrap();
        let first = digester.build(100).await.unwrap().expect("сводка");
        assert_eq!((first.period_from, first.period_to), (49, 100));
        assert!(first.text.contains("1 коммит"), "{}", first.text);

        assert!(digester.build(200).await.unwrap().is_none());
        store.insert_event(id, "commit", 250, Some("b2"), &json!({ "sha": "b2", "author": "E" })).await.unwrap();
        let second = digester.build(300).await.unwrap().expect("вторая сводка");
        // Пустой период 100..200 вошёл во вторую сводку.
        assert_eq!((second.period_from, second.period_to), (100, 300));
    }
}

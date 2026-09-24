//! Сборщик: превращает состояние репозиториев в события журнала.
//!
//! Источник правды — git, а не файловые события: `notify` лишь подсказывает,
//! какие проекты опросить раньше срока, а полный опрос по таймеру находит
//! всё, что пропущено (демон был остановлен, событие потерялось).

use crate::discovery::{owner, Walker};
use crate::git;
use crate::store::{Snapshot, Store};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use tokio::sync::{Mutex, RwLock};

pub struct Collector {
    store: Store,
    walker: Walker,
    roots: Vec<PathBuf>,
    /// Путь проекта -> id; по нему путь файлового события находит проект.
    index: RwLock<Vec<(PathBuf, i64)>>,
    /// Опросы идут строго по одному: курсоры веток читаются и пишутся
    /// парой, и два параллельных опроса одного проекта записали бы одни и
    /// те же ветки дважды.
    busy: Mutex<()>,
}

impl Collector {
    pub fn new(store: Store, walker: Walker, roots: Vec<PathBuf>) -> Self {
        Self {
            store,
            walker,
            roots,
            index: RwLock::new(Vec::new()),
            busy: Mutex::new(()),
        }
    }

    pub fn walker(&self) -> &Walker {
        &self.walker
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    /// Поиск проектов и опрос всех. Возвращает число новых событий.
    pub async fn rescan(&self, now: i64) -> anyhow::Result<usize> {
        let _guard = self.busy.lock().await;
        let mut events = self.discover(now).await?;
        events += self.scan_all_locked(now).await;
        Ok(events)
    }

    /// Опрос всех известных проектов без поиска новых.
    pub async fn scan_all(&self, now: i64) -> usize {
        let _guard = self.busy.lock().await;
        self.scan_all_locked(now).await
    }

    /// Опрос проектов, которым принадлежат изменившиеся пути. Путь внутри
    /// `.git` вне известных проектов — это `git init` или `git clone`:
    /// запускается поиск проектов.
    pub async fn scan_paths(&self, paths: &HashSet<PathBuf>, now: i64) -> anyhow::Result<usize> {
        let _guard = self.busy.lock().await;
        let index = self.index.read().await.clone();
        let mut owned = HashSet::new();
        let mut unknown_repo = false;
        for path in paths {
            match owner(&index, path) {
                Some(id) => {
                    owned.insert(*id);
                }
                None => unknown_repo |= path.components().any(|c| c == Component::Normal(".git".as_ref())),
            }
        }
        let mut events = 0;
        if unknown_repo {
            events += self.discover(now).await?;
        }
        for (path, id) in index.iter().filter(|(_, id)| owned.contains(id)) {
            events += self.scan_logged(*id, path, now).await;
        }
        Ok(events)
    }

    async fn scan_all_locked(&self, now: i64) -> usize {
        let index = self.index.read().await.clone();
        let mut events = 0;
        for (path, id) in &index {
            events += self.scan_logged(*id, path, now).await;
        }
        events
    }

    /// Сверяет найденные на диске проекты с базой: новые регистрирует,
    /// пропавшие помечает. События о появлении пишутся, только если база
    /// уже знала какие-то проекты: при первом запуске все проекты «новые»,
    /// и сводка из одного их списка бесполезна.
    async fn discover(&self, now: i64) -> anyhow::Result<usize> {
        let walker = self.walker.clone();
        let roots = self.roots.clone();
        let found = tokio::task::spawn_blocking(move || walker.discover(&roots)).await?;
        let known = self.store.projects().await?;
        let first_run = known.is_empty();
        let mut events = 0;
        let mut index = Vec::with_capacity(found.len());
        for project in &found {
            let path = project.path.to_string_lossy();
            let (id, created) = self.store.upsert_project(&path, &project.name, now).await?;
            if created && !first_run {
                self.store
                    .insert_event(id, "project_added", now, None, &json!({ "path": path }))
                    .await?;
                events += 1;
            }
            index.push((project.path.clone(), id));
        }
        let present: HashSet<&Path> = found.iter().map(|project| project.path.as_path()).collect();
        for project in known.iter().filter(|p| p.removed_at.is_none()) {
            if !present.contains(Path::new(&project.path)) {
                self.store.mark_removed(project.id, now).await?;
                self.store
                    .insert_event(project.id, "project_removed", now, None, &json!({ "path": project.path }))
                    .await?;
                events += 1;
            }
        }
        *self.index.write().await = index;
        Ok(events)
    }

    async fn scan_logged(&self, id: i64, path: &Path, now: i64) -> usize {
        match self.scan_project(id, path, now).await {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(project = %path.display(), "опрос не удался: {err}");
                0
            }
        }
    }

    /// Опрос одного проекта: ветки, текущая ветка, незакоммиченное.
    async fn scan_project(&self, id: i64, path: &Path, now: i64) -> anyhow::Result<usize> {
        let heads = git::heads(path).await.map_err(anyhow::Error::msg)?;
        let cursors = self.store.refs(id).await?;
        let previous = self.store.find_project(&path.to_string_lossy()).await?;
        let never_scanned = previous.as_ref().is_none_or(|p| p.last_scanned.is_none());
        let mut events = 0;

        if never_scanned && cursors.is_empty() {
            // Первое знакомство: история до этого момента — не изменения
            // периода, запоминаются только вершины веток.
            for (branch, sha) in &heads {
                self.store.set_ref(id, branch, sha).await?;
            }
        } else {
            events += self.record_branches(id, path, &heads, &cursors, now).await?;
        }

        let head_branch = git::head_branch(path).await;
        if let Some(previous) = previous.as_ref().filter(|_| !never_scanned)
            && previous.head_branch.is_some()
            && head_branch.is_some()
            && previous.head_branch != head_branch
        {
            let payload = json!({ "from": previous.head_branch, "to": head_branch });
            self.store.insert_event(id, "checkout", now, None, &payload).await?;
            events += 1;
        }

        let (dirty_files, dirty_sample) = git::dirty(path).await.map_err(anyhow::Error::msg)?;
        let snapshot = Snapshot {
            head_branch,
            dirty_files: dirty_files as i64,
            dirty_sample,
        };
        self.store.save_snapshot(id, &snapshot, now).await?;
        Ok(events)
    }

    async fn record_branches(
        &self,
        id: i64,
        path: &Path,
        heads: &[(String, String)],
        cursors: &HashMap<String, String>,
        now: i64,
    ) -> anyhow::Result<usize> {
        let mut events = 0;
        // Исключаются все известные вершины, а не только своя: новая ветка,
        // отведённая от main, не должна принести в сводку всю историю main.
        let known: Vec<&str> = cursors.values().map(String::as_str).collect();
        // Сначала ветки, которые уже были, потом новые, и новым исключаются
        // ещё и текущие вершины старых: коммит, сделанный в main до
        // ответвления feature, приписывается main, а не feature.
        let (existing, created): (Vec<_>, Vec<_>) = heads.iter().partition(|(branch, _)| cursors.contains_key(branch));
        let mut known_with_tips = known.clone();
        known_with_tips.extend(existing.iter().map(|(_, sha)| sha.as_str()));
        for (branch, sha) in existing.into_iter().chain(created) {
            let cursor = cursors.get(branch);
            if cursor == Some(sha) {
                continue;
            }
            let exclude = if cursor.is_none() {
                let payload = json!({ "branch": branch, "sha": sha });
                self.store.insert_event(id, "branch_created", now, None, &payload).await?;
                events += 1;
                &known_with_tips
            } else {
                &known
            };
            let commits = git::new_commits(path, sha, exclude).await.map_err(anyhow::Error::msg)?;
            // Старые первыми: порядок id совпадает с порядком истории.
            for commit in commits.iter().rev() {
                let mut payload = serde_json::to_value(commit)?;
                payload["branch"] = json!(branch);
                if self.store.insert_event(id, "commit", now, Some(&commit.sha), &payload).await? {
                    events += 1;
                }
            }
            self.store.set_ref(id, branch, sha).await?;
        }
        let alive: HashSet<&str> = heads.iter().map(|(branch, _)| branch.as_str()).collect();
        for (branch, sha) in cursors {
            if !alive.contains(branch.as_str()) {
                let payload = json!({ "branch": branch, "sha": sha });
                self.store.insert_event(id, "branch_deleted", now, None, &payload).await?;
                self.store.delete_ref(id, branch).await?;
                events += 1;
            }
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    pub fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    fn commit(dir: &Path, file: &str, text: &str, message: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        commit(dir, "README.md", "hello\n", "Initial commit");
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-collector-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    async fn kinds(store: &Store) -> Vec<String> {
        store
            .events_between(0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    #[tokio::test]
    async fn history_before_first_scan_is_baseline_only() {
        let root = temp_root("baseline");
        init_repo(&root.join("app"));
        commit(&root.join("app"), "a.txt", "1\n", "Second");
        let store = Store::open_in_memory().await.unwrap();
        let collector = Collector::new(store.clone(), Walker::new(2, &[]), vec![root.clone()]);

        assert_eq!(collector.rescan(100).await.unwrap(), 0);
        assert!(kinds(&store).await.is_empty());
        let project = &store.active_projects().await.unwrap()[0];
        assert_eq!(project.name, "app");
        assert_eq!(project.head_branch.as_deref(), Some("main"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn commits_branches_and_checkouts_become_events() {
        let root = temp_root("events");
        let app = root.join("app");
        init_repo(&app);
        let store = Store::open_in_memory().await.unwrap();
        let collector = Collector::new(store.clone(), Walker::new(2, &[]), vec![root.clone()]);
        collector.rescan(100).await.unwrap();

        commit(&app, "a.txt", "one\ntwo\n", "Add a");
        git(&app, &["checkout", "-q", "-b", "feature"]);
        commit(&app, "b.txt", "b\n", "Add b");
        std::fs::write(app.join("wip.txt"), "wip\n").unwrap();
        let events = collector.scan_paths(&HashSet::from([app.join("a.txt")]), 200).await.unwrap();
        assert_eq!(events, 4, "{:?}", kinds(&store).await);
        assert_eq!(kinds(&store).await, vec!["commit", "branch_created", "commit", "checkout"]);
        let branches: Vec<String> = store
            .events_between(0, i64::MAX)
            .await
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "commit")
            .map(|event| format!("{}:{}", event.payload["branch"].as_str().unwrap(), event.payload["subject"].as_str().unwrap()))
            .collect();
        assert_eq!(branches, vec!["main:Add a", "feature:Add b"]);
        let project = &store.active_projects().await.unwrap()[0];
        assert_eq!(project.dirty_files, 1);
        assert_eq!(project.dirty_sample, vec!["?? wip.txt"]);

        // Слияние не повторяет уже учтённый коммит ветки.
        git(&app, &["checkout", "-q", "main"]);
        git(&app, &["merge", "-q", "--ff-only", "feature"]);
        git(&app, &["branch", "-q", "-d", "feature"]);
        collector.scan_all(300).await;
        let later: Vec<String> = store
            .events_between(200, 300)
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(later, vec!["branch_deleted", "checkout"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn vanished_cursor_does_not_break_log() {
        let root = temp_root("missing");
        let app = root.join("app");
        init_repo(&app);
        // Курсор на коммит, которого нет в базе объектов: ветку переписали и
        // собрали мусор.
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let commits = git::new_commits(&app, "HEAD", &[missing]).await.unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].subject, "Initial commit");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn new_and_removed_projects_are_reported_after_first_run() {
        let root = temp_root("projects");
        init_repo(&root.join("one"));
        let store = Store::open_in_memory().await.unwrap();
        let collector = Collector::new(store.clone(), Walker::new(2, &[]), vec![root.clone()]);
        collector.rescan(100).await.unwrap();

        init_repo(&root.join("two"));
        let git_dir = HashSet::from([root.join("two/.git/HEAD")]);
        assert_eq!(collector.scan_paths(&git_dir, 200).await.unwrap(), 1);
        std::fs::remove_dir_all(root.join("one")).unwrap();
        collector.rescan(300).await.unwrap();
        assert_eq!(kinds(&store).await, vec!["project_added", "project_removed"]);
        let names: Vec<String> = store.active_projects().await.unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["two"]);
        let _ = std::fs::remove_dir_all(root);
    }
}

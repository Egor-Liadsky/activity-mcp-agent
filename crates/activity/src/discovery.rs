//! Поиск проектов под корнями наблюдения и сопоставление пути файла с
//! проектом.
//!
//! Проект — каталог с `.git` (каталог у обычного репозитория, файл у
//! подмодуля и рабочего дерева `git worktree`). Внутрь найденного проекта
//! обход продолжается: подмодули — самостоятельные проекты со своими
//! коммитами.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Каталоги сборки и зависимостей: в них не ищут проекты, и изменения в них
/// не делают проект «грязным» — `cargo build` иначе будил бы опрос на каждую
/// запись в `target/`.
pub const DEFAULT_EXCLUDES: [&str; 12] = [
    "target",
    "node_modules",
    "build",
    "dist",
    ".build",
    ".gradle",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    "DerivedData",
    "Pods",
];

/// Найденный проект.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub name: String,
}

/// Правила обхода: глубина и исключаемые имена каталогов.
#[derive(Debug, Clone)]
pub struct Walker {
    pub max_depth: usize,
    pub excludes: HashSet<String>,
}

impl Walker {
    pub fn new(max_depth: usize, extra_excludes: &[String]) -> Self {
        let mut excludes: HashSet<String> = DEFAULT_EXCLUDES.iter().map(|name| name.to_string()).collect();
        excludes.extend(extra_excludes.iter().cloned());
        Self { max_depth, excludes }
    }

    /// Проекты под всеми корнями. Имя — путь относительно корня; при
    /// нескольких корнях к нему спереди добавляется имя корня, чтобы
    /// одинаковые подкаталоги разных корней не совпали по имени.
    pub fn discover(&self, roots: &[PathBuf]) -> Vec<Found> {
        let mut found = Vec::new();
        for root in roots {
            let prefix = (roots.len() > 1).then(|| base_name(root));
            self.walk(root, root, 0, prefix.as_deref(), &mut found);
        }
        found.sort_by(|a, b| a.path.cmp(&b.path));
        found.dedup_by(|a, b| a.path == b.path);
        found
    }

    fn walk(&self, root: &Path, dir: &Path, depth: usize, prefix: Option<&str>, found: &mut Vec<Found>) {
        if dir.join(".git").exists() {
            found.push(Found {
                path: dir.to_path_buf(),
                name: project_name(root, dir, prefix),
            });
        }
        if depth >= self.max_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            // Тип записи без перехода по ссылке: символическая ссылка на
            // каталог выше по дереву зациклила бы обход.
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_dir && !name.starts_with('.') && !self.excludes.contains(&name) {
                self.walk(root, &entry.path(), depth + 1, prefix, found);
            }
        }
    }

    /// Изменение по этому пути стоит внимания: не лежит в исключённом
    /// каталоге. `.git` не исключается — новые коммиты видны именно по нему.
    pub fn is_relevant(&self, path: &Path) -> bool {
        !path.components().any(|component| match component {
            Component::Normal(name) => self.excludes.contains(name.to_string_lossy().as_ref()),
            _ => false,
        })
    }
}

fn base_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn project_name(root: &Path, dir: &Path, prefix: Option<&str>) -> String {
    let relative = dir
        .strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().into_owned())
        .filter(|rel| !rel.is_empty());
    match (prefix, relative) {
        (Some(prefix), Some(rel)) => format!("{prefix}/{rel}"),
        (Some(prefix), None) => prefix.to_string(),
        (None, Some(rel)) => rel,
        // Корень сам по себе репозиторий.
        (None, None) => base_name(root),
    }
}

/// Проект, которому принадлежит путь: ближайший предок из списка. Самый
/// глубокий выигрывает — изменение внутри подмодуля относится к подмодулю,
/// а не к репозиторию, который его содержит.
pub fn owner<'a, T>(projects: &'a [(PathBuf, T)], path: &Path) -> Option<&'a T> {
    projects
        .iter()
        .filter(|(root, _)| path.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())
        .map(|(_, value)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("activity-discovery-{name}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn repositories_are_found_with_nested_submodules() {
        let root = temp_dir("walk");
        std::fs::create_dir_all(root.join("a/.git")).unwrap();
        std::fs::create_dir_all(root.join("a/sub")).unwrap();
        std::fs::write(root.join("a/sub/.git"), "gitdir: ../.git/modules/sub").unwrap();
        std::fs::create_dir_all(root.join("group/b/.git")).unwrap();
        std::fs::create_dir_all(root.join("a/target/x/.git")).unwrap();
        std::fs::create_dir_all(root.join(".hidden/c/.git")).unwrap();
        std::fs::create_dir_all(root.join("deep/1/2/3/.git")).unwrap();

        let walker = Walker::new(2, &[]);
        let names: Vec<String> = walker.discover(std::slice::from_ref(&root)).into_iter().map(|f| f.name).collect();
        assert_eq!(names, vec!["a", "a/sub", "group/b"]);

        let other = temp_dir("second");
        std::fs::create_dir_all(other.join("z/.git")).unwrap();
        let found = walker.discover(&[root.clone(), other.clone()]);
        assert!(found.iter().any(|f| f.name == format!("{}/z", base_name(&other))));
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn deepest_project_owns_the_path() {
        let projects = vec![(PathBuf::from("/w/a"), 1), (PathBuf::from("/w/a/sub"), 2), (PathBuf::from("/w/ab"), 3)];
        assert_eq!(owner(&projects, Path::new("/w/a/src/main.rs")), Some(&1));
        assert_eq!(owner(&projects, Path::new("/w/a/sub/x")), Some(&2));
        assert_eq!(owner(&projects, Path::new("/w/ab/x")), Some(&3));
        assert_eq!(owner(&projects, Path::new("/w/c")), None);
    }

    #[test]
    fn build_directories_are_not_relevant() {
        let walker = Walker::new(2, &["tmp".to_string()]);
        assert!(walker.is_relevant(Path::new("/w/a/src/lib.rs")));
        assert!(walker.is_relevant(Path::new("/w/a/.git/refs/heads/main")));
        assert!(!walker.is_relevant(Path::new("/w/a/target/debug/x")));
        assert!(!walker.is_relevant(Path::new("/w/a/web/node_modules/y")));
        assert!(!walker.is_relevant(Path::new("/w/a/tmp/z")));
    }
}

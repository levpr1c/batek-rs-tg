//! Корень репозитория: каталоги `music/`, `video/` и файл `yt_cache.json`.
//!
//! Порядок поиска:
//! 1. `BATEK_ROOT`, если задан и не пуст;
//! 2. текущий каталог, если в нём есть `music/` или `video/`;
//! 3. родитель текущего каталога (запуск `cargo run` из `rust/`);
//! 4. текущий каталог как последний вариант.
//!
//! Файловые проверки синхронные: функция вызывается один раз при старте через
//! `spawn_blocking`, поэтому рантайм не блокируется.

use std::path::{Path, PathBuf};

pub fn resolve_root() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    resolve_root_with(std::env::var("BATEK_ROOT").ok().as_deref(), &cwd)
}

/// Чистая часть `resolve_root`, отделена для тестов без переменных окружения.
pub fn resolve_root_with(explicit: Option<&str>, cwd: &Path) -> PathBuf {
    if let Some(root) = explicit {
        if !root.trim().is_empty() {
            return PathBuf::from(root);
        }
    }

    if has_assets(cwd) {
        return cwd.to_path_buf();
    }

    if let Some(parent) = cwd.parent() {
        if has_assets(parent) {
            return parent.to_path_buf();
        }
    }

    cwd.to_path_buf()
}

fn has_assets(dir: &Path) -> bool {
    dir.join("music").is_dir() || dir.join("video").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-paths-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn explicit_root_wins() {
        let dir = tempdir("explicit");
        assert_eq!(
            resolve_root_with(Some("/srv/batek"), &dir),
            PathBuf::from("/srv/batek")
        );
        // пустое значение игнорируется
        assert_eq!(resolve_root_with(Some("   "), &dir), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cwd_with_assets_wins_over_parent() {
        let root = tempdir("cwd");
        let nested = root.join("nested");
        std::fs::create_dir_all(root.join("music")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(resolve_root_with(None, &root), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_parent_when_started_from_rust_dir() {
        let root = tempdir("parent");
        let rust = root.join("rust");
        std::fs::create_dir_all(root.join("video")).unwrap();
        std::fs::create_dir_all(&rust).unwrap();
        assert_eq!(resolve_root_with(None, &rust), root);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn last_resort_is_cwd() {
        let dir = tempdir("empty");
        assert_eq!(resolve_root_with(None, &dir), dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Два файла настроек, которые меняются из бота.
//!
//! Базы нет: `data/keywords.json` и `data/access.json` читаются при старте и
//! перезаписываются целиком. Запись атомарная: данные уходят во временный
//! файл рядом с целевым, сбрасываются на диск и только потом подменяют
//! целевой через `rename`. Иначе выключенный посреди записи процесс оставил
//! бы обрезанный JSON, и следующий старт молча потерял бы все правки.

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Serialize};
use tracing::warn;

/// Каталог настроек внутри корня бота.
pub const DATA_DIR: &str = "data";

pub fn data_dir(root: &Path) -> PathBuf {
    root.join(DATA_DIR)
}

/// Читает JSON. Отсутствие файла это первый запуск, а не ошибка. Битый файл
/// тоже не повод падать: бот стартует на встроенной таблице, а в журнале
/// остаётся причина.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!("store: {} не прочитан: {e}", path.display());
            return None;
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(e) => {
            warn!("store: {} не разобран: {e}", path.display());
            None
        }
    }
}

/// Временный файл рядом с целевым. Расширение `tmp` вместо `json` могло бы
/// столкнуться с другим именем, поэтому к полному имени добавляется суффикс.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Пишет JSON атомарно. Родительские каталоги создаются при первой правке:
/// репозиторий не хранит `data/`, он появляется вместе с первой настройкой.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "у файла нет родительского каталога",
        ));
    };
    std::fs::create_dir_all(dir)?;

    let tmp = temp_path(path);
    let bytes = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    {
        use std::io::Write as _;
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&bytes)?;
        // Без sync данные могли остаться в кэше страниц, а rename уже
        // подменил бы целевой файл.
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;

    // Синхронизация каталога фиксирует сам rename. Ошибка не критична:
    // содержимое уже на диске, теряется только гарантия порядка после
    // внезапного выключения питания.
    if let Ok(dir) = std::fs::File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Sample {
        names: Vec<String>,
        count: u32,
    }

    #[test]
    fn round_trip_creates_directory_and_leaves_no_temp() {
        let root = temp_root("roundtrip");
        let path = data_dir(&root).join("sample.json");
        let value = Sample {
            names: vec!["бомжара".to_string(), "bomjara".to_string()],
            count: 2,
        };

        write_json_atomic(&path, &value).unwrap();

        assert!(path.exists(), "файл не появился");
        assert!(
            !path.with_file_name("sample.json.tmp").exists(),
            "временный файл остался"
        );
        assert_eq!(read_json::<Sample>(&path), Some(value));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_file_is_none_not_panic() {
        let root = temp_root("missing");
        let path = data_dir(&root).join("nothing.json");
        assert_eq!(read_json::<Sample>(&path), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn broken_file_is_none_and_original_stays() {
        let root = temp_root("broken");
        let path = data_dir(&root).join("sample.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "broken json").unwrap();

        assert_eq!(read_json::<Sample>(&path), None);
        // Битый файл не перезаписывается чтением: его видно и можно починить.
        assert_eq!(std::fs::read(&path).unwrap(), b"broken json");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn overwrite_replaces_previous_contents() {
        let root = temp_root("overwrite");
        let path = data_dir(&root).join("sample.json");

        write_json_atomic(
            &path,
            &Sample {
                names: vec!["a".to_string()],
                count: 1,
            },
        )
        .unwrap();
        write_json_atomic(
            &path,
            &Sample {
                names: vec!["b".to_string()],
                count: 2,
            },
        )
        .unwrap();

        let loaded = read_json::<Sample>(&path).unwrap();
        assert_eq!(loaded.count, 2);
        assert_eq!(loaded.names, vec!["b".to_string()]);

        let _ = std::fs::remove_dir_all(&root);
    }
}

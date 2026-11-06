//! Кэш идентификаторов YouTube.
//!
//! Что чинится относительно Python-версии (`cogs/video.py`):
//! - там шесть вызовов yt-dlp шли на импорте модуля, до готовности бота:
//!   бот не поднимался от секунд до бесконечности (у yt-dlp
//!   `socket_timeout=None`), а результат терялся при каждом рестарте;
//! - списки жили в памяти вечно и не обновлялись;
//! - ошибка одного канала неотличима от пустого канала.
//!
//! Здесь: вызовы идут последовательно в фоновой задаче, у каждого жёсткий
//! таймаут с убийством процесса, результат сливается по вкладкам (упавшая
//! вкладка сохраняет прошлые данные), пишется на диск атомарно и читается
//! при следующем старте до готовности бота.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use anyhow::Context as _;
use rand::Rng as _;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::RwLock;
use tracing::{info, warn};

/// (ключ кэша, адрес вкладки), порядок как в Python `tasks[]`.
pub const TABS: &[(&str, &str)] = &[
    (
        "batek:shorts",
        "https://www.youtube.com/@batek_official9120/shorts",
    ),
    (
        "batek:videos",
        "https://www.youtube.com/@batek_official9120/videos",
    ),
    (
        "victorovich:shorts",
        "https://www.youtube.com/@batya_viktorovich/shorts",
    ),
    (
        "victorovich:videos",
        "https://www.youtube.com/@batya_viktorovich/videos",
    ),
    (
        "timur:shorts",
        "https://www.youtube.com/@SravneniyaOfficial/shorts",
    ),
    (
        "timur:videos",
        "https://www.youtube.com/@SravneniyaOfficial/videos",
    ),
];

const YTDLP_ARGS: &[&str] = &[
    // Пользовательский конфиг yt-dlp может содержать чужие куки, прокси и
    // пути вывода. Боту он не нужен, поэтому конфиг не читается вообще.
    "--ignore-config",
    "--flat-playlist",
    "--quiet",
    "--no-warnings",
    "--skip-download",
];
const TAB_TIMEOUT: Duration = Duration::from_secs(120);
/// Каналы меняются медленно: раз в двенадцать часов сеть дёргается реже,
/// а кэш на диске переживает рестарт.
const REFRESH_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// Потолок на вкладку: канал на 100k роликов не должен съедать память.
const MAX_IDS_PER_TAB: usize = 5_000;

pub type Entries = HashMap<String, Vec<String>>;

pub struct YoutubeCache {
    path: std::path::PathBuf,
    entries: RwLock<Entries>,
    /// Время изменения файла на момент `load`. Фоновая задача сравнивает его
    /// с интервалом обновления, чтобы не ходить в сеть сразу после старта,
    /// если кэш уже свежий.
    loaded_mtime: Option<SystemTime>,
}

impl YoutubeCache {
    pub async fn load(path: std::path::PathBuf) -> Self {
        let mut entries = Entries::new();
        let mut loaded_mtime = None;
        let mut file_present = false;

        match tokio::fs::metadata(&path).await {
            Ok(meta) => {
                file_present = true;
                loaded_mtime = meta.modified().ok();
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!("кэш YouTube: не удалось открыть {}: {e}", path.display()),
        }

        if file_present {
            match tokio::fs::File::open(&path).await {
                Ok(mut file) => {
                    let mut buf = String::new();
                    match file.read_to_string(&mut buf).await {
                        Err(e) => {
                            warn!("кэш YouTube: не удалось прочитать {}: {e}", path.display())
                        }
                        Ok(_) => match serde_json::from_str::<Entries>(&buf) {
                            Ok(map) => {
                                // Файл мог быть испорчен вручную: в кэш
                                // попадают только настоящие id, без дублей и
                                // сверх потолка на вкладку.
                                entries = sanitize_entries(map);
                                info!("кэш YouTube: загружено {} вкладок", entries.len());
                            }
                            Err(e) => {
                                warn!("кэш YouTube: некорректный JSON в {}: {e}", path.display())
                            }
                        },
                    }
                }
                Err(e) => warn!("кэш YouTube: не удалось открыть {}: {e}", path.display()),
            }
        }

        Self {
            path,
            entries: RwLock::new(entries),
            loaded_mtime,
        }
    }

    /// Чтение одной вкладки целиком. Для случайной выдачи есть
    /// `random_id_from`: он не копирует векторы.
    #[allow(dead_code)]
    pub async fn get(&self, key: &str) -> Vec<String> {
        self.entries
            .read()
            .await
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Один равновероятный id из объединения вкладок. Всё чтение идёт под
    /// одной блокировкой, векторы не копируются: выбирается индекс, а
    /// клонируется только победитель.
    pub async fn random_id_from(&self, keys: &[&str]) -> Option<String> {
        let entries = self.entries.read().await;
        let total: usize = keys
            .iter()
            .filter_map(|key| entries.get(*key))
            .map(Vec::len)
            .sum();
        if total == 0 {
            return None;
        }

        let mut pick = rand::rng().random_range(0..total);
        for key in keys {
            let Some(ids) = entries.get(*key) else {
                continue;
            };
            if pick < ids.len() {
                return Some(ids[pick].clone());
            }
            pick -= ids.len();
        }
        None
    }

    /// Один проход по всем вкладкам. Никогда не паникует; упавшая вкладка
    /// оставляет прошлые данные, пустой ответ не затирает кэш.
    async fn refresh_once(&self) {
        let current = self.entries.read().await.clone();
        let mut results = Vec::with_capacity(TABS.len());
        for (key, url) in TABS {
            results.push((
                (*key).to_string(),
                fetch_tab(url).await.map_err(|e| e.to_string()),
            ));
        }

        let (merged, changed, problems) = merge_refresh(&current, &results);

        if changed {
            *self.entries.write().await = merged;
            if let Err(e) = self.persist().await {
                warn!(
                    "кэш YouTube: не удалось сохранить {}: {e}",
                    self.path.display()
                );
            }
        }

        let counts = {
            let entries = self.entries.read().await;
            TABS.iter()
                .map(|(key, _)| format!("{key}={}", entries.get(*key).map_or(0, Vec::len)))
                .collect::<Vec<_>>()
                .join(", ")
        };
        if problems.is_empty() {
            info!("кэш YouTube: обновлён ({counts})");
        } else {
            warn!(
                "кэш YouTube: обновлён с ошибками ({counts}); {}",
                problems.join("; ")
            );
        }
    }

    /// Атомарная запись: временный файл рядом, затем rename.
    async fn persist(&self) -> Result<(), std::io::Error> {
        let snapshot = self.entries.read().await.clone();
        // Компактный JSON: файл читается только этим процессом, отступы
        // здесь лишние байты на диск и время записи.
        let json = serde_json::to_vec(&snapshot)?;
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, &self.path).await
    }
}

/// Слияние результатов одного прохода с текущим кэшем.
///
/// Возвращает новый набор вкладок, изменился ли он и список проблем для
/// журнала. Правила те же, что были внутри `refresh_once`: непустой `Ok`
/// заменяет вкладку, пустой `Ok` и `Err` оставляют прошлые данные, но
/// попадают в проблемы.
pub fn merge_refresh(
    current: &Entries,
    results: &[(String, Result<Vec<String>, String>)],
) -> (Entries, bool, Vec<String>) {
    let mut merged = current.clone();
    let mut changed = false;
    let mut problems = Vec::new();

    for (key, result) in results {
        match result {
            Ok(ids) if !ids.is_empty() => {
                if merged.get(key) != Some(ids) {
                    changed = true;
                }
                merged.insert(key.clone(), ids.clone());
            }
            Ok(_) => problems.push(format!("{key}: пусто")),
            Err(e) => problems.push(format!("{key}: {e}")),
        }
    }

    (merged, changed, problems)
}

/// Фоновое обновление: первый проход после короткой паузы, чтобы старт
/// бота наступил раньше сети, затем раз в двенадцать часов. Если файл кэша
/// моложе интервала, стартовый проход откладывается: рестарт не должен
/// каждый раз дёргать yt-dlp по шести вкладкам.
pub fn spawn_refresh(cache: std::sync::Arc<YoutubeCache>) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Some(age) = cache.loaded_mtime.and_then(|at| at.elapsed().ok()) {
            if age < REFRESH_INTERVAL {
                info!("кэш YouTube: файл моложе {REFRESH_INTERVAL:?}, обновление отложено");
                tokio::time::sleep(REFRESH_INTERVAL - age).await;
            }
        }
        loop {
            cache.refresh_once().await;
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

/// Окружение для `yt-dlp`: процесс не должен видеть токен бота и прочие
/// переменные. Оставляем только то, без чего не работает запуск сети,
/// временных файлов и локали.
fn ytdlp_env() -> [(&'static str, std::ffi::OsString); 4] {
    fn or(key: &str, fallback: &str) -> std::ffi::OsString {
        std::env::var_os(key).unwrap_or_else(|| std::ffi::OsString::from(fallback))
    }
    [
        ("PATH", or("PATH", "/usr/local/bin:/usr/bin:/bin")),
        ("HOME", or("HOME", "/tmp")),
        ("LANG", or("LANG", "C.UTF-8")),
        ("TMPDIR", or("TMPDIR", "/tmp")),
    ]
}

async fn fetch_tab(url: &str) -> anyhow::Result<Vec<String>> {
    let mut command = Command::new("yt-dlp");
    command
        .args(YTDLP_ARGS)
        .args(["--print", "%(id)s"])
        .arg(url)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in ytdlp_env() {
        command.env(key, value);
    }

    let child = command
        .spawn()
        .with_context(|| format!("yt-dlp не запущен для {url}"))?;

    let output = match tokio::time::timeout(TAB_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => anyhow::bail!("yt-dlp упал для {url}: {e}"),
        // на таймауте future дропается вместе с child, kill_on_drop убивает процесс
        Err(_) => anyhow::bail!("yt-dlp не уложился в {TAB_TIMEOUT:?} для {url}"),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("yt-dlp вернул {}: {}", output.status, last_line(&stderr));
    }

    Ok(parse_ids(&String::from_utf8_lossy(&output.stdout)))
}

/// Проверка прочитанного с диска: только настоящие id, без дублей и не
/// больше потолка на вкладку. Испорченный файл не должен раздувать память
/// или отдавать в Telegram мусорные ссылки.
fn sanitize_entries(entries: Entries) -> Entries {
    entries
        .into_iter()
        .map(|(key, ids)| {
            let mut seen = HashSet::new();
            let ids = ids
                .into_iter()
                .filter(|id| is_video_id(id))
                .filter(|id| seen.insert(id.clone()))
                .take(MAX_IDS_PER_TAB)
                .collect();
            (key, ids)
        })
        .collect()
}

/// Разбор вывода `--print %(id)s`: дедупликация, отсев мусора и потолок.
pub fn parse_ids(stdout: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut ids = Vec::new();
    for line in stdout.lines().map(str::trim) {
        if !is_video_id(line) {
            continue;
        }
        if seen.insert(line.to_string()) {
            ids.push(line.to_string());
        }
        if ids.len() >= MAX_IDS_PER_TAB {
            break;
        }
    }
    ids
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn last_line(s: &str) -> &str {
    s.lines().last().unwrap_or("<нет вывода>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dedups_and_drops_junk() {
        let stdout = "dQw4w9WgXcQ\n\n  dQw4w9WgXcQ \nnot-an-id\nAAAAAAAAAAA\n";
        assert_eq!(parse_ids(stdout), vec!["dQw4w9WgXcQ", "AAAAAAAAAAA"]);
    }

    #[test]
    fn respects_cap() {
        let stdout = (0..MAX_IDS_PER_TAB + 10)
            .map(|i| format!("{:011}", i % 10_000_000_000))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_ids(&stdout).len() <= MAX_IDS_PER_TAB);
    }

    #[test]
    fn load_filters_invalid_duplicate_and_oversized_tabs() {
        let mut raw = Entries::new();
        raw.insert(
            "batek:shorts".to_string(),
            vec![
                "dQw4w9WgXcQ".to_string(),
                "dQw4w9WgXcQ".to_string(),
                "not-an-id".to_string(),
                "AAAAAAAAAAA".to_string(),
                "слишком длинный id".to_string(),
            ],
        );
        raw.insert(
            "timur:videos".to_string(),
            (0..MAX_IDS_PER_TAB + 50)
                .map(|i| format!("{:011}", i % 10_000_000_000))
                .collect(),
        );

        let clean = sanitize_entries(raw);

        assert_eq!(
            clean.get("batek:shorts"),
            Some(&ids(&["dQw4w9WgXcQ", "AAAAAAAAAAA"]))
        );
        assert_eq!(
            clean.get("timur:videos").map_or(0, Vec::len),
            MAX_IDS_PER_TAB,
            "потолок вкладки обязан действовать и на диск"
        );
    }

    #[tokio::test]
    async fn random_id_from_uses_all_keys_under_one_lock() {
        let cache = YoutubeCache {
            path: std::path::PathBuf::from("unused.json"),
            entries: RwLock::new(entries(&[
                ("batek:shorts", &["AAAAAAAAAAA"]),
                ("timur:videos", &["dQw4w9WgXcQ"]),
            ])),
            loaded_mtime: None,
        };

        let mut seen = HashSet::new();
        for _ in 0..200 {
            let id = cache
                .random_id_from(&["batek:shorts", "timur:videos"])
                .await
                .expect("id есть");
            seen.insert(id);
        }
        assert_eq!(
            seen,
            HashSet::from(["AAAAAAAAAAA".to_string(), "dQw4w9WgXcQ".to_string()])
        );
        assert_eq!(cache.random_id_from(&["missing"]).await, None);
    }

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_string()).collect()
    }

    fn entries(pairs: &[(&str, &[&str])]) -> Entries {
        pairs
            .iter()
            .map(|(key, values)| ((*key).to_string(), ids(values)))
            .collect()
    }

    #[test]
    fn merge_fills_empty_cache_on_first_run() {
        let current = Entries::new();
        let results = vec![(
            "batek:shorts".to_string(),
            Ok(ids(&["dQw4w9WgXcQ", "AAAAAAAAAAA"])),
        )];

        let (merged, changed, problems) = merge_refresh(&current, &results);

        assert!(changed);
        assert!(problems.is_empty());
        assert_eq!(
            merged.get("batek:shorts"),
            Some(&ids(&["dQw4w9WgXcQ", "AAAAAAAAAAA"]))
        );
        assert!(current.is_empty(), "входной кэш не меняется");
    }

    #[test]
    fn merge_keeps_old_data_for_failed_tab() {
        let current = entries(&[("batek:videos", &["old1", "old2"])]);
        let results = vec![(
            "batek:videos".to_string(),
            Err("yt-dlp упал: таймаут".to_string()),
        )];

        let (merged, changed, problems) = merge_refresh(&current, &results);

        assert!(!changed);
        assert_eq!(merged, current);
        assert_eq!(problems, vec!["batek:videos: yt-dlp упал: таймаут"]);
    }

    #[test]
    fn merge_keeps_old_data_for_empty_result() {
        let current = entries(&[("timur:shorts", &["old1"])]);
        let results = vec![("timur:shorts".to_string(), Ok(Vec::new()))];

        let (merged, changed, problems) = merge_refresh(&current, &results);

        assert!(!changed);
        assert_eq!(merged, current);
        assert_eq!(problems, vec!["timur:shorts: пусто"]);
    }

    #[test]
    fn merge_reports_no_change_when_results_match() {
        let current = entries(&[
            ("batek:shorts", &["a1", "a2"]),
            ("victorovich:videos", &["b1"]),
        ]);
        let results = vec![
            ("batek:shorts".to_string(), Ok(ids(&["a1", "a2"]))),
            ("victorovich:videos".to_string(), Ok(ids(&["b1"]))),
        ];

        let (merged, changed, problems) = merge_refresh(&current, &results);

        assert!(!changed);
        assert!(problems.is_empty());
        assert_eq!(merged, current);
    }

    #[test]
    fn merge_applies_good_tabs_while_failed_tab_stays_old() {
        let current = entries(&[("batek:shorts", &["old"]), ("timur:videos", &["keep"])]);
        let results = vec![
            ("batek:shorts".to_string(), Ok(ids(&["new"]))),
            (
                "timur:videos".to_string(),
                Err("сеть недоступна".to_string()),
            ),
            ("victorovich:shorts".to_string(), Ok(Vec::new())),
        ];

        let (merged, changed, problems) = merge_refresh(&current, &results);

        assert!(changed);
        assert_eq!(merged.get("batek:shorts"), Some(&ids(&["new"])));
        assert_eq!(merged.get("timur:videos"), Some(&ids(&["keep"])));
        assert_eq!(merged.len(), 2, "пустая вкладка не добавляется в кэш");
        assert_eq!(
            problems,
            vec!["timur:videos: сеть недоступна", "victorovich:shorts: пусто"]
        );
    }

    /// Фикстура это реальный вывод yt-dlp, записанный с живого вызова
    /// (правило ворот 1: фикстуры не синтетические).
    #[test]
    fn parses_recorded_live_output() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/yt_playlist.txt");
        let Ok(stdout) = std::fs::read_to_string(&path) else {
            eprintln!("НЕ ВЫПОЛНЕНО: нет фикстуры {}", path.display());
            return;
        };
        let ids = parse_ids(&stdout);
        assert!(
            !ids.is_empty(),
            "живой вывод yt-dlp не должен парситься в пусто"
        );
        assert!(ids.iter().all(|id| is_video_id(id)));
        assert_eq!(ids.len(), ids.iter().collect::<HashSet<_>>().len());
    }
}

//! Мемы из уже существующих каталогов `video/` и `music/`.
//!
//! Отдельного каталога нет: имя мема это stem файла, поэтому
//! `video/bomjara1.mp4` и `music/Тёмный принц - Овердоз.mp3` доступны как
//! `bomjara1` и `Тёмный принц - Овердоз`. Один и тот же stem в двух каталогах
//! даёт один мем: видео важнее аудио, в остальных случаях побеждает первый
//! найденный файл. Список читается один раз при старте: файлы меняются только
//! пересборкой образа.

use std::path::{Path, PathBuf};

use rand::seq::IndexedRandom;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemeKind {
    Image,
    Video,
    Audio,
}

impl MemeKind {
    /// Расширение определяет способ отправки в Telegram. Неизвестные
    /// расширения не мемы и в список не попадают.
    fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "mov" | "webm" => Some(Self::Video),
            "jpg" | "jpeg" | "png" | "gif" | "webp" => Some(Self::Image),
            "mp3" | "ogg" | "wav" => Some(Self::Audio),
            _ => None,
        }
    }
}

/// `Clone`: фоновая inline-догрузка забирает себе владение копией мема.
/// `Debug`: вывод в assert о книге учёта загрузок.
#[derive(Clone, Debug)]
pub struct Meme {
    pub name: String,
    pub path: PathBuf,
    pub kind: MemeKind,
}

pub struct Memes {
    items: Vec<Meme>,
}

impl Memes {
    /// Скан `video/`, затем `music/`. Порядок важен: при совпадении имени
    /// видео из `video/` уже лежит в списке, а видео-дубль вытесняет аудио.
    pub fn scan(root: &Path) -> Self {
        let mut items: Vec<Meme> = Vec::new();

        for dir in ["video", "music"] {
            let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let Some(kind) = MemeKind::from_path(&path) else {
                    continue;
                };

                if let Some(existing) = items.iter_mut().find(|m| m.name == name) {
                    if kind == MemeKind::Video && existing.kind == MemeKind::Audio {
                        existing.path = path;
                        existing.kind = kind;
                    }
                    continue;
                }

                items.push(Meme {
                    name: name.to_string(),
                    path,
                    kind,
                });
            }
        }

        items.sort_by(|a, b| a.name.cmp(&b.name));
        Self { items }
    }

    /// Сначала точное совпадение по нормализованному ключу, затем вхождение,
    /// затем опечатка. Пустой запрос не мем: иначе `find("")` вернул бы
    /// первый файл.
    pub fn find(&self, query: &str) -> Option<&Meme> {
        let needle = meme_key(query);
        if needle.is_empty() {
            return None;
        }

        // Точный ключ важнее опечатки: запрос `бомжара` (ключ bomjara)
        // обязан выбрать bomjara1, а не похожий bomjaro2 с расстоянием 1.
        // Inline-выдача может показывать оба, но `find` выбирает один.
        if let Some(exact) = self
            .items
            .iter()
            .find(|meme| meme_key(&meme.name) == needle)
        {
            return Some(exact);
        }

        self.matches(query).into_iter().next()
    }

    /// Ранжированные совпадения: точный ключ, вхождение (от трёх символов),
    /// затем ограниченное расстояние Левенштейна. Ранжирование нужно inline:
    /// `бомжара` должна показать `bomjara1` раньше `bomjaro2`.
    pub fn matches(&self, query: &str) -> Vec<&Meme> {
        let needle = meme_key(query);
        if needle.is_empty() {
            return Vec::new();
        }

        let needle_len = needle.chars().count();
        let limit = if needle_len < 8 { 1 } else { 2 };
        let mut exact = Vec::new();
        let mut contains = Vec::new();
        let mut fuzzy = Vec::new();

        for meme in &self.items {
            let key = meme_key(&meme.name);
            if key == needle {
                exact.push(meme);
            } else if needle_len >= 3 && key.contains(&needle) {
                contains.push(meme);
            } else if edit_distance_within(&key, &needle, limit) {
                fuzzy.push(meme);
            }
        }

        exact.extend(contains);
        exact.extend(fuzzy);
        exact
    }

    pub fn random(&self) -> Option<&Meme> {
        self.items.choose(&mut rand::rng())
    }

    pub fn names(&self) -> Vec<String> {
        self.items.iter().map(|m| m.name.clone()).collect()
    }

    pub fn items(&self) -> &[Meme] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

/// Кириллица в канонический латинский вид. Имена файлов приходят и на
/// латинице, и на кириллице, поэтому запрос не должен зависеть от алфавита.
fn transliterate(ch: char) -> Option<char> {
    Some(match ch {
        'а' => 'a',
        'б' => 'b',
        'в' => 'v',
        'г' => 'g',
        'д' => 'd',
        'е' | 'ё' | 'э' => 'e',
        'ж' => 'j',
        'з' => 'z',
        'и' | 'й' | 'ы' => 'i',
        'к' => 'k',
        'л' => 'l',
        'м' => 'm',
        'н' => 'n',
        'о' => 'o',
        'п' => 'p',
        'р' => 'r',
        'с' => 's',
        'т' => 't',
        'у' => 'u',
        'ф' => 'f',
        'х' => 'h',
        'ц' | 'ч' => 'c',
        'ш' | 'щ' => 's',
        'ъ' | 'ь' => return None,
        'ю' => 'u',
        'я' => 'a',
        other => other,
    })
}

/// Ключ для сравнения: нижний регистр, транслитерация, только ASCII-буквы и
/// цифры. Хвостовые цифры отбрасываются: `bomjara1` и `bomjara2` должны
/// находиться по запросу `бомжара`, а цифры внутри имени остаются значимыми.
pub fn meme_key(text: &str) -> String {
    let key = normalized_chars(text);
    key.trim_end_matches(|ch: char| ch.is_ascii_digit())
        .to_string()
}

/// Нормализованные слова текста. Цифры на конце не отбрасываются: при
/// сравнении ключевых слов цифра остаётся частью слова, иначе `бомжара2024`
/// ложно совпала бы с `бомжара`.
pub fn normalized_words(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(normalized_chars)
        .filter(|word| !word.is_empty())
        .collect()
}

fn normalized_chars(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter_map(transliterate)
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

/// Ограниченное расстояние Левенштейна: если строка уже дальше лимита,
/// дальше считать незачем. Своя реализация, без новой зависимости.
fn edit_distance_within(left: &str, right: &str, limit: usize) -> bool {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    if left.len().abs_diff(right.len()) > limit {
        return false;
    }

    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];

    for (row, left_ch) in left.iter().enumerate() {
        current[0] = row + 1;
        let mut best = current[0];
        for (col, right_ch) in right.iter().enumerate() {
            let cost = usize::from(left_ch != right_ch);
            current[col + 1] = (previous[col] + cost)
                .min(previous[col + 1] + 1)
                .min(current[col] + 1);
            best = best.min(current[col + 1]);
        }
        // Минимум строки не убывает: полное расстояние не может стать
        // меньше уже посчитанного минимума.
        if best > limit {
            return false;
        }
        std::mem::swap(&mut previous, &mut current);
    }

    previous[right.len()] <= limit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-memes-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("video")).unwrap();
        std::fs::create_dir_all(dir.join("music")).unwrap();
        dir
    }

    #[test]
    fn video_wins_over_audio_and_other_duplicates_keep_first() {
        let root = temp_root("dups");
        std::fs::write(root.join("video/a.mp4"), b"v").unwrap();
        std::fs::write(root.join("music/a.mp3"), b"a").unwrap();
        std::fs::write(root.join("video/b.mp4"), b"v").unwrap();
        std::fs::write(root.join("video/b.webm"), b"v").unwrap();
        std::fs::write(root.join("music/c.mp3"), b"a").unwrap();
        std::fs::write(root.join("video/readme.txt"), b"x").unwrap();

        let memes = Memes::scan(&root);
        let names = memes.names();
        assert_eq!(names, vec!["a", "b", "c"]);

        let a = memes.find("a").unwrap();
        assert_eq!(a.kind, MemeKind::Video, "видео важнее аудио");
        assert!(a.path.ends_with("video/a.mp4"));
        // Внутри одного каталога порядок read_dir не гарантирован, поэтому
        // проверяется только то, что дубль не добавился вторым мемом.
        assert_eq!(memes.find("b").unwrap().kind, MemeKind::Video);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_is_case_insensitive_and_falls_back_to_substring() {
        let root = temp_root("find");
        std::fs::write(root.join("music/Track.MP3"), b"a").unwrap();

        let memes = Memes::scan(&root);
        assert_eq!(memes.find("track").unwrap().name, "Track");
        assert_eq!(memes.find("TRACK").unwrap().name, "Track");
        assert_eq!(memes.find("rac").unwrap().name, "Track");
        assert_eq!(memes.find("no-such").map(|m| m.name.as_str()), None);
        assert_eq!(memes.find("   ").map(|m| m.name.as_str()), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cyrillic_queries_find_latin_and_cyrillic_names() {
        let root = temp_root("cross-alphabet");
        std::fs::write(root.join("video/bomjara1.mp4"), b"v").unwrap();
        std::fs::write(root.join("video/bomjaro2.mp4"), b"v").unwrap();
        std::fs::write(root.join("music/Ласковый Май - Белые Розы.mp3"), b"a").unwrap();
        std::fs::write(root.join("music/Григорий Лепс - Натали.mp3"), b"a").unwrap();

        let memes = Memes::scan(&root);

        // Опечатка в имени файла: точный ключ bomjara1 идёт первым, но
        // bomjaro2 тоже попадает в выдачу.
        let ranked: Vec<&str> = memes
            .matches("бомжара")
            .iter()
            .map(|meme| meme.name.as_str())
            .collect();
        assert_eq!(ranked.first(), Some(&"bomjara1"), "{ranked:?}");
        assert!(ranked.contains(&"bomjaro2"), "{ranked:?}");
        assert_eq!(
            memes.find("бомжара").map(|meme| meme.name.as_str()),
            Some("bomjara1")
        );
        assert_eq!(
            memes.find("BOMJARA").map(|meme| meme.name.as_str()),
            Some("bomjara1")
        );

        // Кириллический запрос находит кириллическое имя.
        assert_eq!(
            memes.find("белые розы").map(|meme| meme.name.as_str()),
            Some("Ласковый Май - Белые Розы")
        );
        assert_eq!(
            memes.find("натали").map(|meme| meme.name.as_str()),
            Some("Григорий Лепс - Натали")
        );

        // Латинский запрос находит кириллическое имя и наоборот.
        assert_eq!(
            memes.find("laskovii").map(|meme| meme.name.as_str()),
            Some("Ласковый Май - Белые Розы")
        );
        assert_eq!(
            memes.find("bomjara").map(|meme| meme.name.as_str()),
            Some("bomjara1")
        );

        // Несвязанный запрос не должен цепляться к этим файлам.
        assert_eq!(memes.find("сравнение").map(|meme| meme.name.as_str()), None);
        assert!(memes.matches("сравнение").is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn transliteration_covers_cyrillic_letters() {
        for (input, expected) in [
            ("ёлка", "elka"),
            ("йод", "iod"),
            ("юг", "ug"),
            ("яма", "ama"),
            ("жук", "juk"),
            ("хлеб", "hleb"),
            ("цирк", "cirk"),
            ("щель", "sel"),
            ("час", "cas"),
            ("шаг", "sag"),
            ("Съезд", "sezd"),
        ] {
            assert_eq!(meme_key(input), expected, "{input}");
        }

        // Хвостовые цифры отбрасываются, цифры в середине остаются.
        assert_eq!(meme_key("bomjara1"), "bomjara");
        assert_eq!(meme_key("bomjaro2"), "bomjaro");
        assert_eq!(meme_key("m2024x"), "m2024x");
    }

    #[test]
    fn classifies_by_extension_case_insensitively() {
        for (file, kind) in [
            ("a.mp4", MemeKind::Video),
            ("a.MOV", MemeKind::Video),
            ("a.webm", MemeKind::Video),
            ("a.jpg", MemeKind::Image),
            ("a.JPEG", MemeKind::Image),
            ("a.png", MemeKind::Image),
            ("a.gif", MemeKind::Image),
            ("a.webp", MemeKind::Image),
            ("a.mp3", MemeKind::Audio),
            ("a.OGG", MemeKind::Audio),
            ("a.wav", MemeKind::Audio),
        ] {
            assert_eq!(
                MemeKind::from_path(Path::new(file)),
                Some(kind),
                "расширение {file}"
            );
        }
        assert_eq!(MemeKind::from_path(Path::new("a.txt")), None);
        assert_eq!(MemeKind::from_path(Path::new("noext")), None);
    }
}

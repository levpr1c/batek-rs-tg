//! Один матчер на все реакции бота, весь процесс живёт в Telegram.
//!
//! Лимиты живут в `KeywordLimiter`: ключ — идентификатор чата, а карта
//! создана вместе с ботом и не переживает перезапуск.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use tokio::sync::RwLock;

use crate::memes::{meme_key, normalized_words, Meme, Memes};

/// Не чаще одного ответа на фразу в одном чате.
pub const KEYWORD_RATE_LIMIT: Duration = Duration::from_secs(20);

/// Записи старше этого срока удаляются при следующей проверке: карта не
/// растёт на долгоживущих каналах и группах.
pub const KEYWORD_PRUNE_AFTER: Duration = Duration::from_secs(10 * 60);

/// Ключевые слова для реакции в группах. Список расширяется здесь; каждая
/// строка нормализуется тем же матчером, что и имена мемов, поэтому запрос
/// не зависит от алфавита и написания.
pub const KEYWORD_REACTIONS: [(&str, KeywordAction); 29] = [
    ("гранд мобайл", KeywordAction::Grandmobile),
    ("грандмобайл", KeywordAction::Grandmobile),
    ("grandmobile", KeywordAction::Grandmobile),
    ("grand mobile", KeywordAction::Grandmobile),
    ("белые розы", KeywordAction::Meme("Белые Розы")),
    ("belye rozy", KeywordAction::Meme("Белые Розы")),
    ("belye-rozy", KeywordAction::Meme("Белые Розы")),
    // Бомжара как /bomjara: один из двух роликов, а не всегда первый.
    (
        "бомжара",
        KeywordAction::RandomMeme(&["bomjara", "bomjaro"]),
    ),
    ("бомж", KeywordAction::RandomMeme(&["bomjara", "bomjaro"])),
    (
        "bomjara",
        KeywordAction::RandomMeme(&["bomjara", "bomjaro"]),
    ),
    (
        "bomzhara",
        KeywordAction::RandomMeme(&["bomjara", "bomjaro"]),
    ),
    // Отдельные слова, а не опечатки: fuzzy-матчер их не выведет.
    (
        "manjaro",
        KeywordAction::RandomMeme(&["bomjara", "bomjaro"]),
    ),
    (
        "манжара",
        KeywordAction::RandomMeme(&["bomjara", "bomjaro"]),
    ),
    // makuos.mp4: транслитерация не выводит эти формы друг из друга,
    // поэтому синонимы перечислены явно.
    ("макуос", KeywordAction::Meme("makuos")),
    ("makuos", KeywordAction::Meme("makuos")),
    ("марк адерли", KeywordAction::Meme("makuos")),
    ("марк", KeywordAction::Meme("makuos")),
    ("mark aderli", KeywordAction::Meme("makuos")),
    ("mark", KeywordAction::Meme("makuos")),
    // Фамилия без имени: целым словом, а не подстрокой, поэтому
    // перечислена явно.
    ("аддерли", KeywordAction::Meme("makuos")),
    ("adderli", KeywordAction::Meme("makuos")),
    // shefos.mp4: "320" это отдельное слово, а не подстрока; границу слова
    // и лимит в 20 секунд держит общий матчер.
    ("шефос", KeywordAction::Meme("shefos")),
    ("shefos", KeywordAction::Meme("shefos")),
    ("шеф", KeywordAction::Meme("shefos")),
    ("пингвин320кг", KeywordAction::Meme("shefos")),
    ("пингвин320", KeywordAction::Meme("shefos")),
    ("pingvin320kg", KeywordAction::Meme("shefos")),
    ("pingvin320", KeywordAction::Meme("shefos")),
    ("320", KeywordAction::Meme("shefos")),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeywordAction {
    Grandmobile,
    /// Первый мем, имя которого содержит эту подстроку без учёта регистра.
    Meme(&'static str),
    /// Случайный мем из всех, чьё имя содержит любую из подстрок.
    RandomMeme(&'static [&'static str]),
}

/// Ищет ключевую фразу по нормализованным словам: регистр, алфавит и
/// дефисы не важны, но фраза должна занимать целые слова. Поэтому
/// `небомжара` не совпадает, а `bomzhara` совпадает с `бомжара`.
pub fn match_keyword(text: &str) -> Option<(&'static str, KeywordAction)> {
    let words = normalized_words(text);
    if words.is_empty() {
        return None;
    }

    KEYWORD_REACTIONS.iter().copied().find(|(phrase, _)| {
        let needle = normalized_words(phrase);
        if needle.is_empty() || needle.len() > words.len() {
            return false;
        }
        let whole = needle.concat();
        words
            .windows(needle.len())
            .any(|window| window.concat() == whole)
    })
}

/// Запрос к той же таблице одной строкой: слова склеиваются, границы
/// сознательно теряются, потому что inline-запрос приходит по мере набора.
fn normalized_query(text: &str) -> String {
    normalized_words(text).concat()
}

/// Свободный запрос inline к той же таблице, что и chat-реакции. Точный
/// нормализованный матч сильнее префикса, поэтому совпадение по таблице
/// исключает поиск по именам мемов. Префикс работает от двух символов:
/// "ман" уже приводит к bomjara, пока пользователь допечатывает. Из равных
/// длин побеждает первая запись таблицы, порядок стабилен.
pub fn match_keyword_prefix(query: &str) -> Option<(&'static str, KeywordAction)> {
    let needle = normalized_query(query);
    if needle.is_empty() {
        return None;
    }

    // Точный нормализованный матч важнее префикса: "грандмобайл" обязан
    // прийти к одноимённой записи, а не через префикс.
    if let Some(exact) = KEYWORD_REACTIONS
        .iter()
        .copied()
        .find(|(phrase, _)| normalized_query(phrase) == needle)
    {
        return Some(exact);
    }
    if needle.chars().count() < 2 {
        return None;
    }

    let mut best: Option<(usize, (&'static str, KeywordAction))> = None;
    for entry in KEYWORD_REACTIONS.iter().copied() {
        let normalized = normalized_query(entry.0);
        if normalized.starts_with(&needle) {
            let len = normalized.len();
            if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                best = Some((len, entry));
            }
        }
    }
    best.map(|(_, entry)| entry)
}

/// Все мемы, чьё имя содержит любую из подстрок. Кандидаты нужны, чтобы
/// `бомжара` выбирала между bomjara1 и bomjaro2, как `/bomjara`.
pub fn random_meme_candidates<'a>(memes: &'a Memes, needles: &[&str]) -> Vec<&'a Meme> {
    let needles: Vec<String> = needles.iter().map(|needle| meme_key(needle)).collect();
    let mut candidates: Vec<&Meme> = Vec::new();
    for meme in memes.items() {
        let key = meme_key(&meme.name);
        if needles.iter().any(|needle| key.contains(needle.as_str()))
            && !candidates.iter().any(|known| known.name == meme.name)
        {
            candidates.push(meme);
        }
    }
    candidates
}

/// Один равновероятный кандидат. `partial_shuffle` выбирает случайный
/// элемент, не трогая остальные.
pub fn pick_random_meme<'a>(candidates: &mut [&'a Meme]) -> Option<&'a Meme> {
    let (picked, _) = candidates.partial_shuffle(&mut rand::rng(), 1);
    picked.first().copied()
}

/// Лимит ответов на ключевые слова. Ключ это id чата и сама фраза: лимиты
/// чатов не смешиваются, а карта живёт вместе с процессом бота.
#[derive(Default)]
pub struct KeywordLimiter {
    last: RwLock<HashMap<(i64, &'static str), Instant>>,
}

impl KeywordLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Проверяет и обновляет лимит: не чаще одного ответа на фразу в чат за
    /// `KEYWORD_RATE_LIMIT`. Попутно чистит старые записи.
    pub async fn allow(&self, chat: i64, phrase: &'static str) -> bool {
        let mut last = self.last.write().await;
        last.retain(|_, at| at.elapsed() < KEYWORD_PRUNE_AFTER);

        let key = (chat, phrase);
        if let Some(at) = last.get(&key) {
            if at.elapsed() < KEYWORD_RATE_LIMIT {
                return false;
            }
        }
        last.insert(key, Instant::now());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-keywords-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("video")).unwrap();
        std::fs::create_dir_all(dir.join("music")).unwrap();
        dir
    }

    #[test]
    fn shared_table_covers_every_alias() {
        for (phrase, expected) in KEYWORD_REACTIONS {
            let (_, action) =
                match_keyword(phrase).unwrap_or_else(|| panic!("нет реакции на {phrase}"));
            assert_eq!(action, expected, "{phrase}");
        }
    }

    #[test]
    fn matching_is_alphabet_agnostic_and_honours_whole_words() {
        let bomjara = KeywordAction::RandomMeme(&["bomjara", "bomjaro"]);
        for text in [
            "бомжара",
            "бомж",
            "bomjara",
            "bomzhara",
            "BOMZHARA",
            "manjaro",
            "манжара",
        ] {
            assert_eq!(
                match_keyword(text).map(|(_, action)| action),
                Some(bomjara),
                "{text}"
            );
        }

        for text in ["белые розы", "belye rozy", "Belye-Rozy"] {
            assert_eq!(
                match_keyword(text).map(|(_, action)| action),
                Some(KeywordAction::Meme("Белые Розы")),
                "{text}"
            );
        }

        for text in ["гранд мобайл", "грандмобайл", "grandmobile", "grand mobile"]
        {
            assert_eq!(
                match_keyword(text).map(|(_, action)| action),
                Some(KeywordAction::Grandmobile),
                "{text}"
            );
        }

        for text in [
            "небомжара",
            "xbomjara",
            "belye rozyk",
            "мегаграндмобайл",
            "бомжара2024",
            "1320",
        ] {
            assert_eq!(match_keyword(text), None, "{text}");
        }
    }

    #[test]
    fn bare_320_matches_only_as_a_word() {
        assert_eq!(
            match_keyword("320").map(|(_, action)| action),
            Some(KeywordAction::Meme("shefos"))
        );
        assert_eq!(
            match_keyword("скинь 320").map(|(_, action)| action),
            Some(KeywordAction::Meme("shefos"))
        );
        assert_eq!(match_keyword("1320"), None);
        assert_eq!(match_keyword("320кг"), None);
    }

    #[test]
    fn synonyms_resolve_to_their_files() {
        let root = temp_root("synonyms");
        for file in ["bomjara1.mp4", "bomjaro2.mp4", "makuos.mp4", "shefos.mp4"] {
            std::fs::write(root.join("video").join(file), b"v").unwrap();
        }
        let memes = Memes::scan(&root);

        let cases: &[(&[&str], &str)] = &[
            (
                &[
                    "макуос",
                    "makuos",
                    "марк адерли",
                    "марк",
                    "mark aderli",
                    "mark",
                ],
                "makuos",
            ),
            (
                &[
                    "шефос",
                    "shefos",
                    "шеф",
                    "пингвин320кг",
                    "пингвин320",
                    "pingvin320kg",
                    "pingvin320",
                    "320",
                ],
                "shefos",
            ),
        ];

        for (texts, expected) in cases {
            for text in *texts {
                let (_, action) =
                    match_keyword(text).unwrap_or_else(|| panic!("нет реакции на {text}"));
                let needle = match action {
                    KeywordAction::Meme(needle) => needle,
                    other => panic!("{text}: не мем, а {other:?}"),
                };
                let found = memes
                    .find(needle)
                    .unwrap_or_else(|| panic!("{text}: мем {needle} не найден"));
                assert_eq!(found.name, *expected, "{text}");
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bomjara_picks_one_of_two_videos() {
        let root = temp_root("bomjara");
        for file in ["bomjara1.mp4", "bomjaro2.mp4", "makuos.mp4"] {
            std::fs::write(root.join("video").join(file), b"v").unwrap();
        }
        let memes = Memes::scan(&root);

        let action = match_keyword("бомжара")
            .expect("бомжара должна срабатывать")
            .1;
        let needles = match action {
            KeywordAction::RandomMeme(needles) => needles,
            other => panic!("ожидался RandomMeme, получено {other:?}"),
        };

        let candidates = random_meme_candidates(&memes, needles);
        let mut names: Vec<&str> = candidates.iter().map(|meme| meme.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["bomjara1", "bomjaro2"]);

        let mut picked = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let mut pool = candidates.clone();
            let meme = pick_random_meme(&mut pool).expect("кандидаты есть");
            assert!(names.contains(&meme.name.as_str()), "{}", meme.name);
            assert_eq!(pool.len(), 2);
            picked.insert(meme.name.as_str());
        }
        assert_eq!(picked.len(), 2, "{picked:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn matcher_is_one_function_for_every_caller() {
        // Никаких подмен на стороне обработчиков: reactions зовут ту же
        // функцию, что и тесты, проверяем через указатель.
        let shared: fn(&str) -> Option<(&'static str, KeywordAction)> = match_keyword;
        assert_eq!(
            shared("бомжара").map(|(_, action)| action),
            Some(KeywordAction::RandomMeme(&["bomjara", "bomjaro"]))
        );
        assert_eq!(
            shared("марк адерли").map(|(_, action)| action),
            Some(KeywordAction::Meme("makuos"))
        );
    }

    #[test]
    fn inline_prefix_tables_cover_every_alias() {
        let bomjara = KeywordAction::RandomMeme(&["bomjara", "bomjaro"]);
        for (query, expected) in [
            ("ман", bomjara),
            ("манжара", bomjara),
            ("manjaro", bomjara),
            // Имя мема совпадает с алиасом: таблица одна на чат и inline.
            ("bomjara", bomjara),
            ("аддерли", KeywordAction::Meme("makuos")),
            ("марк", KeywordAction::Meme("makuos")),
            ("makuos", KeywordAction::Meme("makuos")),
            ("пингвин320", KeywordAction::Meme("shefos")),
            ("шефос", KeywordAction::Meme("shefos")),
            ("белые", KeywordAction::Meme("Белые Розы")),
            ("гранд", KeywordAction::Grandmobile),
        ] {
            assert_eq!(
                match_keyword_prefix(query).map(|(_, action)| action),
                Some(expected),
                "{query}"
            );
        }

        // Постороннее слово и слишком короткий ввод не совпадают.
        assert_eq!(match_keyword_prefix("вполне посторонний вопрос"), None);
        assert_eq!(match_keyword_prefix("в"), None);
        assert_eq!(match_keyword_prefix(""), None);
    }

    #[test]
    fn inline_prefix_agrees_with_chat_matcher_on_every_phrase() {
        for (phrase, expected) in KEYWORD_REACTIONS {
            assert_eq!(
                match_keyword_prefix(phrase).map(|(_, action)| action),
                Some(expected),
                "inline и chat расходятся на {phrase}"
            );
            assert_eq!(
                match_keyword(phrase).map(|(_, action)| action),
                Some(expected),
                "chat не видит собственную фразу {phrase}"
            );
        }
    }
}

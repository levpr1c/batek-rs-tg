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

/// Наборы файлов для random-действий: каждое слово из группы выбирает один
/// из файлов через `random_meme_candidates`, как /bomjara выбирает из двух.
const BARANI: &[&str] = &["barani_call", "barani_perdj"];
const CATS: &[&str] = &["cat_kzn", "kzn_picture"];
const SERJ: &[&str] = &[
    "serj_2ava",
    "serj_bob",
    "serj_dead",
    "serj_thx",
    "serj_dead_distr",
];

/// Ключевые слова для реакции в группах. Список расширяется здесь; каждая
/// строка нормализуется тем же матчером, что и имена мемов, поэтому запрос
/// не зависит от алфавита и написания. Правило группы: у каждого действия
/// есть фраза и кириллицей, и ASCII (транслит или латиница), иначе тест
/// every_action_has_cyrillic_and_ascii_phrase падает.
pub const KEYWORD_REACTIONS: [(&str, KeywordAction); 142] = [
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
    // arectovich_zig: одиночное фото, зига это то самое.
    ("зига", KeywordAction::Meme("arectovich_zig")),
    ("зиг", KeywordAction::Meme("arectovich_zig")),
    ("гитлер", KeywordAction::Meme("arectovich_zig")),
    ("hitler", KeywordAction::Meme("arectovich_zig")),
    ("zig", KeywordAction::Meme("arectovich_zig")),
    ("арестович", KeywordAction::Meme("arectovich_zig")),
    ("arestovich", KeywordAction::Meme("arectovich_zig")),
    ("arectovich", KeywordAction::Meme("arectovich_zig")),
    ("украина", KeywordAction::Meme("arectovich_zig")),
    ("укр", KeywordAction::Meme("arectovich_zig")),
    ("ukr", KeywordAction::Meme("arectovich_zig")),
    ("uk", KeywordAction::Meme("arectovich_zig")),
    ("ukraine", KeywordAction::Meme("arectovich_zig")),
    // barani: случайный выбор между двумя баранами.
    ("бараны", KeywordAction::RandomMeme(BARANI)),
    ("баран", KeywordAction::RandomMeme(BARANI)),
    ("barani", KeywordAction::RandomMeme(BARANI)),
    ("vpn", KeywordAction::RandomMeme(BARANI)),
    ("впн", KeywordAction::RandomMeme(BARANI)),
    // batek_bot_issue: боль того самого Андрея, фото одно.
    ("бабушка андрея", KeywordAction::Meme("batek_bot_issue")),
    ("бабушка", KeywordAction::Meme("batek_bot_issue")),
    ("grandma", KeywordAction::Meme("batek_bot_issue")),
    ("babushka", KeywordAction::Meme("batek_bot_issue")),
    ("андрей", KeywordAction::Meme("batek_bot_issue")),
    ("andrey", KeywordAction::Meme("batek_bot_issue")),
    ("andrei", KeywordAction::Meme("batek_bot_issue")),
    // baza.
    ("база", KeywordAction::Meme("baza")),
    ("baza", KeywordAction::Meme("baza")),
    ("base", KeywordAction::Meme("baza")),
    // казан и кот: два фото, случайный выбор.
    ("казан", KeywordAction::RandomMeme(CATS)),
    ("саша", KeywordAction::RandomMeme(CATS)),
    ("сашко", KeywordAction::RandomMeme(CATS)),
    ("кот", KeywordAction::RandomMeme(CATS)),
    ("cat", KeywordAction::RandomMeme(CATS)),
    ("kzn", KeywordAction::RandomMeme(CATS)),
    ("kazan", KeywordAction::RandomMeme(CATS)),
    ("sasha", KeywordAction::RandomMeme(CATS)),
    ("поверни", KeywordAction::RandomMeme(CATS)),
    ("rotate", KeywordAction::RandomMeme(CATS)),
    // hohli.
    ("забанили", KeywordAction::Meme("hohli")),
    ("бан", KeywordAction::Meme("hohli")),
    ("ban", KeywordAction::Meme("hohli")),
    ("banned", KeywordAction::Meme("hohli")),
    ("hohli", KeywordAction::Meme("hohli")),
    ("хохлы", KeywordAction::Meme("hohli")),
    // openvk.
    ("openvk", KeywordAction::Meme("openvk")),
    ("опенвк", KeywordAction::Meme("openvk")),
    ("вк", KeywordAction::Meme("openvk")),
    ("vk", KeywordAction::Meme("openvk")),
    // pizdec_game.
    ("пиздец", KeywordAction::Meme("pizdec_game")),
    ("pizdec", KeywordAction::Meme("pizdec_game")),
    ("game", KeywordAction::Meme("pizdec_game")),
    ("игра", KeywordAction::Meme("pizdec_game")),
    // серёжа: случайный выбор из пяти фото про одного Серёжу.
    ("серёжа", KeywordAction::RandomMeme(SERJ)),
    ("сережа", KeywordAction::RandomMeme(SERJ)),
    ("серж", KeywordAction::RandomMeme(SERJ)),
    ("serj", KeywordAction::RandomMeme(SERJ)),
    ("seryozha", KeywordAction::RandomMeme(SERJ)),
    ("sergey", KeywordAction::RandomMeme(SERJ)),
    // лейн: вечно вторая 2ava, фото одно.
    ("лейн", KeywordAction::Meme("serj_2ava")),
    ("lain", KeywordAction::Meme("serj_2ava")),
    ("lainz", KeywordAction::Meme("serj_2ava")),
    ("лейнз", KeywordAction::Meme("serj_2ava")),
    // боб.
    ("боб", KeywordAction::Meme("serj_bob")),
    ("bob", KeywordAction::Meme("serj_bob")),
    // умёр: реагируем на смерть отдельным фото, форма «ё» нормализуется.
    ("умёр", KeywordAction::Meme("serj_dead")),
    ("умер", KeywordAction::Meme("serj_dead")),
    ("уснул", KeywordAction::Meme("serj_dead")),
    ("отшёл", KeywordAction::Meme("serj_dead")),
    ("отошёл", KeywordAction::Meme("serj_dead")),
    ("dead", KeywordAction::Meme("serj_dead")),
    ("died", KeywordAction::Meme("serj_dead")),
    ("asleep", KeywordAction::Meme("serj_dead")),
    // спасибо.
    ("спасибо", KeywordAction::Meme("serj_thx")),
    ("спс", KeywordAction::Meme("serj_thx")),
    ("thanks", KeywordAction::Meme("serj_thx")),
    ("thx", KeywordAction::Meme("serj_thx")),
    ("thank", KeywordAction::Meme("serj_thx")),
    // сус: хамелеон справляется, фото одно.
    ("сус", KeywordAction::Meme("serj_dead_distr")),
    ("суся", KeywordAction::Meme("serj_dead_distr")),
    ("сюся", KeywordAction::Meme("serj_dead_distr")),
    ("opensuse", KeywordAction::Meme("serj_dead_distr")),
    ("suse", KeywordAction::Meme("serj_dead_distr")),
    ("хамелеон", KeywordAction::Meme("serj_dead_distr")),
    ("chameleon", KeywordAction::Meme("serj_dead_distr")),
    // тимка: ушёл в афк, фото одно. Фраза «отошёл» есть и у serj_dead:
    // скан таблицы решает, какое фото увидят, оба честные реакции.
    ("ушёл", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("ушел", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("отошёл", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("отошел", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("afk", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("афк", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("тимоха", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("тимка", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("timka", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    ("timoha", KeywordAction::Meme("TIMKA_DONT_EXIST")),
    // watching: куда он смотрит.
    ("смотрит", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("watching", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("where", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("neko", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("неко", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("nekomoki", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("некомоки", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("олег", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    ("oleg", KeywordAction::Meme("WHERE_HE_IS_WATHCING")),
    // хуюнай.
    ("хуюнай", KeywordAction::Meme("xyunai")),
    ("xyunai", KeywordAction::Meme("xyunai")),
    ("hyunai", KeywordAction::Meme("xyunai")),
    // задумка: тот самый дедлок.
    ("дедлок", KeywordAction::Meme("Zadumka")),
    ("deadlock", KeywordAction::Meme("Zadumka")),
    ("пейдж", KeywordAction::Meme("Zadumka")),
    ("pejdzh", KeywordAction::Meme("Zadumka")),
    ("paige", KeywordAction::Meme("Zadumka")),
    ("задумка", KeywordAction::Meme("Zadumka")),
    ("zadumka", KeywordAction::Meme("Zadumka")),
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

/// Совпадение иглы с мемом по нормализованным строкам в обе стороны:
/// точный ключ или подстрока в любом направлении. «Серёжа» таблица
/// держит одной иглой `serj`, и она обязана накрыть все его фото.
fn needle_covers_name(name_key: &str, needle: &str) -> bool {
    let needle_key = meme_key(needle);
    !(name_key.is_empty() || needle_key.is_empty())
        && (name_key.contains(needle_key.as_str()) || needle_key.contains(name_key))
}

/// Фразы таблицы, чей ключевой элемент указывает на этот мем. Meme(needle)
/// сравнивается с именем напрямую, RandomMeme — любой его иглой. Порядок
/// таблицы сохраняется, повторы отбрасываются. Это слова для caption
/// в /triggers: группам видно, какое слово вызывает какое фото.
pub fn phrases_for_meme(meme: &Meme) -> Vec<&'static str> {
    let name_key = meme_key(&meme.name);
    let mut phrases: Vec<&'static str> = Vec::new();
    for (phrase, action) in KEYWORD_REACTIONS.iter().copied() {
        let hit = match action {
            KeywordAction::Grandmobile => false,
            KeywordAction::Meme(needle) => needle_covers_name(&name_key, needle),
            KeywordAction::RandomMeme(needles) => needles
                .iter()
                .any(|needle| needle_covers_name(&name_key, needle)),
        };
        if hit && !phrases.contains(&phrase) {
            phrases.push(phrase);
        }
    }
    phrases
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

    use crate::memes::MemeKind;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-keywords-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("video")).unwrap();
        std::fs::create_dir_all(dir.join("music")).unwrap();
        std::fs::create_dir_all(dir.join("fotos")).unwrap();
        dir
    }

    #[test]
    fn shared_table_covers_every_alias() {
        // Некоторые фразы делятся двумя группами («отошёл» ведёт и к
        // serj_dead, и к тимке): скан таблицы решает, чьё фото пойдёт.
        // Нормализация съедает «ё», поэтому набор собирается по тому же
        // ключу, что видит матчер. Найденное действие обязан входить в
        // набор, объявленный этой фразой.
        let mut claimed: HashMap<String, Vec<KeywordAction>> = HashMap::new();
        for (phrase, action) in KEYWORD_REACTIONS {
            claimed
                .entry(normalized_words(phrase).concat())
                .or_default()
                .push(action);
        }
        for (phrase, _) in KEYWORD_REACTIONS {
            let found = match_keyword(phrase).unwrap_or_else(|| panic!("нет реакции на {phrase}"));
            let known = &claimed[&normalized_words(phrase).concat()];
            assert!(
                known.contains(&found.1),
                "{phrase}: матч вернул {:?}, объявлены {known:?}",
                found.1
            );
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
    fn every_action_has_cyrillic_and_ascii_phrase() {
        // Ворота из AGENTS.md: у каждого действия таблицы должна быть
        // хотя бы одна кириллическая фраза и одна ASCII (латиница, транслит
        // или чисто цифровая). Группы собираются по действиям.
        let mut groups: HashMap<KeywordAction, (bool, bool)> = HashMap::new();
        for (phrase, action) in KEYWORD_REACTIONS {
            let seen = groups.entry(action).or_insert((false, false));
            if phrase
                .chars()
                .any(|ch| ch.is_alphabetic() && !ch.is_ascii())
            {
                seen.0 = true;
            }
            if phrase.is_ascii() {
                seen.1 = true;
            }
        }
        assert!(
            groups.len() >= 20,
            "таблица подозрительно сжалась: {} групп",
            groups.len()
        );
        for (action, (cyr, ascii)) in groups {
            assert!(cyr, "{action:?}: нет кириллической фразы");
            assert!(ascii, "{action:?}: нет латиницы или транслита");
        }
    }

    #[test]
    fn short_ascii_aliases_fire_only_as_whole_words() {
        let cases: &[(&str, KeywordAction)] = &[
            ("uk", KeywordAction::Meme("arectovich_zig")),
            ("cat", KeywordAction::RandomMeme(CATS)),
            ("ban", KeywordAction::Meme("hohli")),
            ("bob", KeywordAction::Meme("serj_bob")),
            ("dead", KeywordAction::Meme("serj_dead")),
            ("base", KeywordAction::Meme("baza")),
            ("vk", KeywordAction::Meme("openvk")),
        ];
        for (word, expected) in cases {
            assert_eq!(
                match_keyword(word).map(|(_, action)| action),
                Some(*expected),
                "{word}"
            );
        }

        // Внутри длинных слов короткие алиасы не срабатывают: матчер
        // обязан держать границу слова и для новых коротких фраз.
        for text in [
            "scatter",
            "banana",
            "базовый",
            "украшение",
            "вконтакте",
            "videogames",
            "serjacked",
        ] {
            assert_eq!(match_keyword(text), None, "{text}");
        }
    }

    #[test]
    fn photo_aliases_resolve_to_image_files() {
        let files = [
            "arectovich_zig.png",
            "barani_call.png",
            "barani_perdj.png",
            "batek_bot_issue.png",
            "baza.png",
            "cat_kzn.png",
            "hohli.png",
            "kzn_picture.png",
            "openvk.jpeg",
            "pizdec_game.png",
            "serj_2ava.png",
            "serj_bob.png",
            "serj_dead_distr.png",
            "serj_dead.png",
            "serj_thx.png",
            "TIMKA_DONT_EXIST.png",
            "WHERE_HE_IS_WATHCING.png",
            "xyunai.png",
            "Zadumka.png",
        ];
        let root = temp_root("photo-aliases");
        for file in files {
            std::fs::write(root.join("fotos").join(file), b"p").unwrap();
        }
        let memes = Memes::scan(&root);

        // Каждый стем из таблицы обязан найтись и быть фото из fotos/.
        let photo_stems = [
            "arectovich_zig",
            "barani_call",
            "barani_perdj",
            "batek_bot_issue",
            "baza",
            "cat_kzn",
            "hohli",
            "kzn_picture",
            "openvk",
            "pizdec_game",
            "serj_2ava",
            "serj_bob",
            "serj_dead",
            "serj_dead_distr",
            "serj_thx",
            "TIMKA_DONT_EXIST",
            "WHERE_HE_IS_WATHCING",
            "xyunai",
            "Zadumka",
        ];
        assert_eq!(
            photo_stems.len(),
            files.len(),
            "стемы и файлы держатся вместе"
        );
        for stem in photo_stems {
            let meme = memes
                .find(stem)
                .unwrap_or_else(|| panic!("{stem} не найден"));
            assert_eq!(meme.kind, MemeKind::Image, "{stem}");
            assert!(
                meme.path.starts_with(root.join("fotos")),
                "{}: чужой путь {:?}",
                stem,
                meme.path
            );
        }

        // Случайные группы держат свой размер: выбор честный.
        assert_eq!(random_meme_candidates(&memes, BARANI).len(), 2);
        assert_eq!(random_meme_candidates(&memes, CATS).len(), 2);
        assert_eq!(random_meme_candidates(&memes, SERJ).len(), 5);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn photo_aliases_pass_through_inline_prefix_matcher() {
        // Для целых слов inline и chat обязаны отдать одну и ту же фразу
        // и действие: таблица одна, матчеры расходятся только на префиксах.
        for query in ["зига", "укр", "база", "кот", "казан", "vpn", "дедлок"]
        {
            assert_eq!(match_keyword_prefix(query), match_keyword(query), "{query}");
        }

        // Префиксы Finds как префиксы: "каз" уже ведет к группе казана.
        assert_eq!(
            match_keyword_prefix("каз").map(|(_, action)| action),
            Some(KeywordAction::RandomMeme(CATS)),
            "каз"
        );
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
        for (phrase, _) in KEYWORD_REACTIONS {
            let by_chat = match_keyword(phrase).map(|(_, action)| action);
            assert!(
                by_chat.is_some(),
                "chat не видит собственную фразу {phrase}"
            );
            assert_eq!(
                match_keyword_prefix(phrase).map(|(_, action)| action),
                by_chat,
                "inline и chat расходятся на {phrase}"
            );
        }
    }

    fn plain_meme(name: &str) -> Meme {
        Meme {
            name: name.to_string(),
            path: PathBuf::from(format!("video/{name}.mp4")),
            kind: MemeKind::Video,
        }
    }

    #[test]
    fn phrases_for_meme_lists_table_hits() {
        let bomjara1 = phrases_for_meme(&plain_meme("bomjara1"));
        assert!(
            bomjara1.contains(&"бомжара"),
            "случайная группа обязана попасть в caption: {bomjara1:?}"
        );
        assert!(
            bomjara1.contains(&"манжара"),
            "алиасы группы тоже слова этого мема: {bomjara1:?}"
        );
        assert!(
            !bomjara1.contains(&"дедлок"),
            "чужие группы не приписываются: {bomjara1:?}"
        );

        let makuos = phrases_for_meme(&plain_meme("makuos"));
        assert!(
            makuos.contains(&"марк адерли"),
            "чистое Meme-действие с иглой makuos: {makuos:?}"
        );
        assert!(makuos.contains(&"makuos"), "{makuos:?}");
        assert!(
            !makuos.contains(&"база"),
            "нейтральные фразы чужие: {makuos:?}"
        );

        let zadumka = phrases_for_meme(&plain_meme("Zadumka"));
        assert!(zadumka.contains(&"дедлок"), "{zadumka:?}");
        assert!(zadumka.contains(&"задумка"), "{zadumka:?}");

        let loner = phrases_for_meme(&plain_meme("Григорий Лепс - Натали"));
        assert!(loner.is_empty(), "нет алиасов, значит и пусто: {loner:?}");
    }
}

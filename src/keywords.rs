//! Один матчер на все реакции бота, весь процесс живёт в Telegram.
//!
//! Встроенная таблица `KEYWORD_REACTIONS` остаётся источником правды, но
//! теперь над ней строится `KeywordTable`: встроенные фразы минус
//! выключенные плюс добавленные из бота. Таблица собирается при старте и
//! после каждой правки из панели; на сообщение она не пересобирается.
//!
//! Лимиты живут в `KeywordLimiter`: ключ это идентификатор чата, а карта
//! создана вместе с ботом и не переживает перезапуск.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::warn;

use crate::memes::{meme_key, normalized_words, Meme, Memes};
use crate::store;

/// Не чаще одного ответа на фразу в одном чате.
pub const KEYWORD_RATE_LIMIT: Duration = Duration::from_secs(20);

/// Записи старше этого срока удаляются при следующей проверке: карта не
/// растёт на долгоживущих каналах и группах.
pub const KEYWORD_PRUNE_AFTER: Duration = Duration::from_secs(10 * 60);

/// Предел длины добавляемой фразы. Метка запроса в /meme ограничена теми же
/// 64 символами, а фраза короче строки чата и хорошо читается в карточке.
pub const PHRASE_MAX: usize = 64;

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
    ("белые розы", meme("Белые Розы")),
    ("belye rozy", meme("Белые Розы")),
    ("belye-rozy", meme("Белые Розы")),
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
    ("макуос", meme("makuos")),
    ("makuos", meme("makuos")),
    ("марк адерли", meme("makuos")),
    ("марк", meme("makuos")),
    ("mark aderli", meme("makuos")),
    ("mark", meme("makuos")),
    // Фамилия без имени: целым словом, а не подстрокой, поэтому
    // перечислена явно.
    ("аддерли", meme("makuos")),
    ("adderli", meme("makuos")),
    // shefos.mp4: "320" это отдельное слово, а не подстрока; границу слова
    // и лимит в 20 секунд держит общий матчер.
    ("шефос", meme("shefos")),
    ("shefos", meme("shefos")),
    ("шеф", meme("shefos")),
    ("пингвин320кг", meme("shefos")),
    ("пингвин320", meme("shefos")),
    ("pingvin320kg", meme("shefos")),
    ("pingvin320", meme("shefos")),
    ("320", meme("shefos")),
    // arectovich_zig: одиночное фото, зига это то самое.
    ("зига", meme("arectovich_zig")),
    ("зиг", meme("arectovich_zig")),
    ("гитлер", meme("arectovich_zig")),
    ("hitler", meme("arectovich_zig")),
    ("zig", meme("arectovich_zig")),
    ("арестович", meme("arectovich_zig")),
    ("arestovich", meme("arectovich_zig")),
    ("arectovich", meme("arectovich_zig")),
    ("украина", meme("arectovich_zig")),
    ("укр", meme("arectovich_zig")),
    ("ukr", meme("arectovich_zig")),
    ("uk", meme("arectovich_zig")),
    ("ukraine", meme("arectovich_zig")),
    // barani: случайный выбор между двумя баранами.
    ("бараны", KeywordAction::RandomMeme(BARANI)),
    ("баран", KeywordAction::RandomMeme(BARANI)),
    ("barani", KeywordAction::RandomMeme(BARANI)),
    ("vpn", KeywordAction::RandomMeme(BARANI)),
    ("впн", KeywordAction::RandomMeme(BARANI)),
    // batek_bot_issue: боль того самого Андрея, фото одно.
    ("бабушка андрея", meme("batek_bot_issue")),
    ("бабушка", meme("batek_bot_issue")),
    ("grandma", meme("batek_bot_issue")),
    ("babushka", meme("batek_bot_issue")),
    ("андрей", meme("batek_bot_issue")),
    ("andrey", meme("batek_bot_issue")),
    ("andrei", meme("batek_bot_issue")),
    // baza.
    ("база", meme("baza")),
    ("baza", meme("baza")),
    ("base", meme("baza")),
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
    ("забанили", meme("hohli")),
    ("бан", meme("hohli")),
    ("ban", meme("hohli")),
    ("banned", meme("hohli")),
    ("hohli", meme("hohli")),
    ("хохлы", meme("hohli")),
    // openvk.
    ("openvk", meme("openvk")),
    ("опенвк", meme("openvk")),
    ("вк", meme("openvk")),
    ("vk", meme("openvk")),
    // pizdec_game.
    ("пиздец", meme("pizdec_game")),
    ("pizdec", meme("pizdec_game")),
    ("game", meme("pizdec_game")),
    ("игра", meme("pizdec_game")),
    // серёжа: случайный выбор из пяти фото про одного Серёжу.
    ("серёжа", KeywordAction::RandomMeme(SERJ)),
    ("сережа", KeywordAction::RandomMeme(SERJ)),
    ("серж", KeywordAction::RandomMeme(SERJ)),
    ("serj", KeywordAction::RandomMeme(SERJ)),
    ("seryozha", KeywordAction::RandomMeme(SERJ)),
    ("sergey", KeywordAction::RandomMeme(SERJ)),
    // лейн: вечно вторая 2ava, фото одно.
    ("лейн", meme("serj_2ava")),
    ("lain", meme("serj_2ava")),
    ("lainz", meme("serj_2ava")),
    ("лейнз", meme("serj_2ava")),
    // боб.
    ("боб", meme("serj_bob")),
    ("bob", meme("serj_bob")),
    // умёр: реагируем на смерть отдельным фото, форма «ё» нормализуется.
    ("умёр", meme("serj_dead")),
    ("умер", meme("serj_dead")),
    ("уснул", meme("serj_dead")),
    ("отшёл", meme("serj_dead")),
    ("отошёл", meme("serj_dead")),
    ("dead", meme("serj_dead")),
    ("died", meme("serj_dead")),
    ("asleep", meme("serj_dead")),
    // спасибо.
    ("спасибо", meme("serj_thx")),
    ("спс", meme("serj_thx")),
    ("thanks", meme("serj_thx")),
    ("thx", meme("serj_thx")),
    ("thank", meme("serj_thx")),
    // сус: хамелеон справляется, фото одно.
    ("сус", meme("serj_dead_distr")),
    ("суся", meme("serj_dead_distr")),
    ("сюся", meme("serj_dead_distr")),
    ("opensuse", meme("serj_dead_distr")),
    ("suse", meme("serj_dead_distr")),
    ("хамелеон", meme("serj_dead_distr")),
    ("chameleon", meme("serj_dead_distr")),
    // тимка: ушёл в афк, фото одно. Фраза «отошёл» есть и у serj_dead:
    // скан таблицы решает, какое фото увидят, оба честные реакции.
    ("ушёл", meme("TIMKA_DONT_EXIST")),
    ("ушел", meme("TIMKA_DONT_EXIST")),
    ("отошёл", meme("TIMKA_DONT_EXIST")),
    ("отошел", meme("TIMKA_DONT_EXIST")),
    ("afk", meme("TIMKA_DONT_EXIST")),
    ("афк", meme("TIMKA_DONT_EXIST")),
    ("тимоха", meme("TIMKA_DONT_EXIST")),
    ("тимка", meme("TIMKA_DONT_EXIST")),
    ("timka", meme("TIMKA_DONT_EXIST")),
    ("timoha", meme("TIMKA_DONT_EXIST")),
    // watching: куда он смотрит.
    ("смотрит", meme("WHERE_HE_IS_WATHCING")),
    ("watching", meme("WHERE_HE_IS_WATHCING")),
    ("where", meme("WHERE_HE_IS_WATHCING")),
    ("neko", meme("WHERE_HE_IS_WATHCING")),
    ("неко", meme("WHERE_HE_IS_WATHCING")),
    ("nekomoki", meme("WHERE_HE_IS_WATHCING")),
    ("некомоки", meme("WHERE_HE_IS_WATHCING")),
    ("олег", meme("WHERE_HE_IS_WATHCING")),
    ("oleg", meme("WHERE_HE_IS_WATHCING")),
    // хуюнай.
    ("хуюнай", meme("xyunai")),
    ("xyunai", meme("xyunai")),
    ("hyunai", meme("xyunai")),
    // задумка: тот самый дедлок.
    ("дедлок", meme("Zadumka")),
    ("deadlock", meme("Zadumka")),
    ("пейдж", meme("Zadumka")),
    ("pejdzh", meme("Zadumka")),
    ("paige", meme("Zadumka")),
    ("задумка", meme("Zadumka")),
    ("zadumka", meme("Zadumka")),
];

/// Цель реакции. `Meme` держит `Cow`, потому что фраза, добавленная из бота,
/// приходит во время работы: встроенные записи остаются заимствованными
/// строками без аллокации, добавленные владеют своей.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum KeywordAction {
    Grandmobile,
    /// Мем, имя которого совпадает с этой иглой.
    Meme(Cow<'static, str>),
    /// Случайный мем из всех, чьё имя содержит любую из подстрок.
    RandomMeme(&'static [&'static str]),
}

/// Короткая запись встроенного действия: таблица остаётся `const`, хотя
/// вариант `Meme` теперь хранит `Cow`.
pub const fn meme(name: &'static str) -> KeywordAction {
    KeywordAction::Meme(Cow::Borrowed(name))
}

/// Одна строка таблицы, разобранная один раз при сборке.
#[derive(Debug)]
struct Entry {
    /// Фраза как её видит человек и как её показывает панель.
    phrase: String,
    /// Нормализованные слова фразы: по ним chat-матчер держит границы слов.
    words: Vec<String>,
    /// Склеенная нормализованная фраза: по ней inline-матчер ищет префикс.
    key: String,
    /// Нормализованные цели действия. Пусто у `Grandmobile`: файла нет.
    needles: Vec<String>,
    action: KeywordAction,
    /// Встроенная фраза или добавленная из бота: от этого зависит, как её
    /// убирать, запретом в файле правок или удалением записи.
    builtin: bool,
    /// У действия нет фразы на одном из алфавитов. Такая запись видна в
    /// панели, но не срабатывает: контракт «оба алфавита» держится на живой
    /// таблице, а не на обещании. Первая фраза нового мема попадает сюда же,
    /// пока не добавлена вторая, и включается сама.
    pending: bool,
}

/// Готовая таблица. Собирается функцией `build` и живёт под `RwLock` в
/// `TgData`; матчеры только читают.
#[derive(Debug)]
pub struct KeywordTable {
    entries: Vec<Entry>,
}

impl KeywordTable {
    /// Встроенная таблица минус выключенные плюс добавленные. Порядок:
    /// сначала встроенные в порядке объявления, затем добавленные в порядке
    /// добавления. Порядок важен для стабильности inline-выдачи.
    pub fn build(edits: &KeywordEdits) -> Self {
        let mut entries =
            Vec::with_capacity(KEYWORD_REACTIONS.len().saturating_add(edits.added.len()));
        for (phrase, action) in KEYWORD_REACTIONS.iter() {
            if edits.disabled.iter().any(|disabled| disabled == phrase) {
                continue;
            }
            entries.push(entry(phrase.to_string(), action.clone(), true));
        }
        for added in &edits.added {
            let action = KeywordAction::Meme(Cow::Owned(added.meme.clone()));
            entries.push(entry(added.phrase.clone(), action, false));
        }

        // Готовность действия считается по всей таблице: одна добавленная
        // кириллическая фраза без ASCII не должна срабатывать.
        let ready: Vec<bool> = entries
            .iter()
            .map(|entry| {
                alphabet_pair_ok(
                    entries
                        .iter()
                        .filter(|other| other.action == entry.action)
                        .map(|other| other.phrase.as_str()),
                )
            })
            .collect();
        for (entry, ready) in entries.iter_mut().zip(ready) {
            entry.pending = !ready;
        }

        Self { entries }
    }

    /// Ищет ключевую фразу по нормализованным словам: регистр, алфавит и
    /// дефисы не важны, но фраза должна занимать целые слова. Поэтому
    /// `небомжара` не совпадает, а `bomzhara` совпадает с `бомжара`.
    pub fn match_keyword(&self, text: &str) -> Option<(&str, KeywordAction)> {
        let words = normalized_words(text);
        if words.is_empty() {
            return None;
        }

        self.entries.iter().find_map(|entry| {
            if entry.pending || entry.words.is_empty() || entry.words.len() > words.len() {
                return None;
            }
            let hit = words
                .windows(entry.words.len())
                .any(|window| window == entry.words.as_slice());
            hit.then(|| (entry.phrase.as_str(), entry.action.clone()))
        })
    }

    /// Свободный запрос inline к той же таблице, что и chat-реакции. Точный
    /// нормализованный матч сильнее префикса, поэтому совпадение по таблице
    /// исключает поиск по именам мемов. Префикс работает от двух символов:
    /// "ман" уже приводит к bomjara, пока пользователь допечатывает. Из равных
    /// длин побеждает первая запись таблицы, порядок стабилен.
    pub fn match_keyword_prefix(&self, query: &str) -> Option<(&str, KeywordAction)> {
        let needle = normalized_query(query);
        if needle.is_empty() {
            return None;
        }

        // Точный нормализованный матч важнее префикса: "грандмобайл" обязан
        // прийти к одноимённой записи, а не через префикс.
        if let Some(exact) = self
            .entries
            .iter()
            .find(|entry| !entry.pending && entry.key == needle)
        {
            return Some((exact.phrase.as_str(), exact.action.clone()));
        }
        if needle.chars().count() < 2 {
            return None;
        }

        let mut best: Option<&Entry> = None;
        for entry in &self.entries {
            if !entry.pending
                && entry.key.starts_with(&needle)
                && best.is_none_or(|best| entry.key.len() > best.key.len())
            {
                best = Some(entry);
            }
        }
        best.map(|entry| (entry.phrase.as_str(), entry.action.clone()))
    }

    /// Фразы, чья игла указывает на этот мем. Meme(needle) сравнивается с
    /// именем напрямую, RandomMeme — любой его иглой. Порядок таблицы
    /// сохраняется, повторы отбрасываются. Это слова для caption в
    /// /triggers: группам видно, какое слово вызывает какое фото.
    pub fn phrases_for_meme(&self, meme: &Meme) -> Vec<&str> {
        self.phrases_covering(|name_key| keys_cover(name_key, &meme_key(&meme.name)))
    }

    /// То же по имени мема без объекта `Meme`: панель показывает карточку и
    /// для файла, который только что загружен.
    pub fn phrases_for_name(&self, name: &str) -> Vec<&str> {
        self.phrases_covering(|name_key| keys_cover(name_key, &meme_key(name)))
    }

    /// Тройки «фраза, встроенная ли, ждёт ли пары» для карточки панели: по
    /// ним видно, что можно удалить совсем, что только выключить и что пока
    /// не сработает.
    pub fn phrase_sources_for_name(&self, name: &str) -> Vec<(&str, bool, bool)> {
        let name_key = meme_key(name);
        self.entries
            .iter()
            .filter(|entry| {
                entry
                    .needles
                    .iter()
                    .any(|needle| keys_cover(&name_key, needle))
            })
            .map(|entry| (entry.phrase.as_str(), entry.builtin, entry.pending))
            .collect()
    }

    fn phrases_covering<F>(&self, covers: F) -> Vec<&str>
    where
        F: Fn(&str) -> bool,
    {
        let mut phrases: Vec<&str> = Vec::new();
        for entry in &self.entries {
            let hit = entry.needles.iter().any(|needle| covers(needle));
            if hit && !phrases.contains(&entry.phrase.as_str()) {
                phrases.push(entry.phrase.as_str());
            }
        }
        phrases
    }

    /// Есть ли у действия фразы обоих алфавитов. Встроенная таблица держит
    /// это правило тестом, а правки из бота проверяются здесь: иначе бот
    /// принял бы фразу, которая нарушает контракт.
    pub fn action_has_both_alphabets(&self, action: &KeywordAction) -> bool {
        alphabet_pair_ok(
            self.entries
                .iter()
                .filter(|entry| &entry.action == action)
                .map(|entry| entry.phrase.as_str()),
        )
    }

    /// Фраза с таким ключом, ведущая к другому действию. Одинаковый ключ у
    /// разных написаний одного действия это норма (`белые розы` и
    /// `belye rozy`), а вот ключ, уже занятый другим мемом, добавить нельзя:
    /// матчер вернул бы чужое действие.
    fn conflicting_phrase(&self, key: &str, action: &KeywordAction) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| entry.key == key && &entry.action != action)
            .map(|entry| entry.phrase.as_str())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

fn entry(phrase: String, action: KeywordAction, builtin: bool) -> Entry {
    let words = normalized_words(&phrase);
    let key = words.concat();
    let needles = match &action {
        KeywordAction::Grandmobile => Vec::new(),
        KeywordAction::Meme(needle) => vec![meme_key(needle)],
        KeywordAction::RandomMeme(needles) => {
            needles.iter().map(|needle| meme_key(needle)).collect()
        }
    };
    Entry {
        phrase,
        words,
        key,
        needles,
        action,
        builtin,
        // Заполняется в `build` по всей таблице: цена одной фразы ничего не
        // говорит о паре алфавитов у действия.
        pending: false,
    }
}

/// Запрос к той же таблице одной строкой: слова склеиваются, границы
/// сознательно теряются, потому что inline-запрос приходит по мере набора.
fn normalized_query(text: &str) -> String {
    normalized_words(text).concat()
}

/// Совпадение иглы с мемом по нормализованным строкам в обе стороны:
/// точный ключ или подстрока в любом направлении. «Серёжа» таблица
/// держит одной иглой `serj`, и она обязана накрыть все его фото.
fn keys_cover(name_key: &str, needle_key: &str) -> bool {
    !(name_key.is_empty() || needle_key.is_empty())
        && (name_key.contains(needle_key) || needle_key.contains(name_key))
}

/// Правки, которые переживают перезапуск. Файл хранит только отличия от
/// встроенной таблицы: добавленные пары и выключенные встроенные фразы.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct KeywordEdits {
    #[serde(default)]
    pub added: Vec<AddedPhrase>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddedPhrase {
    pub phrase: String,
    pub meme: String,
}

/// Проверка фразы до вставки. Возвращает её в обрезанном виде: пробелы по
/// краям в таблице не нужны, а внутри фразы остаются значимыми.
pub fn validate_phrase(phrase: &str) -> Result<String, String> {
    let trimmed = phrase.trim();
    if trimmed.is_empty() {
        return Err("Фраза пустая.".to_string());
    }
    if trimmed.chars().count() > PHRASE_MAX {
        return Err(format!("Фраза длиннее {PHRASE_MAX} символов."));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Фраза содержит управляющие символы.".to_string());
    }
    if normalized_words(trimmed).is_empty() {
        return Err("Во фразе нет ни букв, ни цифр.".to_string());
    }
    Ok(trimmed.to_string())
}

/// Правило контракта: у фраз есть и кириллица, и ASCII. Кириллицей считается
/// буква вне ASCII, ASCII это любая фраза целиком из ASCII, включая цифры.
pub fn alphabet_pair_ok<'a>(phrases: impl Iterator<Item = &'a str>) -> bool {
    let mut cyrillic = false;
    let mut ascii = false;
    for phrase in phrases {
        if phrase
            .chars()
            .any(|ch| ch.is_alphabetic() && !ch.is_ascii())
        {
            cyrillic = true;
        }
        if phrase.is_ascii() {
            ascii = true;
        }
        if cyrillic && ascii {
            return true;
        }
    }
    false
}

/// Хранилище правок: файл плюс текущее содержимое. Мутации сначала пишут
/// файл, потом меняют память: если диск недоступен, панель честно скажет об
/// ошибке, а таблица останется прежней.
pub struct KeywordStore {
    path: PathBuf,
    edits: KeywordEdits,
}

impl KeywordStore {
    pub fn load(path: PathBuf) -> Self {
        let edits = store::read_json::<KeywordEdits>(&path).unwrap_or_default();
        Self { path, edits }
    }

    #[cfg(test)]
    pub fn edits(&self) -> &KeywordEdits {
        &self.edits
    }

    pub fn table(&self) -> KeywordTable {
        KeywordTable::build(&self.edits)
    }

    /// Добавляет фразу к мему. Проверки: фраза валидна, дубля по
    /// нормализованному ключу нет, у действия остаются оба алфавита.
    pub fn add(&mut self, phrase: &str, meme_name: &str) -> Result<KeywordTable, String> {
        let phrase = validate_phrase(phrase)?;
        if meme_name.trim().is_empty() {
            return Err("Не указан мем.".to_string());
        }

        let mut next = self.edits.clone();
        next.added.push(AddedPhrase {
            phrase: phrase.clone(),
            meme: meme_name.to_string(),
        });
        let table = KeywordTable::build(&next);

        let key = normalized_words(&phrase).concat();
        let action = KeywordAction::Meme(Cow::Owned(meme_name.to_string()));
        if let Some(occupied) = table.conflicting_phrase(&key, &action) {
            return Err(format!(
                "Фраза '{phrase}' уже занята другой реакцией: '{occupied}'."
            ));
        }

        self.commit(next)?;
        Ok(table)
    }

    /// Убирает фразу. Добавленная удаляется совсем, встроенная попадает в
    /// список выключенных: исходники бот не переписывает. Перед записью
    /// проверяется, что у действия остались оба алфавита.
    pub fn remove(&mut self, phrase: &str) -> Result<KeywordTable, String> {
        let mut next = self.edits.clone();
        let added_index = next.added.iter().position(|added| added.phrase == phrase);

        if let Some(index) = added_index {
            next.added.remove(index);
        } else if KEYWORD_REACTIONS
            .iter()
            .any(|(builtin, _)| *builtin == phrase)
        {
            if !next.disabled.iter().any(|disabled| disabled == phrase) {
                next.disabled.push(phrase.to_string());
            }
        } else {
            return Err(format!("Фразы '{phrase}' нет в таблице."));
        }

        let table = KeywordTable::build(&next);
        let action = self
            .table()
            .entries
            .iter()
            .find(|entry| entry.phrase == phrase)
            .map(|entry| entry.action.clone());
        if let Some(action) = action {
            if !table.action_has_both_alphabets(&action) {
                return Err(format!(
                    "После удаления '{phrase}' у действия не останется фразы \
                     на втором алфавите."
                ));
            }
        }

        self.commit(next)?;
        Ok(table)
    }

    fn commit(&mut self, next: KeywordEdits) -> Result<(), String> {
        if let Err(e) = store::write_json_atomic(&self.path, &next) {
            warn!("keywords: {} не записан: {e}", self.path.display());
            return Err("Не удалось сохранить правку.".to_string());
        }
        self.edits = next;
        Ok(())
    }
}

/// Все мемы, чьё имя содержит любую из подстрок. Кандидаты нужны, чтобы
/// `бомжара` выбирала между bomjara1 и bomjaro2, как `/bomjara`.
pub fn random_meme_candidates<'a>(memes: &'a Memes, needles: &[&str]) -> Vec<&'a Meme> {
    let needles: Vec<String> = needles.iter().map(|needle| meme_key(needle)).collect();
    let mut candidates: Vec<&Meme> = Vec::new();
    for (meme, key) in memes.items().iter().zip(memes.keys()) {
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

/// Лимит ответов на ключевые слова. Ключ это только id чата: одно окно на
/// чат, а не на фразу, поэтому разные слова в одной группе не обходят
/// ограничение. Карта живёт вместе с процессом бота.
#[derive(Default)]
pub struct KeywordLimiter {
    last: RwLock<HashMap<i64, Instant>>,
}

/// Потолок карты: после чистки старых записей она не должна расти дальше.
const KEYWORD_MAP_LIMIT: usize = 10_000;

impl KeywordLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Проверяет и обновляет лимит: не чаще одного ответа на любое ключевое
    /// слово в чат за `KEYWORD_RATE_LIMIT`. Попутно чистит старые записи;
    /// если и после чистки записей слишком много, карта очищается целиком.
    pub async fn allow(&self, chat: i64) -> bool {
        let mut last = self.last.write().await;
        last.retain(|_, at| at.elapsed() < KEYWORD_PRUNE_AFTER);
        if last.len() > KEYWORD_MAP_LIMIT {
            last.clear();
        }

        if let Some(at) = last.get(&chat) {
            if at.elapsed() < KEYWORD_RATE_LIMIT {
                return false;
            }
        }
        last.insert(chat, Instant::now());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::memes::MemeKind;

    /// Встроенная таблица без правок: эталон для тестов матчеров.
    fn table() -> KeywordTable {
        KeywordTable::build(&KeywordEdits::default())
    }

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
        let table = table();
        let mut claimed: HashMap<String, Vec<KeywordAction>> = HashMap::new();
        for (phrase, action) in KEYWORD_REACTIONS {
            claimed
                .entry(normalized_words(phrase).concat())
                .or_default()
                .push(action);
        }
        for (phrase, _) in KEYWORD_REACTIONS {
            let found = table
                .match_keyword(phrase)
                .unwrap_or_else(|| panic!("нет реакции на {phrase}"));
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
                table().match_keyword(text).map(|(_, action)| action),
                Some(bomjara.clone()),
                "{text}"
            );
        }

        for text in ["белые розы", "belye rozy", "Belye-Rozy"] {
            assert_eq!(
                table().match_keyword(text).map(|(_, action)| action),
                Some(meme("Белые Розы")),
                "{text}"
            );
        }

        for text in ["гранд мобайл", "грандмобайл", "grandmobile", "grand mobile"]
        {
            assert_eq!(
                table().match_keyword(text).map(|(_, action)| action),
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
            assert_eq!(table().match_keyword(text), None, "{text}");
        }
    }

    #[test]
    fn bare_320_matches_only_as_a_word() {
        assert_eq!(
            table().match_keyword("320").map(|(_, action)| action),
            Some(meme("shefos"))
        );
        assert_eq!(
            table().match_keyword("скинь 320").map(|(_, action)| action),
            Some(meme("shefos"))
        );
        assert_eq!(table().match_keyword("1320"), None);
        assert_eq!(table().match_keyword("320кг"), None);
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
            ("uk", meme("arectovich_zig")),
            ("cat", KeywordAction::RandomMeme(CATS)),
            ("ban", meme("hohli")),
            ("bob", meme("serj_bob")),
            ("dead", meme("serj_dead")),
            ("base", meme("baza")),
            ("vk", meme("openvk")),
        ];
        for (word, expected) in cases {
            assert_eq!(
                table().match_keyword(word).map(|(_, action)| action),
                Some(expected.clone()),
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
            assert_eq!(table().match_keyword(text), None, "{text}");
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
            assert_eq!(
                table().match_keyword_prefix(query),
                table().match_keyword(query),
                "{query}"
            );
        }

        // Префиксы Finds как префиксы: "каз" уже ведет к группе казана.
        assert_eq!(
            table()
                .match_keyword_prefix("каз")
                .map(|(_, action)| action),
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
                let (_, action) = table()
                    .match_keyword(text)
                    .unwrap_or_else(|| panic!("нет реакции на {text}"));
                let needle = match action {
                    KeywordAction::Meme(needle) => needle,
                    other => panic!("{text}: не мем, а {other:?}"),
                };
                let found = memes
                    .find(needle.as_ref())
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

        let action = table()
            .match_keyword("бомжара")
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
        // Никаких подмен на стороне обработчиков: reactions зовут тот же
        // метод таблицы, что и тесты.
        assert_eq!(
            table().match_keyword("бомжара").map(|(_, action)| action),
            Some(KeywordAction::RandomMeme(&["bomjara", "bomjaro"]))
        );
        assert_eq!(
            table()
                .match_keyword("марк адерли")
                .map(|(_, action)| action),
            Some(meme("makuos"))
        );
    }

    #[test]
    fn inline_prefix_tables_cover_every_alias() {
        let bomjara = KeywordAction::RandomMeme(&["bomjara", "bomjaro"]);
        for (query, expected) in [
            ("ман", bomjara.clone()),
            ("манжара", bomjara.clone()),
            ("manjaro", bomjara.clone()),
            // Имя мема совпадает с алиасом: таблица одна на чат и inline.
            ("bomjara", bomjara.clone()),
            ("аддерли", meme("makuos")),
            ("марк", meme("makuos")),
            ("makuos", meme("makuos")),
            ("пингвин320", meme("shefos")),
            ("шефос", meme("shefos")),
            ("белые", meme("Белые Розы")),
            ("гранд", KeywordAction::Grandmobile),
        ] {
            assert_eq!(
                table()
                    .match_keyword_prefix(query)
                    .map(|(_, action)| action),
                Some(expected),
                "{query}"
            );
        }

        // Постороннее слово и слишком короткий ввод не совпадают.
        assert_eq!(
            table().match_keyword_prefix("вполне посторонний вопрос"),
            None
        );
        assert_eq!(table().match_keyword_prefix("в"), None);
        assert_eq!(table().match_keyword_prefix(""), None);
    }

    #[test]
    fn inline_prefix_agrees_with_chat_matcher_on_every_phrase() {
        for (phrase, _) in KEYWORD_REACTIONS {
            let by_chat = table().match_keyword(phrase).map(|(_, action)| action);
            assert!(
                by_chat.is_some(),
                "chat не видит собственную фразу {phrase}"
            );
            assert_eq!(
                table()
                    .match_keyword_prefix(phrase)
                    .map(|(_, action)| action),
                by_chat,
                "inline и chat расходятся на {phrase}"
            );
        }
    }

    /// Старая построчная логика без OnceLock: эталон для проверки индекса.
    fn naive_match_keyword(text: &str) -> Option<(&'static str, KeywordAction)> {
        let words = normalized_words(text);
        if words.is_empty() {
            return None;
        }
        KEYWORD_REACTIONS
            .iter()
            .find(|(phrase, _)| {
                let needle = normalized_words(phrase);
                if needle.is_empty() || needle.len() > words.len() {
                    return false;
                }
                let whole = needle.concat();
                words
                    .windows(needle.len())
                    .any(|window| window.concat() == whole)
            })
            .map(|(phrase, action)| (*phrase, action.clone()))
    }

    #[test]
    fn index_matches_naive_matcher() {
        assert_eq!(table().len(), KEYWORD_REACTIONS.len());

        let mut samples: Vec<String> = KEYWORD_REACTIONS
            .iter()
            .map(|(phrase, _)| (*phrase).to_string())
            .collect();
        samples.extend(
            [
                "",
                "   ",
                "небомжара",
                "1320",
                "скинь 320",
                "просто текст",
                "GRAND MOBILE",
                "bomzhara!",
                "отошёл",
            ]
            .iter()
            .map(|text| (*text).to_string()),
        );

        for text in samples {
            assert_eq!(
                table().match_keyword(&text),
                naive_match_keyword(&text),
                "индекс разошёлся с эталоном на {text:?}"
            );
            assert_eq!(
                table().match_keyword_prefix(&text),
                naive_prefix_match(&text),
                "inline-индекс разошёлся с эталоном на {text:?}"
            );
        }
    }

    /// Старый prefix-матчер для сравнения: точный ключ, затем самый длинный
    /// префикс, при равенстве побеждает первая строка таблицы.
    fn naive_prefix_match(query: &str) -> Option<(&'static str, KeywordAction)> {
        let needle = normalized_query(query);
        if needle.is_empty() {
            return None;
        }
        if let Some((phrase, action)) = KEYWORD_REACTIONS
            .iter()
            .find(|(phrase, _)| normalized_query(phrase) == needle)
        {
            return Some((*phrase, action.clone()));
        }
        if needle.chars().count() < 2 {
            return None;
        }
        let mut best: Option<(usize, (&'static str, KeywordAction))> = None;
        for (phrase, action) in KEYWORD_REACTIONS.iter() {
            let normalized = normalized_query(phrase);
            if normalized.starts_with(&needle) {
                let len = normalized.len();
                if best.as_ref().is_none_or(|(best_len, _)| len > *best_len) {
                    best = Some((len, (*phrase, action.clone())));
                }
            }
        }
        best.map(|(_, entry)| entry)
    }

    #[tokio::test]
    async fn limiter_is_one_window_per_chat_not_per_phrase() {
        let limiter = KeywordLimiter::new();
        assert!(limiter.allow(1).await);
        // Другая фраза в том же чате не обходит окно: ключ теперь только чат.
        assert!(!limiter.allow(1).await);
        // Другой чат живёт независимо.
        assert!(limiter.allow(2).await);
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
        let table = table();
        let bomjara1 = table.phrases_for_meme(&plain_meme("bomjara1"));
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

        let makuos = table.phrases_for_meme(&plain_meme("makuos"));
        assert!(
            makuos.contains(&"марк адерли"),
            "чистое Meme-действие с иглой makuos: {makuos:?}"
        );
        assert!(makuos.contains(&"makuos"), "{makuos:?}");
        assert!(
            !makuos.contains(&"база"),
            "нейтральные фразы чужие: {makuos:?}"
        );

        let zadumka = table.phrases_for_meme(&plain_meme("Zadumka"));
        assert!(zadumka.contains(&"дедлок"), "{zadumka:?}");
        assert!(zadumka.contains(&"задумка"), "{zadumka:?}");

        let loner = table.phrases_for_meme(&plain_meme("Григорий Лепс - Натали"));
        assert!(loner.is_empty(), "нет алиасов, значит и пусто: {loner:?}");
    }
}

#[cfg(test)]
mod edit_tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("batek-keyword-edits-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("keywords.json")
    }

    fn store(tag: &str) -> KeywordStore {
        KeywordStore::load(temp_path(tag))
    }

    #[test]
    fn added_phrase_matches_and_disabled_one_stops() {
        let edits = KeywordEdits {
            added: vec![AddedPhrase {
                phrase: "тестовое слово".to_string(),
                meme: "baza".to_string(),
            }],
            disabled: vec!["320".to_string()],
        };
        let table = KeywordTable::build(&edits);

        assert_eq!(
            table
                .match_keyword("тестовое слово")
                .map(|(_, action)| action),
            Some(KeywordAction::Meme(Cow::Owned("baza".to_string())))
        );
        // Кириллица и латиница находят одну и ту же добавленную фразу.
        assert_eq!(
            table
                .match_keyword("TESTovoe slovo")
                .map(|(_, action)| action),
            Some(KeywordAction::Meme(Cow::Owned("baza".to_string())))
        );

        // Выключенная встроенная фраза выпадает, её соседи по действию живы.
        assert_eq!(table.match_keyword("320"), None);
        assert_eq!(
            table.match_keyword("шефос").map(|(_, action)| action),
            Some(meme("shefos"))
        );
    }

    #[test]
    fn phrase_validation_rejects_junk() {
        assert_eq!(validate_phrase("  бомжара  ").unwrap(), "бомжара");
        assert!(validate_phrase("   ").is_err());
        assert!(validate_phrase("\n\t").is_err());
        assert!(validate_phrase("\u{0}abc").is_err());
        assert!(validate_phrase("---").is_err());
        assert!(validate_phrase(&"я".repeat(PHRASE_MAX + 1)).is_err());
        assert!(validate_phrase(&"я".repeat(PHRASE_MAX)).is_ok());
    }

    #[test]
    fn pair_rule_needs_cyrillic_and_ascii() {
        assert!(alphabet_pair_ok(["слово", "slovo"].into_iter()));
        assert!(!alphabet_pair_ok(["слово", "ещё"].into_iter()));
        assert!(!alphabet_pair_ok(["slovo"].into_iter()));
        // Цифры считаются ASCII, как и в тесте встроенной таблицы.
        assert!(alphabet_pair_ok(["слово", "320"].into_iter()));
        assert!(!alphabet_pair_ok(std::iter::empty()));
    }

    #[test]
    fn incomplete_pair_is_saved_but_waits_for_the_second_alphabet() {
        let mut store = store("pair-add");

        // Первая фраза только кириллицей принимается, но не срабатывает:
        // у действия ещё нет ASCII-фразы.
        let table = store.add("слово", "novyi_mem").unwrap();
        assert_eq!(
            table.match_keyword("слово"),
            None,
            "неполная пара сработала"
        );
        let sources = table.phrase_sources_for_name("novyi_mem");
        assert!(
            sources
                .iter()
                .any(|(phrase, _, pending)| *phrase == "слово" && *pending),
            "фраза не помечена как ждущая пары: {sources:?}"
        );

        // Вторая фраза складывает пару, и обе начинают срабатывать.
        let table = store.add("slovo", "novyi_mem").unwrap();
        assert_eq!(
            table.match_keyword("слово").map(|(_, action)| action),
            Some(KeywordAction::Meme(Cow::Owned("novyi_mem".to_string())))
        );
        assert_eq!(
            table.match_keyword("slovo").map(|(_, action)| action),
            Some(KeywordAction::Meme(Cow::Owned("novyi_mem".to_string())))
        );

        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn duplicate_phrase_is_refused() {
        let mut store = store("duplicate");
        // «бомжара» уже во встроенной таблице: дубль по ключу не нужен.
        let err = store.add("бомжара", "baza").unwrap_err();
        assert!(err.contains("занята"), "{err}");
        assert!(
            store.edits().added.is_empty(),
            "отказ не должен писать файл"
        );
        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn removal_refuses_to_break_the_alphabet_pair() {
        let mut store = store("pair-remove");
        // У baza ровно одна кириллическая фраза и две ASCII.
        let err = store.remove("база").unwrap_err();
        assert!(err.contains("втором алфавите"), "{err}");
        assert!(!store.edits().disabled.contains(&"база".to_string()));

        // Фраза без пары у своего действия удаляется нормально.
        let table = store.remove("base").unwrap();
        assert_eq!(table.match_keyword("base"), None);
        assert_eq!(
            table.match_keyword("baza").map(|(_, action)| action),
            Some(meme("baza"))
        );

        let _ = std::fs::remove_dir_all(store.path.parent().unwrap());
    }

    #[test]
    fn edits_survive_reload_and_removal_round_trip() {
        let path = temp_path("reload");
        {
            let mut store = KeywordStore::load(path.clone());
            // У baza уже есть оба алфавита, поэтому добавление не делает
            // действие неполным.
            store.add("тестовое", "baza").unwrap();
            store.add("testovoe", "baza").unwrap();
        }

        let mut reloaded = KeywordStore::load(path.clone());
        assert_eq!(reloaded.edits().added.len(), 2);
        assert_eq!(
            reloaded
                .table()
                .match_keyword("тестовое")
                .map(|(_, action)| action),
            Some(meme("baza"))
        );

        // Добавленные фразы убираются совсем: файл возвращается к пустому.
        reloaded.remove("testovoe").unwrap();
        reloaded.remove("тестовое").unwrap();
        assert!(reloaded.edits().added.is_empty());
        assert_eq!(KeywordStore::load(path.clone()).edits().added.len(), 0);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn phrases_for_name_lists_added_and_builtin_sources() {
        let edits = KeywordEdits {
            added: vec![AddedPhrase {
                phrase: "тестовое слово".to_string(),
                meme: "baza".to_string(),
            }],
            disabled: Vec::new(),
        };
        let table = KeywordTable::build(&edits);

        let sources = table.phrase_sources_for_name("baza");
        let phrase = sources
            .iter()
            .find(|(phrase, _, _)| *phrase == "тестовое слово")
            .expect("добавленная фраза не попала в карточку");
        assert!(!phrase.1, "добавленная фраза не встроенная");
        assert!(!phrase.2, "у baza оба алфавита, пара не ждёт");

        let builtin = sources
            .iter()
            .find(|(phrase, _, _)| *phrase == "база")
            .expect("встроенная фраза потерялась");
        assert!(builtin.1, "встроенная фраза помечена как добавленная");
    }
}

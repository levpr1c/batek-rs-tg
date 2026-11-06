//! Стендап-Telegram-бот: один процесс, один транспорт, без базы данных.
//!
//! Bot API не умеет входить в голосовые чаты, поэтому `/play` и `/stop` здесь
//! только отвечают отказом. Остальное: команды с видеороликами, `/meme`,
//! ключевые слова в группах и inline-режим по файлам из `video/`, `fotos/`
//! и `music/`.
//!
//! Без `TELEGRAM_TOKEN` бот не запускается: для него больше нет другого
//! транспорта. Сбой Telegram не паникует: диспетчер запускается в отдельной
//! задаче и перезапускается с задержкой 5, 10, 20, 40, 60 с.
//!
//! Inline-режим отдаёт только `cached`-результаты: Telegram не разрешает
//! подставлять локальный файл в inline-ответ, нужен file_id после первой
//! отправки. Ответ строится только из кэша и отдаётся сразу; совпавшие,
//! но ещё не загруженные мемы уходят в фоновую загрузку и попадают в
//! выдачу следующего запроса. Кэш заполняется каждым успешным `/meme`;
//! `TG_CACHE_CHAT` прогревает его на старте.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use rand::seq::IndexedRandom;
use rand::Rng;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommand, FileId, InlineKeyboardButton, InlineKeyboardMarkup, InlineQueryResult,
    InlineQueryResultArticle, InlineQueryResultCachedAudio, InlineQueryResultCachedPhoto,
    InlineQueryResultCachedVideo, InputFile, InputMessageContent, InputMessageContentText, Me,
    Message,
};
use teloxide::update_listeners::polling_default;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::keywords;
use crate::memes::{meme_key, Meme, MemeKind, Memes};
use crate::misc;
use crate::youtube::YoutubeCache;

/// Лимит Bot API на загрузку файла, байт. Файлы больше не отправляются:
/// Telegram отклонит их целиком, поэтому отвечаем до чтения.
pub const TELEGRAM_UPLOAD_LIMIT: u64 = 50 * 1024 * 1024;

/// Сколько мемов прогревать по `TG_CACHE_CHAT`: 50 сообщений это предел
/// разумной пачки на старте, остальные попадут в кэш первым `/meme`.
const WARM_LIMIT: usize = 50;

/// Шанс ответа на ключевое слово в чате. Файл существует и проверен до
/// костей, но молчание в 55% случаев держит бота от ощущения спама.
/// Промах костей не занимает слот лимита KEYWORD_RATE_LIMIT: следующее
/// слово в чате может сработать сразу.
const KEYWORD_REPLY_CHANCE: f64 = 0.45;

type TgResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
pub struct TgData {
    /// Токен хранится здесь, но никогда не логируется.
    pub token: String,
    pub yt: Arc<YoutubeCache>,
    pub memes: Arc<Memes>,
    pub root: Arc<PathBuf>,
    /// file_id по имени мема. Заполняется только успешными отправками:
    /// угадать file_id нельзя, его выдаёт Telegram.
    pub file_ids: Arc<RwLock<HashMap<String, String>>>,
    /// Бронь на фоновую inline-догрузку по имени мема: повторный запрос
    /// не запускает вторую загрузку того же файла.
    pub inline_uploading: Arc<RwLock<HashSet<String>>>,
    /// Лимиты ключевых слов этого процесса: не чаще одного ответа на фразу
    /// в чат за KEYWORD_RATE_LIMIT.
    pub keywords: Arc<keywords::KeywordLimiter>,
    /// Прогрев выполняется один раз за процесс, а не на каждый перезапуск
    /// диспетчера после сетевой ошибки.
    warm_done: Arc<AtomicBool>,
}

impl TgData {
    pub fn new(
        token: String,
        yt: Arc<YoutubeCache>,
        memes: Arc<Memes>,
        root: Arc<PathBuf>,
    ) -> Self {
        Self {
            token,
            yt,
            memes,
            root,
            file_ids: Arc::new(RwLock::new(HashMap::new())),
            inline_uploading: Arc::new(RwLock::new(HashSet::new())),
            keywords: Arc::new(keywords::KeywordLimiter::new()),
            warm_done: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Запуск надзираемой задачи. Ошибки не выходят за пределы бота: сбой
/// перезапускает диспетчер вместо падения процесса, журналирование и backoff
/// здесь же.
pub async fn spawn(data: TgData) -> anyhow::Result<()> {
    if data.token.trim().is_empty() {
        info!("Telegram выключен: нет TELEGRAM_TOKEN");
        return Ok(());
    }

    tokio::spawn(async move {
        const BACKOFF: [u64; 5] = [5, 10, 20, 40, 60];
        let mut attempt = 0usize;

        loop {
            match run(data.clone()).await {
                Ok(()) => info!("Telegram: диспетчер остановлен"),
                Err(e) => error!("Telegram: {e:#}"),
            }

            let delay = BACKOFF[attempt.min(BACKOFF.len() - 1)];
            attempt = attempt.saturating_add(1);
            warn!("Telegram: перезапуск через {delay} с");
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    });

    Ok(())
}

async fn run(data: TgData) -> anyhow::Result<()> {
    let bot = Bot::new(data.token.clone());
    let me = bot.get_me().await.context("get_me")?;
    info!(
        "Telegram: бот @{} запущен, мемов: {}",
        me.username(),
        data.memes.len()
    );

    if let Err(e) = bot.set_my_commands(telegram_commands()).await {
        warn!("Telegram: не удалось обновить список команд: {e}");
    }

    warm_cache(&bot, &data).await;

    let handler = dptree::entry()
        .branch(Update::filter_message().endpoint(on_message))
        .branch(Update::filter_callback_query().endpoint(on_callback))
        .branch(Update::filter_inline_query().endpoint(on_inline));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![data])
        .build();

    // `dispatch` паникует, если первый get_me не удался; `try_` возвращает
    // ошибку, и надзирающая петля перезапускает бота с backoff.
    let listener = polling_default(bot).await;
    dispatcher
        .try_dispatch_with_listener(listener, LoggingErrorHandler::new())
        .await
        .context("диспетчер Telegram завершился")?;

    Ok(())
}

/// Имена команд ascii по правилам Bot API, описания не менее глупые, чем
/// сами команды.
fn telegram_commands() -> Vec<BotCommand> {
    vec![
        BotCommand::new("start", "Запустить бота"),
        BotCommand::new("help", "слєш хелп"),
        BotCommand::new("grandmobile", "грэндмобайл"),
        BotCommand::new("batek", "белые розы"),
        BotCommand::new("timur", "ЗАЧЕМ Я ПОМЕНЯЛ ЕЕ НА ЭТО НА ЧЕРНОБЕЛОЕ"),
        BotCommand::new("victorovich", "В СТАРОМ ПАРКЕ ПАХНЕТ ХВОЙНОЙ ТИШИНОЙ"),
        BotCommand::new("meme", "случайный мем из video/, fotos/ или music/"),
        BotCommand::new("batekfetch", "ЭЭЭФЕТЧИ"),
        BotCommand::new("bomjara", "АКАКФИЛЬМНАНОУТБУКЕВКЛЮЧИТЬ"),
        BotCommand::new(
            "roflvideo",
            "не смешная хуйня про 320 пошути же даун ебучий",
        ),
        BotCommand::new("kupyansk", "купянскнаш"),
        BotCommand::new("play", "Я НАФАРМИЛ КРИПТЫ КАК СКУФ"),
        BotCommand::new("stop", "ВЫКЛЮЧИ ЕГО НАХУЙ"),
    ]
}

/// Один раз на старте, если задан `TG_CACHE_CHAT` с числовым id чата.
/// Ошибки только логируются: без прогрева inline-режим просто пуст.
async fn warm_cache(bot: &Bot, data: &TgData) {
    let Ok(raw) = std::env::var("TG_CACHE_CHAT") else {
        return;
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return;
    }
    let Ok(id) = raw.parse::<i64>() else {
        warn!("Telegram: TG_CACHE_CHAT должен быть числовым id чата, получено {raw:?}");
        return;
    };
    if data.warm_done.swap(true, Ordering::SeqCst) {
        return;
    }

    let chat = ChatId(id);
    let items = data.memes.items();
    let total = items.len().min(WARM_LIMIT);
    if total == 0 {
        warn!("Telegram: прогревать нечего, мемов нет");
        return;
    }
    info!("Telegram: прогрев file_id в чате {id}, мемов: {total}");

    for (index, meme) in items.iter().take(WARM_LIMIT).enumerate() {
        match send_meme(bot, chat, meme, data).await {
            Ok(()) => info!("Telegram: прогрев {}/{}: {}", index + 1, total, meme.name),
            Err(e) => warn!(
                "Telegram: прогрев {}/{}: {} не отправлен: {e}",
                index + 1,
                total,
                meme.name
            ),
        }
        // Без throttle у teloxide пачка из 50 сообщений упирается в лимиты
        // Telegram; 100 мс держат темп около 10 сообщений в секунду.
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn on_message(bot: Bot, msg: Message, me: Me, data: TgData) -> TgResult {
    let Some(text) = msg.text() else {
        return Ok(());
    };

    // Диагностика: видно, доходят ли сообщения вообще и что в них. Включается
    // через RUST_LOG=batek_rs_tg=debug.
    debug!(
        chat = msg.chat.id.0,
        chat_kind = ?msg.chat.kind,
        from_bot = msg.from.as_ref().is_some_and(|u| u.is_bot),
        text = %text,
        "Telegram: входящее сообщение"
    );

    if let Some((command, args)) = parse_command(text, me.username()) {
        return handle_command(&bot, &msg, &data, &command, &args).await;
    }

    if should_react_to_keyword(&msg, text, me.username()) {
        react_to_keyword(&bot, &msg, &data, text).await;
    }

    handle_mention(&bot, &msg, &data, me.username(), text).await
}

/// Реакция на ключевые слова только в группах и супергруппах, только на
/// текст от человека и только когда это не команда и не упоминание бота:
/// иначе одно сообщение получило бы два ответа.
fn should_react_to_keyword(msg: &Message, text: &str, username: &str) -> bool {
    if !(msg.chat.is_group() || msg.chat.is_supergroup()) {
        return false;
    }
    if msg.from.as_ref().is_some_and(|user| user.is_bot) {
        return false;
    }
    if text.trim_start().starts_with('/') {
        return false;
    }

    let mention = format!("@{username}").to_ascii_lowercase();
    !text.to_ascii_lowercase().contains(&mention)
}

/// Ищет фразу как целые слова без учёта регистра. Границей считается любой
/// не-буквенно-цифровой символ, поэтому `неграндмобайл` не совпадает.
fn contains_keyword(text: &str, phrase: &str) -> bool {
    let haystack = text.to_lowercase();
    let needle = phrase.to_lowercase();
    if needle.is_empty() {
        return false;
    }

    let mut from = 0usize;
    while let Some(offset) = haystack[from..].find(&needle) {
        let start = from + offset;
        let end = start + needle.len();
        let left_ok = haystack[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let right_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if left_ok && right_ok {
            return true;
        }

        // start всегда на границе char, поэтому следующий срез валиден.
        from = start + haystack[start..].chars().next().map_or(1, char::len_utf8);
    }

    false
}

async fn react_to_keyword(bot: &Bot, msg: &Message, data: &TgData, text: &str) {
    let Some((phrase, action)) = keywords::match_keyword(text) else {
        return;
    };
    let chat = msg.chat.id;

    // Файл проверяется до отправки: пропавший мем не должен превращаться
    // в ответ об ошибке и не должен занимать окно лимита.
    let meme = match action {
        keywords::KeywordAction::Grandmobile => None,
        keywords::KeywordAction::Meme(needle) => {
            let Some(meme) = data.memes.find(needle) else {
                warn!("Telegram: ключевое слово '{phrase}': мем '{needle}' не найден");
                return;
            };
            if tokio::fs::metadata(&meme.path).await.is_err() {
                warn!(
                    "Telegram: ключевое слово '{phrase}': файл мема '{}' не найден",
                    meme.name
                );
                return;
            }
            Some(meme)
        }
        keywords::KeywordAction::RandomMeme(needles) => {
            let mut candidates = keywords::random_meme_candidates(&data.memes, needles);
            let mut existing = Vec::with_capacity(candidates.len());
            for meme in candidates.drain(..) {
                if tokio::fs::metadata(&meme.path).await.is_ok() {
                    existing.push(meme);
                } else {
                    warn!(
                        "Telegram: ключевое слово '{phrase}': файл мема '{}' не найден",
                        meme.name
                    );
                }
            }
            let Some(meme) = keywords::pick_random_meme(&mut existing) else {
                warn!("Telegram: ключевое слово '{phrase}': нет доступных мемов");
                return;
            };
            Some(meme)
        }
    };

    // Кости до лимита: молчание не должно занимать слот 20 с, слот уходит
    // только реальной отправке. Текст без файла (паста) по костям не играет.
    if meme.is_some() && !rand::rng().random_bool(KEYWORD_REPLY_CHANCE) {
        debug!("Telegram: ключевое слово '{phrase}': кости промолчали");
        return;
    }

    if !data.keywords.allow(chat.0, phrase).await {
        return;
    }

    match meme {
        Some(meme) => send_meme_logged(bot, chat, meme, data).await,
        None => {
            if let Err(e) = bot.send_message(chat, misc::grandmobile_text()).await {
                warn!("Telegram: ключевое слово '{phrase}': {e}");
            }
        }
    }
}

fn parse_command(text: &str, username: &str) -> Option<(String, String)> {
    let text = text.trim();
    let (head, rest) = match text.split_once(char::is_whitespace) {
        Some((head, rest)) => (head, rest.trim()),
        None => (text, ""),
    };

    let command = head.strip_prefix('/')?;
    let (command, target) = match command.split_once('@') {
        Some((command, target)) => (command, Some(target)),
        None => (command, None),
    };

    if let Some(target) = target {
        if !target.eq_ignore_ascii_case(username) {
            return None;
        }
    }
    if command.is_empty() {
        return None;
    }

    Some((command.to_ascii_lowercase(), rest.to_string()))
}

async fn handle_command(
    bot: &Bot,
    msg: &Message,
    data: &TgData,
    command: &str,
    args: &str,
) -> TgResult {
    let chat = msg.chat.id;

    match command {
        "start" => {
            bot.send_message(
                chat,
                "Бот запущен. /help покажет команды, /meme отправит мем.",
            )
            .await?;
        }
        "help" => {
            bot.send_message(chat, "аидитынахуй").await?;
        }
        "grandmobile" => {
            bot.send_message(chat, misc::grandmobile_text()).await?;
        }
        "batek" => video_choice(bot, msg, "b").await?,
        "timur" => video_choice(bot, msg, "t").await?,
        "victorovich" => video_choice(bot, msg, "v").await?,
        "meme" => meme_command(bot, chat, data, args).await?,
        "batekfetch" => {
            let info = tokio::task::spawn_blocking(misc::fetch_info).await?;
            bot.send_message(chat, misc::format_fetch(&info)).await?;
        }
        "bomjara" => {
            let file = if rand::rng().random_bool(0.5) {
                "bomjara1.mp4"
            } else {
                "bomjaro2.mp4"
            };
            send_root_video(bot, chat, data, file).await?;
        }
        "roflvideo" => {
            let file = if rand::rng().random_bool(0.5) {
                "makuos.mp4"
            } else {
                "shefos.mp4"
            };
            send_root_video(bot, chat, data, file).await?;
        }
        "kupyansk" => send_root_video(bot, chat, data, "kupyanskdis.mp4").await?,
        "play" | "stop" => {
            bot.send_message(chat, "голосов в Telegram нет, этот бот без музыки")
                .await?;
        }
        _ => {
            bot.send_message(chat, "❌ Неизвестная команда. Отправь /help.")
                .await?;
        }
    }

    Ok(())
}

/// Python: `randint(1, 100) > 10` отправлял кнопки, то есть ровно 10% на шутку.
fn should_aiditinaxuy() -> bool {
    rand::rng().random_range(1..=100) <= 10
}

async fn video_choice(bot: &Bot, msg: &Message, key: &str) -> TgResult {
    let chat = msg.chat.id;

    if should_aiditinaxuy() {
        bot.send_message(chat, "аидитынахуй").await?;
        return Ok(());
    }

    let owner = msg.from.as_ref().map_or(0, |user| user.id.0);
    let shorts = format!("v:{owner}:{key}:s");
    let videos = format!("v:{owner}:{key}:d");

    let keyboard = InlineKeyboardMarkup::new(vec![vec![
        InlineKeyboardButton::callback("📱 Случайный Шортс", shorts),
        InlineKeyboardButton::callback("📺 Обычное видео", videos),
    ]]);

    bot.send_message(chat, "че смотреть будем")
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

/// `v:<owner>:<b|t|v>:<s|d>`. В callback data нельзя класть ничего
/// приватного: её видит клиент, поэтому здесь только id и ключ канала.
fn parse_callback(data: &str) -> Option<(u64, &'static str, char)> {
    let mut parts = data.split(':');
    if parts.next()? != "v" {
        return None;
    }
    let owner = parts.next()?.parse().ok()?;
    let key = match parts.next()? {
        "b" => "batek",
        "t" => "timur",
        "v" => "victorovich",
        _ => return None,
    };
    let kind = match parts.next()? {
        "s" => 's',
        "d" => 'd',
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((owner, key, kind))
}

async fn on_callback(bot: Bot, query: CallbackQuery, data: TgData) -> TgResult {
    let Some(payload) = query.data.as_deref() else {
        let _ = bot.answer_callback_query(query.id.clone()).await;
        return Ok(());
    };
    let Some((owner, key, kind)) = parse_callback(payload) else {
        let _ = bot.answer_callback_query(query.id.clone()).await;
        return Ok(());
    };

    if query.from.id.0 != owner {
        let _ = bot
            .answer_callback_query(query.id.clone())
            .text("это не твои кнопки")
            .show_alert(true)
            .await;
        return Ok(());
    }

    let _ = bot.answer_callback_query(query.id.clone()).await;

    let Some(message) = query.regular_message() else {
        return Ok(());
    };
    let chat = message.chat.id;

    let cache_key = match kind {
        's' => format!("{key}:shorts"),
        _ => format!("{key}:videos"),
    };
    let ids = data.yt.get(&cache_key).await;

    let text = match ids.choose(&mut rand::rng()) {
        Some(id) if kind == 's' => format!("Вот твой шортс:\nhttps://youtu.be/{id}"),
        Some(id) => format!("Вот обычный видос:\nhttps://youtu.be/{id}"),
        None if kind == 's' => "Шортсов пока нет :(".to_string(),
        None => "Обычных видео пока нет :(".to_string(),
    };
    bot.send_message(chat, text).await?;

    Ok(())
}

async fn meme_command(bot: &Bot, chat: ChatId, data: &TgData, args: &str) -> TgResult {
    let query = args.trim();

    if let Some(meme) = if query.is_empty() {
        data.memes.random()
    } else {
        data.memes.find(query)
    } {
        send_meme_logged(bot, chat, meme, data).await;
        return Ok(());
    }

    let mut text = if query.is_empty() {
        "❌ Мемы не найдены. Положи файлы в video/, fotos/ или music/.".to_string()
    } else {
        format!("❌ Мем '{query}' не найден.")
    };

    let names = data.memes.names();
    if !names.is_empty() {
        text.push_str("\nДоступные: ");
        text.push_str(
            &names
                .iter()
                .take(10)
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
        if names.len() > 10 {
            text.push_str(", ...");
        }
    }

    bot.send_message(chat, text).await?;
    Ok(())
}

/// Текст без имени: в группах бот должен понимать `@bot мем` и `мем @bot`.
async fn handle_mention(
    bot: &Bot,
    msg: &Message,
    data: &TgData,
    username: &str,
    text: &str,
) -> TgResult {
    let mention = format!("@{username}").to_ascii_lowercase();
    let haystack = text.to_ascii_lowercase();
    let Some(position) = haystack.find(&mention) else {
        return Ok(());
    };

    // to_ascii_lowercase не меняет длину в байтах, поэтому границы среза
    // остаются валидными даже для кириллицы вокруг упоминания.
    let remainder = format!("{}{}", &text[..position], &text[position + mention.len()..]);
    let remainder = remainder.trim();
    if remainder.is_empty() {
        return Ok(());
    }

    match data.memes.find(remainder) {
        Some(meme) => send_meme_logged(bot, msg.chat.id, meme, data).await,
        None => debug!("Telegram: упоминание без мема: {remainder:?}"),
    }
    Ok(())
}

async fn send_meme_logged(bot: &Bot, chat: ChatId, meme: &Meme, data: &TgData) {
    if let Err(e) = send_meme(bot, chat, meme, data).await {
        error!("Telegram: мем {} не отправлен: {e}", meme.name);
        let _ = bot.send_message(chat, "❌ Не удалось отправить мем.").await;
    }
}

async fn send_meme(
    bot: &Bot,
    chat: ChatId,
    meme: &Meme,
    data: &TgData,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let metadata = match tokio::fs::metadata(&meme.path).await {
        Ok(metadata) => metadata,
        Err(_) => {
            bot.send_message(chat, format!("❌ Файл '{}' не найден.", meme.name))
                .await?;
            return Ok(());
        }
    };

    if metadata.len() > TELEGRAM_UPLOAD_LIMIT {
        bot.send_message(chat, "❌ Файл больше 50 МБ, Telegram его не примет.")
            .await?;
        return Ok(());
    }

    let file = InputFile::file(&meme.path);
    let message = match meme.kind {
        MemeKind::Video => bot.send_video(chat, file).await?,
        MemeKind::Image => bot.send_photo(chat, file).await?,
        MemeKind::Audio => bot.send_audio(chat, file).await?,
    };

    if let Some(file_id) = message_file_id(&message) {
        data.file_ids
            .write()
            .await
            .insert(meme.name.clone(), file_id);
    }

    Ok(())
}

/// Telegram может вернуть видео документом (например webm), поэтому смотрим
/// все четыре поля, которые перечислены в задаче.
fn message_file_id(message: &Message) -> Option<String> {
    if let Some(video) = message.video() {
        return Some(video.file.id.0.clone());
    }
    if let Some(audio) = message.audio() {
        return Some(audio.file.id.0.clone());
    }
    if let Some(document) = message.document() {
        return Some(document.file.id.0.clone());
    }
    // Telegram отдаёт размеры по возрастанию; последний элемент самый большой.
    message
        .photo()
        .and_then(|sizes| sizes.last())
        .map(|photo| photo.file.id.0.clone())
}

async fn send_root_video(bot: &Bot, chat: ChatId, data: &TgData, filename: &str) -> TgResult {
    let path = data.root.join("video").join(filename);
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
        Err(_) => {
            bot.send_message(chat, format!("❌ Файл '{filename}' не найден."))
                .await?;
            return Ok(());
        }
    };

    if metadata.len() > TELEGRAM_UPLOAD_LIMIT {
        bot.send_message(chat, "❌ Файл больше 50 МБ, Telegram его не примет.")
            .await?;
        return Ok(());
    }

    if let Err(e) = bot.send_video(chat, InputFile::file(path)).await {
        error!("Telegram: файл {filename} не отправлен: {e}");
        let _ = bot
            .send_message(chat, "❌ Не удалось отправить файл.")
            .await;
    }
    Ok(())
}

/// Предел Bot API на число результатов в одном inline-ответе.
const INLINE_RESULT_LIMIT: usize = 50;

/// Сколько мемов показывать в меню пустого запроса: список должен быть
/// достаточно длинным, чтобы выбрать, и не дублировать весь каталог.
const INLINE_PICKER_LIMIT: usize = 20;

/// Каналы, чьи шортсы и видео объединяются в случайную выдачу.
const INLINE_CHANNELS: [&str; 3] = ["batek", "timur", "victorovich"];

/// Команды, которые можно набрать в inline-запросе. `/play` и `/stop` сюда
/// не входят: в Telegram они только отвечают отказом.
const INLINE_COMMANDS: [&str; 11] = [
    "start",
    "help",
    "meme",
    "batek",
    "timur",
    "victorovich",
    "grandmobile",
    "batekfetch",
    "bomjara",
    "roflvideo",
    "kupyansk",
];

/// Действие inline-запроса, совпавшего с командой.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineCommand {
    /// Готовые файлы из кэша: подстроки имён мемов.
    Media(&'static [&'static str]),
    /// Два случайных ролика одного канала: шортс и обычное видео.
    RandomVideo(&'static str),
    /// Текст, который отправляется как есть.
    Text(InlineText),
    /// Обычный поиск по именам мемов.
    Meme,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineText {
    CommandList,
    Grandmobile,
}

/// Команда и её inline-содержимое. Таблица одна, чтобы новое соответствие
/// добавлялось строкой, а не ещё одной ветвью в обработчике.
const INLINE_QUERY_COMMANDS: [(&str, InlineCommand); 11] = [
    ("bomjara", InlineCommand::Media(&["bomjara1", "bomjaro2"])),
    ("roflvideo", InlineCommand::Media(&["makuos", "shefos"])),
    ("kupyansk", InlineCommand::Media(&["kupyanskdis"])),
    ("meme", InlineCommand::Meme),
    ("batek", InlineCommand::RandomVideo("batek")),
    ("timur", InlineCommand::RandomVideo("timur")),
    ("victorovich", InlineCommand::RandomVideo("victorovich")),
    ("help", InlineCommand::Text(InlineText::CommandList)),
    ("start", InlineCommand::Text(InlineText::CommandList)),
    ("grandmobile", InlineCommand::Text(InlineText::Grandmobile)),
    ("batekfetch", InlineCommand::Text(InlineText::CommandList)),
];

/// Список команд для inline-меню: те же описания, что и в
/// `telegram_commands()`, по одной команде на строку. `/play` и `/stop`
/// убраны: в Telegram они только отвечают отказом. Чистая функция, чтобы
/// состав списка проверялся тестом.
fn command_list_text() -> String {
    telegram_commands()
        .into_iter()
        .filter(|command| command.command != "play" && command.command != "stop")
        .map(|command| format!("/{} - {}", command.command, command.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Inline-запрос считается префиксом имени команды без учёта регистра.
/// Пустой запрос и запрос с пробелами командой не считаются: для них
/// строится обычное меню или поиск по мемам.
fn command_prefix(query: &str) -> Option<&'static str> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() || needle.contains(char::is_whitespace) {
        return None;
    }
    INLINE_COMMANDS
        .iter()
        .copied()
        .find(|command| command.starts_with(&needle))
}

/// Запрос совпадает с командой, если это её точное имя, префикс без пробелов
/// или команда как отдельное слово внутри фразы. Регистр не важен.
fn inline_command(query: &str) -> Option<InlineCommand> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }

    if let Some((_, action)) = INLINE_QUERY_COMMANDS
        .iter()
        .copied()
        .find(|(command, _)| *command == needle.as_str())
    {
        return Some(action);
    }

    // Префикс ищется по тому же списку команд, что и раньше: так запрос
    // "bom" и "bomjara" ведут к одному действию.
    if let Some(command) = command_prefix(&needle) {
        if let Some((_, action)) = INLINE_QUERY_COMMANDS
            .iter()
            .copied()
            .find(|(name, _)| *name == command)
        {
            return Some(action);
        }
    }

    INLINE_QUERY_COMMANDS
        .iter()
        .copied()
        .find(|(command, _)| contains_keyword(&needle, command))
        .map(|(_, action)| action)
}

fn article_result(
    id: impl Into<String>,
    title: impl Into<String>,
    description: impl Into<String>,
    text: impl Into<String>,
) -> InlineQueryResult {
    let article = InlineQueryResultArticle::new(
        id,
        title,
        InputMessageContent::Text(InputMessageContentText::new(text)),
    )
    .description(description);
    InlineQueryResult::Article(article)
}

fn cached_meme_result(id: impl Into<String>, meme: &Meme, file_id: &str) -> InlineQueryResult {
    cached_meme_result_titled(id, meme, file_id, meme.name.clone())
}

fn cached_meme_result_titled(
    id: impl Into<String>,
    meme: &Meme,
    file_id: &str,
    title: String,
) -> InlineQueryResult {
    let id = id.into();
    let file_id = FileId::from(file_id.to_string());

    match meme.kind {
        MemeKind::Video => {
            InlineQueryResult::CachedVideo(InlineQueryResultCachedVideo::new(id, file_id, title))
        }
        MemeKind::Image => InlineQueryResult::CachedPhoto(
            InlineQueryResultCachedPhoto::new(id, file_id).title(title),
        ),
        // У cached audio нет title, поэтому имя уходит в caption.
        MemeKind::Audio => InlineQueryResult::CachedAudio(
            InlineQueryResultCachedAudio::new(id, file_id).caption(title),
        ),
    }
}

/// Случайный id из кэша перечисленных каналов. Пустой кэш даёт `None`:
/// действие без ссылки в меню не попадает.
async fn random_youtube_ids(data: &TgData, channels: &[&str], suffix: &str) -> Option<String> {
    let mut ids = Vec::new();
    for channel in channels {
        ids.extend(data.yt.get(&format!("{channel}:{suffix}")).await);
    }
    ids.choose(&mut rand::rng()).cloned()
}

/// Случайный id по всем каналам: так устроено меню пустого запроса.
async fn random_youtube_id(data: &TgData, suffix: &str) -> Option<String> {
    random_youtube_ids(data, &INLINE_CHANNELS, suffix).await
}

/// Сколько файлов разрешено запустить фоном за один inline-запрос: больше
/// трёх упирается в лимиты Telegram и не нужна задержка ответа.
const INLINE_LAZY_UPLOADS: usize = 3;

/// Догружает мем в личный чат, запоминает выданный file_id и удаляет
/// сообщение, чтобы не засорять переписку. Вызывается только из фоновых
/// задач: await здесь не имеет права задерживать ответ на inline-запрос.
/// Возвращает file_id только при полном успехе.
async fn upload_for_inline(bot: &Bot, user: ChatId, meme: Meme, data: &TgData) -> Option<String> {
    // Проверка до отправки: send_meme на ошибке пишет сообщение в чат, а в
    // личном чате это был бы мусор от неудачного прогрева.
    match tokio::fs::metadata(&meme.path).await {
        Ok(metadata) if metadata.len() <= TELEGRAM_UPLOAD_LIMIT => {}
        Ok(_) => {
            debug!("Telegram: inline: файл {} больше лимита", meme.name);
            return None;
        }
        Err(e) => {
            debug!("Telegram: inline: файл {} недоступен: {e}", meme.name);
            return None;
        }
    }

    let file = InputFile::file(&meme.path);
    let sent = match meme.kind {
        MemeKind::Video => bot.send_video(user, file).await,
        MemeKind::Image => bot.send_photo(user, file).await,
        MemeKind::Audio => bot.send_audio(user, file).await,
    };
    let message = match sent {
        Ok(message) => message,
        Err(e) => {
            // Самая частая причина: пользователь ни разу не запускал бота.
            debug!("Telegram: inline: мем {} не загружен: {e}", meme.name);
            return None;
        }
    };

    let file_id = message_file_id(&message);
    if let Some(file_id) = &file_id {
        data.file_ids
            .write()
            .await
            .insert(meme.name.clone(), file_id.clone());
    }

    if let Err(e) = bot.delete_message(user, message.id).await {
        debug!(
            "Telegram: inline: сообщение прогрева {} не удалено: {e}",
            meme.name
        );
    }

    file_id
}

/// Готовность ответа по совпавшим мемам.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MemeReadiness {
    /// Хотя бы один file_id взят из кэша: есть что показывать сразу.
    Cached,
    /// Мемы нашлись, но кэш пуст. Фоновая загрузка запланирована, повторный
    /// запрос через секунду увидит результат.
    Preparing,
    /// Совпадений нет.
    NoMatch,
}

/// Готовит выдачу по совпавшим мемам, не выходя за пределы кэша: ждёт
/// только чтение file_ids, загрузок здесь нет. Совпавшие мемы без file_id
/// возвращаются списком на фоновую догрузку, ограниченным бюджетом запроса.
async fn serve_matched_memes<F>(
    data: &TgData,
    matched: &[&Meme],
    results: &mut Vec<InlineQueryResult>,
    mut result_id: F,
    in_flight: &mut HashSet<String>,
) -> (MemeReadiness, Vec<Meme>)
where
    F: FnMut(&Meme, usize) -> String,
{
    let cache = data.file_ids.read().await;
    let mut readiness = MemeReadiness::NoMatch;
    let mut pending: Vec<Meme> = Vec::new();

    for &meme in matched {
        match cache.get(&meme.name) {
            Some(file_id) => {
                if results.len() >= INLINE_RESULT_LIMIT {
                    break;
                }
                readiness = MemeReadiness::Cached;
                let id = result_id(meme, results.len());
                results.push(cached_meme_result(id, meme, file_id));
            }
            None => {
                if readiness == MemeReadiness::NoMatch {
                    readiness = MemeReadiness::Preparing;
                }
                if pending.len() < INLINE_LAZY_UPLOADS {
                    pending.push(meme.clone());
                }
            }
        }
    }
    drop(cache);

    (readiness, queue_pending_uploads(pending, in_flight))
}

/// Отбирает мемы на фоновую догрузку: бюджет на запрос и бронь против
/// дубликатов. Чистая книга учёта без сети, чтобы повторный запрос не
/// запускал вторую загрузку того же файла.
fn queue_pending_uploads(pending: Vec<Meme>, in_flight: &mut HashSet<String>) -> Vec<Meme> {
    let mut queued = Vec::new();
    for meme in pending {
        if queued.len() >= INLINE_LAZY_UPLOADS {
            break;
        }
        // insert возвращает false, если имя уже в работе: такой мем
        // пропускается без списания бюджета.
        if in_flight.insert(meme.name.clone()) {
            queued.push(meme);
        }
    }
    queued
}

/// Запускает фоновую догрузку уже отобранных мемов. Вызывается после
/// ответа: await внутри не влияет на срок ответа inline-запроса.
fn spawn_inline_uploads(bot: &Bot, user: ChatId, queued: Vec<Meme>, data: &TgData) {
    for meme in queued {
        let bot = bot.clone();
        let data = data.clone();
        tokio::spawn(async move {
            let name = meme.name.clone();
            // Кэш могли прогреть между бронью и запуском задачи.
            if data.file_ids.read().await.contains_key(&name) {
                data.inline_uploading.write().await.remove(&name);
                return;
            }
            upload_for_inline(&bot, user, meme, &data).await;
            // Бронь снимается после попытки: удачная загрузка дальше читается
            // из кэша, неудачная позволяет следующему запросу повторить.
            data.inline_uploading.write().await.remove(&name);
        });
    }
}

/// Подсказка, когда мемы совпали, но кэш пуст: загрузка запущена, файл
/// появится в выдаче следующего запроса, не этого.
fn preparing_article() -> InlineQueryResult {
    article_result(
        "preparing",
        "Мем загружается",
        "файл появится через секунду",
        "Мем нашёлся, но ещё не в кэше. Загрузка идёт в фоне, повторите запрос через секунду.",
    )
}

async fn on_inline(bot: Bot, query: InlineQuery, data: TgData) -> TgResult {
    let needle = query.query.trim().to_lowercase();
    let mut results: Vec<InlineQueryResult> = Vec::new();

    if needle.is_empty() {
        results.push(article_result(
            "commands",
            "Выберите команду",
            "список команд и мемов",
            command_list_text(),
        ));

        if let Some(id) = random_youtube_id(&data, "shorts").await {
            results.push(article_result(
                "yt-shorts",
                "🎲 Случайный шортс",
                "случайный шортс",
                format!("https://youtu.be/{id}"),
            ));
        }
        if let Some(id) = random_youtube_id(&data, "videos").await {
            results.push(article_result(
                "yt-videos",
                "📺 Случайное видео",
                "случайное видео",
                format!("https://youtu.be/{id}"),
            ));
        }

        let cache = data.file_ids.read().await;
        let cached: Vec<(&Meme, &String)> = data
            .memes
            .items()
            .iter()
            .filter_map(|meme| cache.get(&meme.name).map(|file_id| (meme, file_id)))
            .collect();

        let random_name = cached
            .choose(&mut rand::rng())
            .map(|(meme, _)| meme.name.clone());
        if let Some(name) = &random_name {
            if let Some((meme, file_id)) = cached.iter().find(|(meme, _)| &meme.name == name) {
                results.push(cached_meme_result_titled(
                    "meme-random",
                    meme,
                    file_id,
                    "🎵 Случайный мем".to_string(),
                ));
            }
        }

        let mut added = 0usize;
        for (meme, file_id) in &cached {
            if added >= INLINE_PICKER_LIMIT || results.len() >= INLINE_RESULT_LIMIT {
                break;
            }
            if Some(&meme.name) == random_name.as_ref() {
                continue;
            }
            results.push(cached_meme_result(format!("m{added}"), meme, file_id));
            added += 1;
        }
    } else {
        // Личный чат автора запроса: туда фоном уходит файл, чтобы получить
        // file_id, и оттуда же удаляется сообщение. Сам ответ строится
        // только из кэша: загрузка лежит вне окна inline-ответа, ожидание
        // в этом обработчике давало "query is too old".
        let user = ChatId(query.from.id.0 as i64);

        match inline_command(&needle) {
            // Команда с готовыми файлами: отдаём сами файлы из кэша, а
            // недостающие file_id догружаются фоном. Если кэш пуст, но мем
            // нашёлся, подсказка просит повторить запрос; если мема нет
            // вообще, ответ прежний, текстом.
            Some(InlineCommand::Media(needles)) => {
                // Точный ключ, а не опечатка: иначе `bomjaro2` при отсутствии
                // файла подтянул бы `bomjara1` вторым результатом с тем же id.
                let matched = needles
                    .iter()
                    .filter_map(|needle| data.memes.find(needle))
                    .filter(|meme| {
                        needles
                            .iter()
                            .any(|needle| meme_key(&meme.name) == meme_key(needle))
                    })
                    .collect::<Vec<_>>();
                let (readiness, queued) = serve_matched_memes(
                    &data,
                    &matched,
                    &mut results,
                    |meme, _| meme.name.clone(),
                    &mut *data.inline_uploading.write().await,
                )
                .await;
                spawn_inline_uploads(&bot, user, queued, &data);
                match readiness {
                    MemeReadiness::Cached => {}
                    MemeReadiness::Preparing => results.push(preparing_article()),
                    MemeReadiness::NoMatch => {
                        results.push(article_result(
                            "media-cold",
                            "Мем ещё не в кэше",
                            "появится после /meme или прогрева",
                            "Этот мем появится в inline после первой отправки /meme <имя> \
                             в этом чате или после прогрева TG_CACHE_CHAT.",
                        ));
                    }
                }
            }
            // Два действия для канала: шортс и обычное видео.
            Some(InlineCommand::RandomVideo(channel)) => {
                if let Some(id) = random_youtube_ids(&data, &[channel], "shorts").await {
                    results.push(article_result(
                        "yt-shorts",
                        "🎲 Случайный шортс",
                        "случайный шортс",
                        format!("https://youtu.be/{id}"),
                    ));
                }
                if let Some(id) = random_youtube_ids(&data, &[channel], "videos").await {
                    results.push(article_result(
                        "yt-videos",
                        "📺 Случайное видео",
                        "случайное видео",
                        format!("https://youtu.be/{id}"),
                    ));
                }
                if results.is_empty() {
                    results.push(article_result(
                        "yt-empty",
                        "Видео ещё не загружены",
                        "кэш канала пуст",
                        "Кэш видео этого канала пока пуст, попробуй позже.",
                    ));
                }
            }
            // Текстовые команды отдают свой текст целиком.
            Some(InlineCommand::Text(kind)) => {
                let (title, text) = match kind {
                    InlineText::CommandList => ("Список команд", command_list_text()),
                    InlineText::Grandmobile => {
                        ("Гранд мобайл", misc::grandmobile_text().to_string())
                    }
                };
                results.push(article_result(
                    "command-text",
                    title,
                    "отправит текст",
                    text,
                ));
            }
            // "meme" и точное имя мема идут обычным поиском по подстроке.
            // Сначала общая таблица триггеров с группами: алиасы работают
            // здесь так же, как в чатах. Совпадение по таблице исключает
            // обычный поиск по именам, чтобы не задваивать те же файлы.
            Some(InlineCommand::Meme) | None => {
                let matched: Vec<&Meme> =
                    if let Some((phrase, action)) = keywords::match_keyword_prefix(&needle) {
                        debug!(
                            "Telegram: inline: '{}' по таблице '{}' ({phrase:?})",
                            needle, phrase
                        );
                        match action {
                            keywords::KeywordAction::Grandmobile => {
                                results.push(article_result(
                                    "keyword-grandmobile",
                                    "Гранд мобайл",
                                    "отправит текст",
                                    misc::grandmobile_text().to_string(),
                                ));
                                Vec::new()
                            }
                            keywords::KeywordAction::Meme(meme_needle) => {
                                data.memes.find(meme_needle).into_iter().collect()
                            }
                            keywords::KeywordAction::RandomMeme(random_needles) => {
                                // Все кандидаты отдельными результатами: и
                                // bomjara1, и bomjaro2, а не один случайный.
                                // Каталог уже отсортирован по имени, порядок
                                // выдачи детерминированный.
                                keywords::random_meme_candidates(&data.memes, random_needles)
                            }
                        }
                    } else {
                        data.memes.matches(&needle)
                    };

                let (readiness, queued) = serve_matched_memes(
                    &data,
                    &matched,
                    &mut results,
                    |_, len| format!("k{len}"),
                    &mut *data.inline_uploading.write().await,
                )
                .await;
                spawn_inline_uploads(&bot, user, queued, &data);

                match readiness {
                    MemeReadiness::Cached => {}
                    MemeReadiness::Preparing => results.insert(0, preparing_article()),
                    // Оставшийся без результатов текстом случай: ключ не
                    // совпал и по таблице, и по именам, либо совпавший
                    // мем вообще не лежит в каталоге.
                    MemeReadiness::NoMatch => {
                        if results.is_empty() {
                            results.push(article_result(
                                "not-found",
                                "Ничего не найдено",
                                "список команд",
                                command_list_text(),
                            ));
                        }
                    }
                }
            }
        }
    }

    if let Err(e) = bot.answer_inline_query(query.id, results).await {
        error!("Telegram: inline-ответ не отправлен: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_and_without_mention() {
        assert_eq!(
            parse_command("/meme белые розы", "batek_bot"),
            Some(("meme".to_string(), "белые розы".to_string()))
        );
        assert_eq!(
            parse_command("/meme@batek_bot купянск", "batek_bot"),
            Some(("meme".to_string(), "купянск".to_string()))
        );
        assert_eq!(
            parse_command("/MEME@BATEK_BOT", "batek_bot"),
            Some(("meme".to_string(), String::new()))
        );
        assert_eq!(parse_command("/meme@other_bot x", "batek_bot"), None);
        assert_eq!(parse_command("мем", "batek_bot"), None);
        assert_eq!(parse_command("/", "batek_bot"), None);
    }

    #[test]
    fn parses_callback_payload_and_rejects_junk() {
        assert_eq!(parse_callback("v:42:t:s"), Some((42, "timur", 's')));
        assert_eq!(parse_callback("v:42:v:d"), Some((42, "victorovich", 'd')));
        assert_eq!(parse_callback("v:x:t:s"), None);
        assert_eq!(parse_callback("v:42:t:x"), None);
        assert_eq!(parse_callback("v:42:z:s"), None);
        assert_eq!(parse_callback("v:42:t:s:extra"), None);
        assert_eq!(parse_callback("x:42:t:s"), None);
    }

    #[test]
    fn joke_is_about_ten_percent() {
        let hits = (0..10_000).filter(|_| super::should_aiditinaxuy()).count();
        // 10% от 10 000 это 1000; допускаем статистический разброс.
        assert!((850..=1150).contains(&hits), "выпало {hits}");
    }

    #[test]
    fn command_list_has_all_commands_one_per_line() {
        let text = command_list_text();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 11, "ожидалось 11 команд, получено:\n{text}");

        for command in [
            "start",
            "help",
            "grandmobile",
            "batek",
            "timur",
            "victorovich",
            "meme",
            "batekfetch",
            "bomjara",
            "roflvideo",
            "kupyansk",
        ] {
            let prefix = format!("/{command} ");
            assert!(
                lines.iter().any(|line| line.starts_with(&prefix)),
                "нет команды {command} в списке:\n{text}"
            );
        }
    }

    #[test]
    fn command_prefix_is_case_insensitive_and_prefix_only() {
        assert_eq!(command_prefix("HE"), Some("help"));
        assert_eq!(command_prefix("Meme"), Some("meme"));
        assert_eq!(command_prefix("batek"), Some("batek"));
        assert_eq!(command_prefix("batekf"), Some("batekfetch"));
        assert_eq!(command_prefix("kUP"), Some("kupyansk"));
        assert_eq!(command_prefix("elp"), None);
        assert_eq!(command_prefix("video"), None);
        assert_eq!(command_prefix(""), None);
        assert_eq!(command_prefix("   "), None);
        assert_eq!(command_prefix("zzz"), None);
    }

    #[test]
    fn inline_command_maps_media_commands_to_meme_names() {
        assert_eq!(
            inline_command("bomjara"),
            Some(InlineCommand::Media(&["bomjara1", "bomjaro2"]))
        );
        assert_eq!(
            inline_command("roflvideo"),
            Some(InlineCommand::Media(&["makuos", "shefos"]))
        );
        assert_eq!(
            inline_command("KUPYANSK"),
            Some(InlineCommand::Media(&["kupyanskdis"]))
        );
        // Префикс и слово внутри фразы ведут к тому же файлу.
        assert_eq!(
            inline_command("BOMJ"),
            Some(InlineCommand::Media(&["bomjara1", "bomjaro2"]))
        );
        assert_eq!(
            inline_command("хочу bomjara"),
            Some(InlineCommand::Media(&["bomjara1", "bomjaro2"]))
        );
    }

    #[test]
    fn media_commands_never_fall_back_to_text_hint() {
        for query in ["bomjara", "roflvideo", "kupyansk", "bom"] {
            let mapped = inline_command(query);
            assert!(
                matches!(mapped, Some(InlineCommand::Media(_))),
                "{query}: ожидалось Media, получено {mapped:?}"
            );
            assert!(
                !matches!(mapped, Some(InlineCommand::Text(_))),
                "{query}: подсказка текстом вместо файла"
            );
        }
    }

    #[test]
    fn inline_command_maps_text_and_channel_commands() {
        assert_eq!(
            inline_command("help"),
            Some(InlineCommand::Text(InlineText::CommandList))
        );
        assert_eq!(
            inline_command("start"),
            Some(InlineCommand::Text(InlineText::CommandList))
        );
        assert_eq!(
            inline_command("batekfetch"),
            Some(InlineCommand::Text(InlineText::CommandList))
        );
        assert_eq!(
            inline_command("grandmobile"),
            Some(InlineCommand::Text(InlineText::Grandmobile))
        );
        assert_eq!(
            inline_command("batek"),
            Some(InlineCommand::RandomVideo("batek"))
        );
        assert_eq!(
            inline_command("timur"),
            Some(InlineCommand::RandomVideo("timur"))
        );
        assert_eq!(
            inline_command("victorovich"),
            Some(InlineCommand::RandomVideo("victorovich"))
        );
        assert_eq!(inline_command("meme"), Some(InlineCommand::Meme));
        // Точное имя мема командой не считается: его ищет обычная выдача.
        assert_eq!(inline_command("bomjara1"), None);
        assert_eq!(inline_command("просто текст"), None);
    }

    #[test]
    fn keyword_requires_whole_words_including_cyrillic() {
        // Кириллица, регистр и знаки препинания вокруг фразы.
        assert!(contains_keyword("А где белые розы?", "белые розы"));
        assert!(contains_keyword("скинь БЕЛЫЕ РОЗЫ!", "белые розы"));
        assert!(contains_keyword("гранд мобайл это игра", "гранд мобайл"));
        assert!(contains_keyword("GrandMobile", "grandmobile"));
        assert!(contains_keyword("(бомжара)", "бомжара"));

        // Граница слова: внутри другого слова фраза не совпадает.
        assert!(!contains_keyword("неграндмобайл", "грандмобайл"));
        assert!(!contains_keyword("мегагранд мобайл", "гранд мобайл"));
        assert!(!contains_keyword("белые розык", "белые розы"));
        assert!(!contains_keyword("набомжара", "бомжара"));

        // Пустая фраза не должна совпадать со всем подряд.
        assert!(!contains_keyword("что угодно", ""));
    }

    fn inline_meme(name: &str) -> Meme {
        Meme {
            name: name.to_string(),
            path: PathBuf::from(format!("video/{name}.mp4")),
            kind: MemeKind::Video,
        }
    }

    #[test]
    fn inline_upload_guard_blocks_duplicate_and_allows_other() {
        let mut in_flight = HashSet::new();

        let queued = queue_pending_uploads(
            vec![inline_meme("bomjara1"), inline_meme("makuos")],
            &mut in_flight,
        );
        assert_eq!(
            queued.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["bomjara1", "makuos"]
        );
        assert_eq!(
            in_flight,
            HashSet::from(["bomjara1".to_string(), "makuos".to_string()])
        );

        // Повторный запрос того же мема не ставит вторую загрузку в очередь.
        let repeat = queue_pending_uploads(vec![inline_meme("bomjara1")], &mut in_flight);
        assert!(
            repeat.is_empty(),
            "дубль загрузки по тому же мему: {repeat:?}"
        );

        // Другой мем бронируется как обычно.
        let other = queue_pending_uploads(vec![inline_meme("shefos")], &mut in_flight);
        assert_eq!(
            other.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["shefos"]
        );
    }

    #[test]
    fn inline_upload_guard_caps_budget_per_query() {
        let mut in_flight = HashSet::new();
        let memes = ["a", "b", "c", "d"]
            .iter()
            .map(|n| inline_meme(n))
            .collect();
        let queued = queue_pending_uploads(memes, &mut in_flight);

        assert_eq!(queued.len(), INLINE_LAZY_UPLOADS);
        assert_eq!(in_flight.len(), INLINE_LAZY_UPLOADS);
    }

    #[test]
    fn inline_upload_guard_released_after_attempt_allows_retry() {
        let mut in_flight = HashSet::new();
        let queued = queue_pending_uploads(vec![inline_meme("bomjara1")], &mut in_flight);
        assert_eq!(queued.len(), 1);

        // Фоновая задача снимает бронь той же операцией remove после
        // попытки; после снятия тот же мем снова бронируется.
        in_flight.remove("bomjara1");
        let retry = queue_pending_uploads(vec![inline_meme("bomjara1")], &mut in_flight);
        assert_eq!(retry.len(), 1);
    }
}

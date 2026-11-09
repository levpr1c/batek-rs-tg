//! Панель управления в самом боте: мемы, ключевые слова, модераторы.
//!
//! Панель это сообщение с инлайн-кнопками и, где нужен текст, короткое
//! состояние ожидания ввода. Состояние живёт в памяти: у панели нет причин
//! переживать перезапуск, а незавершённый ввод после рестарта не должен
//! применяться к уже другой таблице.
//!
//! `callback_data` не содержит имён файлов и кодов приглашений: её видит
//! клиент нажавшего, поэтому там только номера страниц и индексы. Значения
//! до 64 байт держит тест `callback_data_fits_telegram_limit`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{FileId, InlineKeyboardButton, InlineKeyboardMarkup, Message, Recipient};
use tracing::debug;

use crate::access::{now_unix, Access, Capability, Role};
use crate::keywords::KeywordTable;
use crate::memes::{meme_key, MemeKind, Memes};
use crate::telegram::TgData;
use crate::uploads;

type TgResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Сколько мемов на одной странице списка. Восемь строк по одной кнопке
/// плюс навигация читаются на телефоне без прокрутки.
pub const PAGE_SIZE: usize = 8;

/// Сколько фраз показывать в карточке. Больше двадцати кнопок удаления
/// превращают карточку в кашу, а редкий мем с длинным хвостом всё равно
/// правится через список слов.
pub const CARD_PHRASE_LIMIT: usize = 20;

/// Сколько кнопок удаления в одном ряду.
const DELETE_PER_ROW: usize = 5;

/// Предел длины текста кнопки: длинные имена файлов ломают вёрстку клавиатуры.
const BUTTON_NAME_MAX: usize = 28;

/// Ожидание ввода живёт пять минут: дольше админ забудет, что бот чего-то
/// ждёт, и следующее сообщение в чат уйдёт не туда.
pub const PENDING_TTL: Duration = Duration::from_secs(5 * 60);

/// Попытки ввода кода приглашения: окно и предел. Код это восемь символов
/// из тридцати двух, перебор по одному сообщению должен упираться в предел.
const REDEEM_WINDOW: Duration = Duration::from_secs(10 * 60);
const REDEEM_ATTEMPTS: u32 = 10;

/// Что панель ждёт от пользователя следующим сообщением.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pending {
    /// Фраза для мема с таким именем.
    AddKeyword { meme: String },
    /// Числовой id или @username нового модератора.
    AddModerator,
    /// Ждём файл: видео, фото, аудио или документ из списка расширений.
    AwaitFile,
    /// Файл получен, ждём имя без расширения.
    AwaitName {
        file_id: String,
        kind: MemeKind,
        ext: String,
    },
}

struct PendingEntry {
    who: i64,
    at: Instant,
    action: Pending,
}

/// Состояния ввода и счётчик попыток. Ключ ожидания это чат: два админа в
/// одном чате не должны перепутать ввод, поэтому хранится ещё и автор.
#[derive(Default)]
pub struct Panels {
    pending: RwLock<HashMap<i64, PendingEntry>>,
    redeem: RwLock<HashMap<i64, (u32, Instant)>>,
}

impl Panels {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, chat: i64, who: i64, action: Pending) {
        let mut pending = self.pending.write().unwrap();
        pending.insert(
            chat,
            PendingEntry {
                who,
                at: Instant::now(),
                action,
            },
        );
    }

    /// Забирает ожидание, если оно не просрочено и принадлежит этому
    /// пользователю. Чужой ввод не снимает ожидание админа.
    pub fn take(&self, chat: i64, who: i64) -> Option<Pending> {
        let mut pending = self.pending.write().unwrap();
        let entry = pending.get(&chat)?;
        if entry.who != who {
            return None;
        }
        if entry.at.elapsed() > PENDING_TTL {
            pending.remove(&chat);
            return None;
        }
        pending.remove(&chat).map(|entry| entry.action)
    }

    /// Снимает ожидание, только если оно принадлежит этому пользователю:
    /// чужое сообщение в чате не отменяет незаконченный ввод админа.
    pub fn cancel(&self, chat: i64, who: i64) {
        let mut pending = self.pending.write().unwrap();
        if pending.get(&chat).is_some_and(|entry| entry.who == who) {
            pending.remove(&chat);
        }
    }

    /// Разрешает попытку ввода кода. Окно скользящее: после истечения
    /// счётчик начинается заново.
    pub fn allow_redeem(&self, user: i64) -> bool {
        let mut attempts = self.redeem.write().unwrap();
        let entry = attempts.entry(user).or_insert((0, Instant::now()));
        if entry.1.elapsed() > REDEEM_WINDOW {
            *entry = (0, Instant::now());
        }
        if entry.0 >= REDEEM_ATTEMPTS {
            return false;
        }
        entry.0 += 1;
        true
    }
}

/// Действие кнопки панели. Индексы указывают на текущий снимок каталога и
/// таблицы; перед применением они проверяются на границы.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PanelAction {
    Home,
    List(usize),
    Card(usize),
    Add(usize),
    Delete(usize, usize),
    Mods,
    ModAdd,
    ModDel(i64),
    Invite,
    Upload,
    Close,
}

impl PanelAction {
    pub fn encode(self) -> String {
        match self {
            Self::Home => "ad:h".to_string(),
            Self::List(page) => format!("ad:l:{page}"),
            Self::Card(index) => format!("ad:c:{index}"),
            Self::Add(index) => format!("ad:+:{index}"),
            Self::Delete(index, phrase) => format!("ad:-:{index}:{phrase}"),
            Self::Mods => "ad:m".to_string(),
            Self::ModAdd => "ad:ma".to_string(),
            Self::ModDel(id) => format!("ad:md:{id}"),
            Self::Invite => "ad:i".to_string(),
            Self::Upload => "ad:u".to_string(),
            Self::Close => "ad:x".to_string(),
        }
    }

    pub fn parse(data: &str) -> Option<Self> {
        let mut parts = data.split(':');
        if parts.next()? != "ad" {
            return None;
        }
        let kind = parts.next()?;
        let action = match kind {
            "h" => Self::Home,
            "l" => Self::List(parts.next()?.parse().ok()?),
            "c" => Self::Card(parts.next()?.parse().ok()?),
            "+" => Self::Add(parts.next()?.parse().ok()?),
            "-" => Self::Delete(parts.next()?.parse().ok()?, parts.next()?.parse().ok()?),
            "m" => Self::Mods,
            "ma" => Self::ModAdd,
            "md" => Self::ModDel(parts.next()?.parse().ok()?),
            "i" => Self::Invite,
            "u" => Self::Upload,
            "x" => Self::Close,
            _ => return None,
        };
        // Лишние куски это не наш формат: лучше промолчать, чем угадывать.
        parts.next().is_none().then_some(action)
    }

    fn capability(self) -> Capability {
        match self {
            Self::Mods | Self::ModAdd | Self::ModDel(_) | Self::Invite => Capability::ManageAccess,
            Self::Upload => Capability::UploadFiles,
            _ => Capability::ManageKeywords,
        }
    }
}

pub fn is_panel_callback(data: &str) -> bool {
    data.starts_with("ad:")
}

/// Открывает панель по команде `/admin`. Чужому молчит, отключённая панель
/// отвечает одной строкой: молчание без объяснения выглядит как поломка.
pub async fn open(bot: &Bot, msg: &Message, data: &TgData) -> TgResult {
    let chat = msg.chat.id;
    let user = msg.from.as_ref().map(|user| user.id.0 as i64);
    if !data.access.is_enabled() {
        bot.send_message(chat, "Панель выключена: TG_ADMIN_USER_ID не задан.")
            .await?;
        return Ok(());
    }
    let Some(role) = user.and_then(|user| data.access.role(user)) else {
        debug!("admin: панель отклонена для {user:?}");
        return Ok(());
    };

    let table = data.keywords.read().await;
    let memes = data.memes().await;
    let (text, keyboard) = home_view(role, &data.access, &table, &memes);
    drop(table);
    bot.send_message(chat, text).reply_markup(keyboard).await?;
    Ok(())
}

/// Активация приглашения. Только личный чат: код в группе остался бы в
/// истории переписки, а приглашение одноразовое и не должно утечь.
pub async fn redeem(bot: &Bot, msg: &Message, data: &TgData, code: &str) -> TgResult {
    let chat = msg.chat.id;
    if !msg.chat.is_private() {
        bot.send_message(
            chat,
            "Приглашение активируется только в личном чате с ботом.",
        )
        .await?;
        return Ok(());
    }
    if !data.access.is_enabled() {
        bot.send_message(chat, "Панель выключена: TG_ADMIN_USER_ID не задан.")
            .await?;
        return Ok(());
    }
    let Some(user) = msg.from.as_ref().map(|user| user.id.0 as i64) else {
        return Ok(());
    };
    if !data.panels.allow_redeem(user) {
        bot.send_message(chat, "Слишком много попыток. Попробуй позже.")
            .await?;
        return Ok(());
    }

    match data.access.redeem(code, user, now_unix()) {
        Ok(()) => {
            bot.send_message(chat, "Готово. Теперь у тебя есть доступ модератора.")
                .await?;
        }
        Err(crate::access::RedeemError::Expired) => {
            bot.send_message(chat, "Код истёк. Попроси новый.").await?;
        }
        Err(crate::access::RedeemError::NotFound) => {
            bot.send_message(chat, "Код не найден или уже использован.")
                .await?;
        }
        Err(crate::access::RedeemError::SaveFailed) => {
            bot.send_message(chat, "Не удалось сохранить права.")
                .await?;
        }
    }
    Ok(())
}

/// Обрабатывает нажатие кнопки панели. Возвращает `false`, если данные не
/// наши: тогда вызывающий пробует свои обработчики.
pub async fn on_callback(bot: &Bot, query: &CallbackQuery, data: &TgData) -> bool {
    let Some(payload) = query.data.as_deref() else {
        return false;
    };
    let Some(action) = PanelAction::parse(payload) else {
        return false;
    };
    let user = query.from.id.0 as i64;

    let Some(role) = data.access.role(user) else {
        // Чужому не показываем даже текст ошибки: он подтвердил бы, что
        // панель существует.
        let _ = bot.answer_callback_query(query.id.clone()).await;
        return true;
    };
    if !role.can(action.capability()) {
        let _ = bot
            .answer_callback_query(query.id.clone())
            .text("это может только админ")
            .show_alert(true)
            .await;
        return true;
    }

    let Some(message) = query.regular_message() else {
        let _ = bot.answer_callback_query(query.id.clone()).await;
        return true;
    };
    let chat = message.chat.id;
    let message_id = message.id;

    match action {
        PanelAction::Upload => {
            data.panels.set(chat.0, user, Pending::AwaitFile);
            let _ = bot.answer_callback_query(query.id.clone()).await;
            bot.send_message(
                chat,
                "Пришли файл одним сообщением: видео, фото, аудио или документ.\n\
                 Пределы Bot API: фото до 10 МБ, остальное до 20 МБ.\n\
                 /cancel отменяет ввод.",
            )
            .await
            .ok();
            return true;
        }
        PanelAction::Close => {
            let _ = bot.answer_callback_query(query.id.clone()).await;
            let _ = bot.delete_message(chat, message_id).await;
            return true;
        }
        PanelAction::Add(index) => {
            let memes = data.memes().await;
            let Some(meme) = memes.items().get(index) else {
                let _ = bot.answer_callback_query(query.id.clone()).await;
                return true;
            };
            data.panels.set(
                chat.0,
                user,
                Pending::AddKeyword {
                    meme: meme.name.clone(),
                },
            );
            let _ = bot.answer_callback_query(query.id.clone()).await;
            bot.send_message(
                chat,
                append_hint(format!(
                    "Фраза для мема '{}'.\nПришли одним сообщением. Любая команда отменит ввод.",
                    display_name(&meme.name)
                )),
            )
            .await
            .ok();
            return true;
        }
        PanelAction::Delete(index, phrase_index) => {
            let phrase = {
                let table = data.keywords.read().await;
                data.memes().await.items().get(index).and_then(|meme| {
                    table
                        .phrase_sources_for_name(&meme.name)
                        .get(phrase_index)
                        .map(|(phrase, _, _)| phrase.to_string())
                })
            };
            let Some(phrase) = phrase else {
                let _ = bot.answer_callback_query(query.id.clone()).await;
                return true;
            };
            let result = data.keyword_store.lock().await.remove(&phrase);
            match result {
                Ok(table) => {
                    *data.keywords.write().await = table;
                    let _ = bot
                        .answer_callback_query(query.id.clone())
                        .text(format!("Удалено: {phrase}"))
                        .await;
                }
                Err(e) => {
                    let _ = bot
                        .answer_callback_query(query.id.clone())
                        .text(e)
                        .show_alert(true)
                        .await;
                }
            }
        }
        PanelAction::ModAdd => {
            data.panels.set(chat.0, user, Pending::AddModerator);
            let _ = bot.answer_callback_query(query.id.clone()).await;
            bot.send_message(
                chat,
                "Пришли числовой id или @username нового модератора.\n\
                 Любая команда отменит ввод.",
            )
            .await
            .ok();
            return true;
        }
        PanelAction::ModDel(target) => {
            let note = match data.access.remove_moderator(target) {
                Ok(()) => format!("Модератор {target} снят."),
                Err(e) => e,
            };
            let _ = bot.answer_callback_query(query.id.clone()).text(note).await;
        }
        PanelAction::Invite => {
            // Код в истории группы остался бы навсегда, а приглашение
            // одноразовое и не должно утечь в переписку.
            if !message.chat.is_private() {
                let _ = bot
                    .answer_callback_query(query.id.clone())
                    .text("Приглашение выдаётся только в личном чате.")
                    .show_alert(true)
                    .await;
                return true;
            }
            let code = match data.access.create_invite(user, now_unix()) {
                Ok(code) => code,
                Err(e) => {
                    let _ = bot
                        .answer_callback_query(query.id.clone())
                        .text(e)
                        .show_alert(true)
                        .await;
                    return true;
                }
            };
            let _ = bot.answer_callback_query(query.id.clone()).await;
            bot.send_message(
                chat,
                append_hint(format!(
                    "Одноразовый код: {code}\nЖивёт 15 минут, активируется один раз \
                     командой /mod {code} в личном чате."
                )),
            )
            .await
            .ok();
            return true;
        }
        _ => {}
    }

    // Остальные действия только перерисовывают текущее сообщение.
    let (text, keyboard) = view(action, &data.access, role, data).await;
    if bot
        .edit_message_text(chat, message_id, text)
        .reply_markup(keyboard)
        .await
        .is_err()
    {
        // Сообщение могли удалить или оно старше 48 часов: тогда новый
        // экран лучше, чем молчание.
        let (text, keyboard) = view(action, &data.access, role, data).await;
        let _ = bot.send_message(chat, text).reply_markup(keyboard).await;
    }
    let _ = bot.answer_callback_query(query.id.clone()).await;
    true
}

/// Применяет ввод, которого ждала панель.
pub async fn handle_input(bot: &Bot, msg: &Message, data: &TgData, pending: Pending) -> TgResult {
    let chat = msg.chat.id;
    let text = msg.text();
    let user = msg.from.as_ref().map(|user| user.id.0 as i64);

    let chat_id = chat.0;
    match pending {
        Pending::AwaitFile => {
            let Some(user) = user else { return Ok(()) };
            if !data
                .access
                .role(user)
                .is_some_and(|role| role.can(Capability::UploadFiles))
            {
                bot.send_message(chat, "Загрузка файлов доступна только админу.")
                    .await?;
                return Ok(());
            }
            match uploads::pick(msg) {
                Ok(Some(upload)) => {
                    let stem = upload
                        .stem
                        .clone()
                        .and_then(|stem| uploads::validate_stem(&stem).ok());
                    match stem {
                        Some(stem) => {
                            save_upload(bot, msg, data, user, &upload, &stem).await?;
                        }
                        None => {
                            data.panels.set(
                                chat_id,
                                user,
                                Pending::AwaitName {
                                    file_id: upload.file_id,
                                    kind: upload.kind,
                                    ext: upload.ext,
                                },
                            );
                            bot.send_message(
                                chat,
                                "Пришли имя файла без расширения: оно станет именем мема.\n\
                                 /cancel отменяет ввод.",
                            )
                            .await?;
                        }
                    }
                }
                Ok(None) => {
                    data.panels.set(chat_id, user, Pending::AwaitFile);
                    bot.send_message(
                        chat,
                        "Жду файл: видео, фото, аудио или документ. /cancel отменяет ввод.",
                    )
                    .await?;
                }
                Err(e) => {
                    data.panels.set(chat_id, user, Pending::AwaitFile);
                    bot.send_message(chat, format!("{e}\nПришли другой файл."))
                        .await?;
                }
            }
        }
        Pending::AwaitName { file_id, kind, ext } => {
            let Some(user) = user else { return Ok(()) };
            let Some(text) = text else {
                data.panels.set(
                    chat_id,
                    user,
                    Pending::AwaitName {
                        file_id: file_id.clone(),
                        kind,
                        ext: ext.clone(),
                    },
                );
                bot.send_message(chat, "Жду имя текстом. /cancel отменяет ввод.")
                    .await?;
                return Ok(());
            };
            if !data
                .access
                .role(user)
                .is_some_and(|role| role.can(Capability::UploadFiles))
            {
                bot.send_message(chat, "Загрузка файлов доступна только админу.")
                    .await?;
                return Ok(());
            }
            match uploads::validate_stem(text) {
                Ok(stem) => {
                    let upload = uploads::Upload {
                        file_id,
                        kind,
                        stem: None,
                        ext,
                    };
                    save_upload(bot, msg, data, user, &upload, &stem).await?;
                }
                Err(e) => {
                    data.panels
                        .set(chat_id, user, Pending::AwaitName { file_id, kind, ext });
                    bot.send_message(chat, format!("{e}\nПришли другое имя."))
                        .await?;
                }
            }
        }
        Pending::AddKeyword { meme } => {
            let Some(user) = user else { return Ok(()) };
            let Some(text) = text else {
                // Сообщение без текста не должно молча терять ожидание.
                data.panels
                    .set(chat_id, user, Pending::AddKeyword { meme: meme.clone() });
                bot.send_message(chat, "Жду фразу текстом. /cancel отменяет ввод.")
                    .await?;
                return Ok(());
            };
            if !data
                .access
                .role(user)
                .is_some_and(|role| role.can(Capability::ManageKeywords))
            {
                bot.send_message(chat, "Прав нет.").await?;
                return Ok(());
            }
            let result = data.keyword_store.lock().await.add(text, &meme);
            match result {
                Ok(table) => {
                    let ready = {
                        let sources = table.phrase_sources_for_name(&meme);
                        crate::keywords::alphabet_pair_ok(
                            sources.iter().map(|(phrase, _, _)| *phrase),
                        )
                    };
                    *data.keywords.write().await = table;
                    let note = if ready {
                        format!("Добавлено к '{}'.", display_name(&meme))
                    } else {
                        format!(
                            "Добавлено к '{}', но фраза не сработает: у мема нет \
                             фразы на втором алфавите. Добавь кириллицу и латиницу.",
                            display_name(&meme)
                        )
                    };
                    bot.send_message(chat, note).await?;
                }
                Err(e) => {
                    bot.send_message(chat, e).await?;
                    return Ok(());
                }
            }
            // Сразу показываем карточку: видно, что фраза рядом с остальными.
            let memes = data.memes().await;
            if let Some(index) = memes.items().iter().position(|item| item.name == meme) {
                let card = {
                    let table = data.keywords.read().await;
                    card_view(&memes, &table, index)
                };
                if let Some((text, keyboard)) = card {
                    let _ = bot.send_message(chat, text).reply_markup(keyboard).await;
                }
            }
        }
        Pending::AddModerator => {
            let Some(user) = user else { return Ok(()) };
            let Some(text) = text else {
                data.panels.set(chat_id, user, Pending::AddModerator);
                bot.send_message(chat, "Жду id или @username текстом. /cancel отменяет ввод.")
                    .await?;
                return Ok(());
            };
            if !data
                .access
                .role(user)
                .is_some_and(|role| role.can(Capability::ManageAccess))
            {
                bot.send_message(chat, "Прав нет.").await?;
                return Ok(());
            }
            let Some(target) = resolve_target(bot, text).await else {
                bot.send_message(
                    chat,
                    "Не понял id. Нужен числовой id или @username с публичным именем.",
                )
                .await?;
                return Ok(());
            };
            if target <= 0 {
                bot.send_message(
                    chat,
                    "id должен быть положительным: это пользователь, а не чат.",
                )
                .await?;
                return Ok(());
            }
            if data.access.admin() == Some(target) {
                bot.send_message(chat, "Это уже админ.").await?;
                return Ok(());
            }
            match data.access.add_moderator(target) {
                Ok(()) => {
                    bot.send_message(chat, format!("Модератор {target} добавлен."))
                        .await?;
                }
                Err(e) => {
                    bot.send_message(chat, e).await?;
                }
            }
        }
    }

    Ok(())
}

/// Точка входа для сообщения, которого ждёт панель. Возвращает `true`,
/// если сообщение поглощено вводом. Команда отменяет ожидание и проходит
/// дальше обычным путём.
pub async fn take_pending_message(bot: &Bot, msg: &Message, data: &TgData) -> bool {
    let Some(sender) = msg.from.as_ref().map(|user| user.id.0 as i64) else {
        return false;
    };
    let chat = msg.chat.id.0;

    if msg
        .text()
        .is_some_and(|text| text.trim_start().starts_with('/'))
    {
        data.panels.cancel(chat, sender);
        return false;
    }

    let Some(pending) = data.panels.take(chat, sender) else {
        return false;
    };
    if let Err(e) = handle_input(bot, msg, data, pending).await {
        debug!("admin: ввод не обработан: {e}");
    }
    true
}

/// Скачивает файл в каталог мемов и пересобирает каталог. Имя проверено
/// раньше; здесь остаются пределы Bot API и атомарная запись: недокачанный
/// файл не должен появиться в каталоге под рабочим именем.
async fn save_upload(
    bot: &Bot,
    msg: &Message,
    data: &TgData,
    sender: i64,
    upload: &uploads::Upload,
    stem: &str,
) -> TgResult {
    let chat = msg.chat.id;
    let kind = upload.kind;
    let file_name = uploads::final_name(stem, &upload.ext);

    let key = meme_key(stem);
    if data.memes().await.keys().contains(&key) {
        // Ожидание имени возвращается: иначе «пришли другое имя» некуда
        // присылать и файл придётся начинать заново.
        data.panels.set(
            chat.0,
            sender,
            Pending::AwaitName {
                file_id: upload.file_id.clone(),
                kind,
                ext: upload.ext.clone(),
            },
        );
        bot.send_message(
            chat,
            format!("Имя '{stem}' уже занято мемом. Пришли другое имя."),
        )
        .await?;
        return Ok(());
    }

    let path = data.root.join(uploads::target_dir(kind)).join(&file_name);
    if path.exists() {
        data.panels.set(
            chat.0,
            sender,
            Pending::AwaitName {
                file_id: upload.file_id.clone(),
                kind,
                ext: upload.ext.clone(),
            },
        );
        bot.send_message(
            chat,
            format!("Файл '{file_name}' уже лежит в каталоге. Пришли другое имя."),
        )
        .await?;
        return Ok(());
    }

    let file = match bot.get_file(FileId(upload.file_id.clone())).await {
        Ok(file) => file,
        Err(e) => {
            // Ошибка Bot API не должна раскрывать токен: Display у ошибки
            // teloxide его не содержит.
            data.panels.set(chat.0, sender, Pending::AwaitFile);
            bot.send_message(
                chat,
                format!("Не удалось получить файл: {e}\nПришли файл заново."),
            )
            .await?;
            return Ok(());
        }
    };

    if u64::from(file.meta.size) > uploads::max_bytes(kind) {
        data.panels.set(chat.0, sender, Pending::AwaitFile);
        bot.send_message(
            chat,
            format!("{}\nПришли файл поменьше.", uploads::limit_text(kind)),
        )
        .await?;
        return Ok(());
    }

    let tmp = path.with_file_name(format!(".{file_name}.tmp"));
    let written = async {
        let mut dst = tokio::fs::File::create(&tmp).await?;
        bot.download_file(&file.path, &mut dst)
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        dst.sync_all().await?;
        drop(dst);
        tokio::fs::rename(&tmp, &path).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;

    if let Err(e) = written {
        let _ = tokio::fs::remove_file(&tmp).await;
        data.panels.set(chat.0, sender, Pending::AwaitFile);
        bot.send_message(
            chat,
            format!("Не удалось сохранить файл: {e}\nПришли файл заново."),
        )
        .await?;
        return Ok(());
    }

    // Скан читает только обычные файлы и новые имена получает сразу:
    // загруженный мем доступен и командам, и ключевым словам без рестарта.
    *data.memes.write().await = Arc::new(Memes::scan(&data.root));
    data.panels.set(
        chat.0,
        sender,
        Pending::AddKeyword {
            meme: stem.to_string(),
        },
    );
    bot.send_message(
        chat,
        format!(
            "Сохранено: {file_name}.\nПришли первую фразу для мема. Нужны оба \
             алфавита: кириллица и латиница или транслит."
        ),
    )
    .await?;
    Ok(())
}

/// Числовой id или @username. Для username нужен `getChat`: Bot API
/// принимает имя только у пользователя с публичным username.
async fn resolve_target(bot: &Bot, text: &str) -> Option<i64> {
    let text = text.trim();
    if let Some(username) = text.strip_prefix('@') {
        let username = username.trim();
        if username.is_empty() {
            return None;
        }
        let chat = bot
            .get_chat(Recipient::ChannelUsername(format!("@{username}")))
            .await
            .ok()?;
        return Some(chat.id.0);
    }
    text.parse::<i64>().ok()
}

async fn view(
    action: PanelAction,
    access: &Access,
    role: Role,
    data: &TgData,
) -> (String, InlineKeyboardMarkup) {
    let memes = data.memes().await;
    let table = data.keywords.read().await;
    match action {
        PanelAction::Home | PanelAction::Close => home_view(role, access, &table, &memes),
        PanelAction::List(page) => list_view(&memes, &table, page),
        PanelAction::Card(index) => {
            card_view(&memes, &table, index).unwrap_or_else(|| list_view(&memes, &table, 0))
        }
        PanelAction::Mods | PanelAction::ModAdd | PanelAction::ModDel(_) => mods_view(access, role),
        PanelAction::Delete(index, _) => {
            card_view(&memes, &table, index).unwrap_or_else(|| list_view(&memes, &table, 0))
        }
        PanelAction::Invite | PanelAction::Upload => {
            // Код и загрузка показываются отдельными сообщениями, здесь
            // только безопасный возврат на главный экран.
            home_view(role, access, &table, &memes)
        }
        // Add уводит в ожидание ввода, здесь только безопасный возврат.
        PanelAction::Add(_) => list_view(&memes, &table, 0),
    }
}

pub fn home_view(
    role: Role,
    access: &Access,
    table: &KeywordTable,
    memes: &Memes,
) -> (String, InlineKeyboardMarkup) {
    let role_name = match role {
        Role::Admin => "админ",
        Role::Moderator => "модератор",
    };
    let mut text = format!(
        "Панель управления.\nРоль: {role_name}.\nМемов: {}.\nФраз в таблице: {}.",
        memes.len(),
        table.len()
    );
    if role == Role::Admin {
        text.push_str(&format!("\nМодераторов: {}.", access.moderators().len()));
    }

    let mut rows = vec![vec![InlineKeyboardButton::callback(
        "Мемы",
        PanelAction::List(0).encode(),
    )]];
    if role == Role::Admin {
        rows.push(vec![InlineKeyboardButton::callback(
            "Загрузить файл",
            PanelAction::Upload.encode(),
        )]);
        rows.push(vec![
            InlineKeyboardButton::callback("Модераторы", PanelAction::Mods.encode()),
            InlineKeyboardButton::callback("Приглашение", PanelAction::Invite.encode()),
        ]);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "Закрыть",
        PanelAction::Close.encode(),
    )]);
    (text, InlineKeyboardMarkup::new(rows))
}

/// Границы страницы. Хвост не раздувается до полной страницы: пустые
/// страницы не существуют.
pub fn page_range(len: usize, page: usize) -> (usize, usize) {
    let start = page.saturating_mul(PAGE_SIZE).min(len);
    let end = start.saturating_add(PAGE_SIZE).min(len);
    (start, end)
}

pub fn page_count(len: usize) -> usize {
    len.div_ceil(PAGE_SIZE).max(1)
}

fn list_view(memes: &Memes, table: &KeywordTable, page: usize) -> (String, InlineKeyboardMarkup) {
    let total = page_count(memes.len());
    let page = page.min(total - 1);
    let (start, end) = page_range(memes.len(), page);

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for (index, meme) in memes.items().iter().enumerate().take(end).skip(start) {
        let count = table.phrases_for_name(&meme.name).len();
        rows.push(vec![InlineKeyboardButton::callback(
            format!("{} ({count} слов)", truncate_button(&meme.name)),
            PanelAction::Card(index).encode(),
        )]);
    }
    if memes.items().is_empty() {
        rows.push(vec![InlineKeyboardButton::callback(
            "Мемов нет",
            PanelAction::Home.encode(),
        )]);
    }

    let mut nav = Vec::new();
    if page > 0 {
        nav.push(InlineKeyboardButton::callback(
            "←",
            PanelAction::List(page - 1).encode(),
        ));
    }
    nav.push(InlineKeyboardButton::callback(
        format!("{}/{}", page + 1, total),
        PanelAction::Home.encode(),
    ));
    if page + 1 < total {
        nav.push(InlineKeyboardButton::callback(
            "→",
            PanelAction::List(page + 1).encode(),
        ));
    }
    rows.push(nav);
    rows.push(vec![InlineKeyboardButton::callback(
        "Назад",
        PanelAction::Home.encode(),
    )]);

    let text = format!(
        "Мемы, {} всего. Нажми мем, чтобы увидеть его слова.",
        memes.len()
    );
    (text, InlineKeyboardMarkup::new(rows))
}

pub fn card_view(
    memes: &Memes,
    table: &KeywordTable,
    index: usize,
) -> Option<(String, InlineKeyboardMarkup)> {
    let meme = memes.items().get(index)?;
    let sources = table.phrase_sources_for_name(&meme.name);

    let mut text = format!(
        "Мем: {}\nТип: {}\nФраз: {}",
        display_name(&meme.name),
        kind_name(meme.kind),
        sources.len()
    );
    if sources.is_empty() {
        text.push_str("\n\nСлов пока нет: добавь первое.");
    } else {
        text.push('\n');
        for (position, (phrase, builtin, pending)) in
            sources.iter().take(CARD_PHRASE_LIMIT).enumerate()
        {
            let mark = if *builtin {
                "встроенное"
            } else {
                "добавленное"
            };
            let mark = if *pending {
                format!("{mark}, ждёт пары")
            } else {
                mark.to_string()
            };
            text.push_str(&format!(
                "\n{}. {} ({mark})",
                position + 1,
                display_name(phrase)
            ));
        }
        if sources.len() > CARD_PHRASE_LIMIT {
            text.push_str(&format!(
                "\n... и ещё {}",
                sources.len() - CARD_PHRASE_LIMIT
            ));
        }
    }

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut row: Vec<InlineKeyboardButton> = Vec::new();
    for position in 0..sources.len().min(CARD_PHRASE_LIMIT) {
        row.push(InlineKeyboardButton::callback(
            format!("✕{}", position + 1),
            PanelAction::Delete(index, position).encode(),
        ));
        if row.len() == DELETE_PER_ROW {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "＋ слово",
        PanelAction::Add(index).encode(),
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        "← К списку",
        PanelAction::List(index / PAGE_SIZE).encode(),
    )]);

    Some((text, InlineKeyboardMarkup::new(rows)))
}

fn mods_view(access: &Access, role: Role) -> (String, InlineKeyboardMarkup) {
    if role != Role::Admin {
        return (
            "Список модераторов видит только админ.".to_string(),
            InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
                "Назад",
                PanelAction::Home.encode(),
            )]]),
        );
    }

    let moderators = access.moderators();
    let mut text = match access.admin() {
        Some(admin) => format!("Админ: {admin}.\nМодераторов: {}.", moderators.len()),
        None => "Админ не задан.".to_string(),
    };
    if moderators.is_empty() {
        text.push_str("\n\nСписок пуст.");
    }

    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    for id in &moderators {
        rows.push(vec![InlineKeyboardButton::callback(
            format!("✕ {id}"),
            PanelAction::ModDel(*id).encode(),
        )]);
    }
    rows.push(vec![InlineKeyboardButton::callback(
        "＋ модератор",
        PanelAction::ModAdd.encode(),
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        "Назад",
        PanelAction::Home.encode(),
    )]);
    (text, InlineKeyboardMarkup::new(rows))
}

fn kind_name(kind: MemeKind) -> &'static str {
    match kind {
        MemeKind::Video => "видео",
        MemeKind::Image => "фото",
        MemeKind::Audio => "аудио",
    }
}

/// Имя для показа: управляющие символы и переводы строк заменяются, чтобы
/// файл с именем вида `a\nb` не ломал карточку.
fn display_name(name: &str) -> String {
    name.chars()
        .map(|ch| if ch.is_control() { '?' } else { ch })
        .take(64)
        .collect()
}

fn truncate_button(name: &str) -> String {
    let shown = display_name(name);
    if shown.chars().count() <= BUTTON_NAME_MAX {
        return shown;
    }
    let mut out: String = shown.chars().take(BUTTON_NAME_MAX - 1).collect();
    out.push('…');
    out
}

/// Подсказка про отмену ввода. Держится отдельно, чтобы формулировка была
/// одна на все экраны.
fn append_hint(mut text: String) -> String {
    text.push_str("\n\n/cancel отменяет ввод.");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-admin-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("video")).unwrap();
        std::fs::create_dir_all(dir.join("fotos")).unwrap();
        std::fs::create_dir_all(dir.join("music")).unwrap();
        dir
    }

    #[test]
    fn callback_data_fits_telegram_limit() {
        let actions = [
            PanelAction::Home,
            PanelAction::List(usize::MAX),
            PanelAction::Card(usize::MAX),
            PanelAction::Add(usize::MAX),
            PanelAction::Delete(usize::MAX, usize::MAX),
            PanelAction::Mods,
            PanelAction::ModAdd,
            PanelAction::ModDel(i64::MIN),
            PanelAction::Invite,
            PanelAction::Upload,
            PanelAction::Close,
        ];
        for action in actions {
            let encoded = action.encode();
            assert!(
                encoded.len() <= 64,
                "{action:?}: {} байт в callback_data",
                encoded.len()
            );
        }
    }

    #[test]
    fn callback_round_trip_and_junk() {
        let actions = [
            PanelAction::Home,
            PanelAction::List(3),
            PanelAction::Card(12),
            PanelAction::Add(4),
            PanelAction::Delete(7, 1),
            PanelAction::Mods,
            PanelAction::ModAdd,
            PanelAction::ModDel(123456789),
            PanelAction::Invite,
            PanelAction::Upload,
            PanelAction::Close,
        ];
        for action in actions {
            assert_eq!(PanelAction::parse(&action.encode()), Some(action));
        }

        for junk in [
            "",
            "ad",
            "ad:",
            "ad:l",
            "ad:l:x",
            "ad:c:1:2",
            "ad:-:1",
            "ad:-:1:2:3",
            "v:42:t:s",
            "ad:zz",
        ] {
            assert_eq!(PanelAction::parse(junk), None, "{junk:?}");
        }
        assert!(is_panel_callback("ad:h"));
        assert!(!is_panel_callback("v:1:b:s"));
    }

    #[test]
    fn only_admin_gets_access_actions() {
        assert_eq!(PanelAction::Mods.capability(), Capability::ManageAccess);
        assert_eq!(PanelAction::Invite.capability(), Capability::ManageAccess);
        assert_eq!(
            PanelAction::ModDel(1).capability(),
            Capability::ManageAccess
        );
        assert_eq!(
            PanelAction::Card(0).capability(),
            Capability::ManageKeywords
        );
        assert_eq!(PanelAction::Add(0).capability(), Capability::ManageKeywords);
    }

    #[test]
    fn page_range_does_not_overflow_or_leave_empty_tail() {
        assert_eq!(page_range(0, 0), (0, 0));
        assert_eq!(page_range(5, 0), (0, 5));
        assert_eq!(page_range(20, 0), (0, 8));
        assert_eq!(page_range(20, 2), (16, 20));
        assert_eq!(page_range(20, 99), (20, 20));
        assert_eq!(page_count(0), 1);
        assert_eq!(page_count(8), 1);
        assert_eq!(page_count(9), 2);
        assert_eq!(page_count(20), 3);
    }

    #[test]
    fn card_lists_phrases_and_marks_source() {
        let root = temp_root("card");
        std::fs::write(root.join("video/bomjara1.mp4"), b"v").unwrap();
        let memes = Memes::scan(&root);
        let table = KeywordTable::build(&Default::default());
        let index = memes
            .items()
            .iter()
            .position(|meme| meme.name == "bomjara1")
            .unwrap();

        let (text, keyboard) = card_view(&memes, &table, index).unwrap();
        assert!(text.contains("bomjara1"), "{text}");
        assert!(text.contains("встроенное"), "{text}");
        assert!(
            keyboard
                .inline_keyboard
                .iter()
                .flatten()
                .any(|button| { button.text.contains("слово") }),
            "нет кнопки добавления"
        );
        assert_eq!(card_view(&memes, &table, 999), None);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn button_names_are_short_and_control_free() {
        assert_eq!(truncate_button("bomjara1"), "bomjara1");
        assert_eq!(display_name("a\nb"), "a?b");
        let long = "я".repeat(100);
        let shown = truncate_button(&long);
        assert_eq!(shown.chars().count(), BUTTON_NAME_MAX);
        assert!(shown.ends_with('…'));
    }

    #[test]
    fn only_admin_sees_upload_and_access_buttons() {
        let root = temp_root("home");
        let memes = Memes::scan(&root);
        let table = KeywordTable::build(&Default::default());
        let access = Access::with_state(
            Some(1),
            root.join("data/access.json"),
            crate::access::AccessFile::default(),
        );

        let labels = |role| {
            let (_, keyboard) = home_view(role, &access, &table, &memes);
            keyboard
                .inline_keyboard
                .iter()
                .flatten()
                .map(|button| button.text.clone())
                .collect::<Vec<_>>()
        };

        let admin = labels(Role::Admin);
        assert!(
            admin.iter().any(|text| text == "Загрузить файл"),
            "{admin:?}"
        );
        assert!(admin.iter().any(|text| text == "Модераторы"), "{admin:?}");
        assert!(admin.iter().any(|text| text == "Приглашение"), "{admin:?}");

        let moderator = labels(Role::Moderator);
        assert!(
            !moderator.iter().any(|text| text == "Модераторы"),
            "модератор видит выдачу прав: {moderator:?}"
        );
        assert!(!moderator.iter().any(|text| text == "Загрузить файл"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_that_appears_in_catalog_is_reachable_by_new_keyword() {
        // Путь загрузки без сети: файл лежит в video/, каталог пересобран,
        // фраза добавлена в файл правок, матчер ведёт к этому файлу.
        let root = temp_root("upload-flow");
        std::fs::write(root.join("video/кресло.mp4"), b"v").unwrap();

        let memes = Memes::scan(&root);
        assert!(
            memes.items().iter().any(|meme| meme.name == "кресло"),
            "файл не попал в каталог: {:?}",
            memes.names()
        );

        let path = root.join("data/keywords.json");
        let mut store = crate::keywords::KeywordStore::load(path.clone());
        store.add("кресло", "кресло").unwrap();
        let table = store.add("kreslo", "кресло").unwrap();

        let (_, action) = table.match_keyword("кресло").expect("фраза не сработала");
        let needle = match action {
            crate::keywords::KeywordAction::Meme(needle) => needle,
            other => panic!("ожидался Meme, получено {other:?}"),
        };
        let found = memes.find(needle.as_ref()).expect("мем не найден по игле");
        assert_eq!(found.name, "кресло");
        assert_eq!(found.kind, MemeKind::Video);
        assert!(found.path.starts_with(root.join("video")));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pending_is_scoped_to_user_and_expires() {
        let panels = Panels::new();
        panels.set(10, 1, Pending::AddModerator);

        // Чужой не может забрать ожидание.
        assert_eq!(panels.take(10, 2), None);
        assert_eq!(panels.take(10, 1), Some(Pending::AddModerator));
        // Забрали один раз: повторно пусто.
        assert_eq!(panels.take(10, 1), None);

        // Просроченное ожидание не применяется.
        {
            let mut pending = panels.pending.write().unwrap();
            pending.insert(
                11,
                PendingEntry {
                    who: 3,
                    at: Instant::now() - PENDING_TTL - Duration::from_secs(1),
                    action: Pending::AddModerator,
                },
            );
        }
        assert_eq!(panels.take(11, 3), None);
        assert!(panels.pending.read().unwrap().get(&11).is_none());
    }

    #[test]
    fn redeem_attempts_are_limited_per_user() {
        let panels = Panels::new();
        for _ in 0..REDEEM_ATTEMPTS {
            assert!(panels.allow_redeem(5));
        }
        assert!(!panels.allow_redeem(5));
        // Другой пользователь живёт независимо.
        assert!(panels.allow_redeem(6));
    }
}

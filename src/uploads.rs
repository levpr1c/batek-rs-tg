//! Новые файлы из панели: приём, проверка имени и запись в каталоги мемов.
//!
//! Bot API отдаёт ботам файлы не больше 20 МБ, поэтому предел проверяется до
//! скачивания, а не после. Фото дополнительно ограничены десятью мегабайтами
//! отправки: файл больше просто не удастся отправить обратно.
//!
//! Имя выбирает админ, потому что имя мема это его идентификатор в таблице
//! ключевых слов. Имя проверяется как имя файла, а не как произвольная
//! строка: `../` или разделитель пути увели бы запись из каталога мемов.

use std::path::Path;

use teloxide::types::Message;

use crate::memes::MemeKind;

/// Предел Bot API на скачивание файла ботом.
pub const DOWNLOAD_LIMIT: u64 = 20 * 1024 * 1024;

/// Предел отправки фото: больше Telegram не примет в `sendPhoto`.
pub const PHOTO_LIMIT: u64 = 10 * 1024 * 1024;

/// Предел длины имени мема. Длиннее не влезает в кнопки панели и в caption.
pub const NAME_MAX: usize = 64;

/// Присланный файл до сохранения.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub file_id: String,
    pub kind: MemeKind,
    /// Предложенное имя без расширения, если Telegram его сообщил.
    pub stem: Option<String>,
    /// Расширение без точки, в нижнем регистре.
    pub ext: String,
}

/// Что за файл прислали. `Ok(None)` значит, что медиа в сообщении нет и
/// панель должна ждать дальше.
pub fn pick(msg: &Message) -> Result<Option<Upload>, String> {
    if let Some(video) = msg.video() {
        return Ok(Some(Upload {
            file_id: video.file.id.0.clone(),
            kind: MemeKind::Video,
            stem: video.file_name.as_deref().and_then(stem_of),
            ext: extension_of(video.file_name.as_deref(), MemeKind::Video)?,
        }));
    }

    if let Some(photo) = msg.photo() {
        // Telegram отдаёт размеры по возрастанию: последний самый большой.
        let largest = photo.last().ok_or_else(|| "Пустое фото.".to_string())?;
        return Ok(Some(Upload {
            file_id: largest.file.id.0.clone(),
            kind: MemeKind::Image,
            stem: None,
            ext: "jpg".to_string(),
        }));
    }

    if let Some(audio) = msg.audio() {
        return Ok(Some(Upload {
            file_id: audio.file.id.0.clone(),
            kind: MemeKind::Audio,
            stem: audio.file_name.as_deref().and_then(stem_of),
            ext: extension_of(audio.file_name.as_deref(), MemeKind::Audio)?,
        }));
    }

    if let Some(document) = msg.document() {
        let file_name = document.file_name.as_deref().unwrap_or_default();
        let kind = MemeKind::from_path(Path::new(file_name))
            .ok_or_else(|| "Это не видео, не фото и не аудио из списка расширений.".to_string())?;
        return Ok(Some(Upload {
            file_id: document.file.id.0.clone(),
            kind,
            stem: stem_of(file_name),
            ext: extension_of(Some(file_name), kind)?,
        }));
    }

    Ok(None)
}

/// Предел для конкретного типа. Фото ограничено отправкой, остальное
/// скачиванием: оба предела Bot API, и оба проверяются до чтения тела.
pub fn max_bytes(kind: MemeKind) -> u64 {
    match kind {
        MemeKind::Image => PHOTO_LIMIT,
        MemeKind::Video | MemeKind::Audio => DOWNLOAD_LIMIT,
    }
}

pub fn limit_text(kind: MemeKind) -> &'static str {
    match kind {
        MemeKind::Image => "Фото больше 10 МБ бот отправить не сможет.",
        MemeKind::Video | MemeKind::Audio => "Bot API отдаёт файлы только до 20 МБ.",
    }
}

/// Каталог для типа мема. Держится рядом с `Memes::scan`, чтобы новая
/// загрузка не попала в каталог, который скан не читает.
pub fn target_dir(kind: MemeKind) -> &'static str {
    match kind {
        MemeKind::Video => "video",
        MemeKind::Image => "fotos",
        MemeKind::Audio => "music",
    }
}

/// Проверка имени. Это имя файла без расширения: разделители пути,
/// управляющие символы и `..` запрещены, иначе запись уйдёт из каталога
/// мемов или сломает карточку панели.
pub fn validate_stem(stem: &str) -> Result<String, String> {
    // Точки на конце срезаются до проверок: имя файла с точкой на конце
    // склеилось бы с расширением в пустое имя.
    let trimmed = stem.trim().trim_end_matches('.').trim_end();
    if trimmed.is_empty() {
        return Err("Имя пустое.".to_string());
    }
    if trimmed.chars().count() > NAME_MAX {
        return Err(format!("Имя длиннее {NAME_MAX} символов."));
    }
    if trimmed.chars().any(char::is_control) {
        return Err("Имя содержит управляющие символы.".to_string());
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err("В имени есть разделитель пути.".to_string());
    }
    if trimmed.contains("..") {
        return Err("В имени есть '..'.".to_string());
    }
    if trimmed.starts_with('.') {
        return Err("Имя не может начинаться с точки.".to_string());
    }
    Ok(trimmed.to_string())
}

/// Имя файла на диске: stem и расширение. Расширение приходит из Telegram
/// или из типа мема, поэтому двойного `.mp4.mp4` не будет.
pub fn final_name(stem: &str, ext: &str) -> String {
    let ext = ext.trim_start_matches('.');
    // Админ мог прислать имя уже с расширением: второй `.mp4` не нужен.
    let lower = stem.to_ascii_lowercase();
    if lower.ends_with(&format!(".{}", ext.to_ascii_lowercase())) {
        return stem.to_string();
    }
    format!("{stem}.{ext}")
}

fn stem_of(file_name: &str) -> Option<String> {
    Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| stem.to_string())
}

/// Расширение результата. Если Telegram сообщил расширение чужого типа или
/// вовсе неизвестное, это ошибка, а не повод подменить его: файл `.mkv`,
/// сохранённый как `.mp4`, не отправится.
fn extension_of(file_name: Option<&str>, kind: MemeKind) -> Result<String, String> {
    let from_name = file_name
        .and_then(|name| Path::new(name).extension())
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase());

    match from_name {
        Some(ext) if MemeKind::from_path(Path::new(&format!("x.{ext}"))) == Some(kind) => Ok(ext),
        Some(ext) if MemeKind::from_path(Path::new(&format!("x.{ext}"))).is_some() => Err(format!(
            "Расширение .{ext} не подходит: файл пришёл другого типа."
        )),
        Some(ext) => Err(format!("Расширение .{ext} не поддерживается.")),
        None => Ok(match kind {
            MemeKind::Video => "mp4".to_string(),
            MemeKind::Image => "jpg".to_string(),
            MemeKind::Audio => "mp3".to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn names_with_path_parts_are_refused() {
        assert_eq!(validate_stem("  бомжара  ").unwrap(), "бомжара");
        assert!(validate_stem("../secret").is_err());
        assert!(validate_stem("sub/dir").is_err());
        assert!(validate_stem("sub\\dir").is_err());
        assert!(validate_stem("..").is_err());
        assert!(validate_stem(".hidden").is_err());
        assert!(validate_stem("a\u{0}b").is_err());
        assert!(validate_stem("").is_err());
        assert!(validate_stem("   ").is_err());
        assert!(validate_stem(&"я".repeat(NAME_MAX + 1)).is_err());
        assert!(validate_stem(&"я".repeat(NAME_MAX)).is_ok());
        // Точка на конце срезается, иначе расширение приклеилось бы к пустому.
        assert_eq!(validate_stem("имя...").unwrap(), "имя");

        // Готовая склейка не даёт двойного расширения и не выходит из каталога.
        let joined = final_name("бомжара", "mp4");
        assert_eq!(joined, "бомжара.mp4");
        assert_eq!(final_name("clip.mp4", "mp4"), "clip.mp4");
        assert_eq!(final_name("clip.MP4", "mp4"), "clip.MP4");
        let dir = target_dir(MemeKind::Video);
        let path = PathBuf::from(dir).join(&joined);
        assert!(path.starts_with("video"), "{path:?}");
    }

    #[test]
    fn limits_follow_bot_api() {
        assert_eq!(max_bytes(MemeKind::Image), PHOTO_LIMIT);
        assert_eq!(max_bytes(MemeKind::Video), DOWNLOAD_LIMIT);
        assert_eq!(max_bytes(MemeKind::Audio), DOWNLOAD_LIMIT);
        assert!(limit_text(MemeKind::Image).contains("10 МБ"));
        assert!(limit_text(MemeKind::Video).contains("20 МБ"));
    }

    #[test]
    fn target_directories_match_the_scan() {
        assert_eq!(target_dir(MemeKind::Video), "video");
        assert_eq!(target_dir(MemeKind::Image), "fotos");
        assert_eq!(target_dir(MemeKind::Audio), "music");
    }

    #[test]
    fn extension_falls_back_to_kind_and_never_mixes_kinds() {
        assert_eq!(
            extension_of(Some("clip.MP4"), MemeKind::Video).unwrap(),
            "mp4"
        );
        assert_eq!(
            extension_of(Some("clip.mov"), MemeKind::Video).unwrap(),
            "mov"
        );
        // Чужое и неизвестное расширение это отказ, а не подмена.
        assert!(extension_of(Some("song.mp3"), MemeKind::Video).is_err());
        assert!(extension_of(Some("clip.mkv"), MemeKind::Video).is_err());
        assert_eq!(extension_of(None, MemeKind::Image).unwrap(), "jpg");
        assert_eq!(extension_of(Some("noext"), MemeKind::Audio).unwrap(), "mp3");
        assert_eq!(
            extension_of(Some("pic.png"), MemeKind::Image).unwrap(),
            "png"
        );
    }

    #[test]
    fn stem_is_taken_without_directories_or_extension() {
        assert_eq!(stem_of("clip.mp4").as_deref(), Some("clip"));
        assert_eq!(
            stem_of("Тёмный принц - Овердоз.mp3").as_deref(),
            Some("Тёмный принц - Овердоз")
        );
        assert_eq!(stem_of("noext").as_deref(), Some("noext"));
    }
}

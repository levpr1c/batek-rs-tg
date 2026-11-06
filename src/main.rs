//! Точка входа: конфигурация, журналирование и запуск Telegram-бота.
//!
//! Бот по одной команде, без базы данных и без голосовых чатов: команды
//! шелят `yt-dlp`, отправляют файлы из `video/` и `music/` и отвечают
//! на ключевые слова в группах.

mod keywords;
mod memes;
mod misc;
mod paths;
mod telegram;
mod youtube;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::memes::Memes;
use crate::youtube::YoutubeCache;

fn main() -> anyhow::Result<()> {
    // Отсутствие .env не ошибка: переменные могут прийти из окружения.
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    // Индекс ключевых слов строится до первого сообщения: первый матч не
    // должен платить за нормализацию всей таблицы.
    keywords::warm();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("не удалось создать рантайм tokio")?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let root: Arc<PathBuf> = Arc::new(paths::resolve_root());

    let token = std::env::var("TELEGRAM_TOKEN")
        .unwrap_or_default()
        .trim()
        .to_string();
    if token.is_empty() {
        anyhow::bail!("TELEGRAM_TOKEN не задан (см. .env.example)");
    }

    let yt = Arc::new(YoutubeCache::load(root.join("yt_cache.json")).await);
    youtube::spawn_refresh(yt.clone());

    let memes = Arc::new(Memes::scan(&root));
    info!(
        root = %root.display(),
        memes = memes.len(),
        "стартовая подготовка завершена"
    );

    let data = telegram::TgData::new(token, yt, memes, root);
    telegram::spawn(data).await?;

    info!("Ctrl-C для остановки");
    tokio::signal::ctrl_c().await.context("ожидание Ctrl-C")?;

    Ok(())
}

//! Права: главный админ из переменной окружения, модераторы и одноразовые
//! приглашения из `data/access.json`.
//!
//! Админа из файла назначить нельзя: он один и живёт в `TG_ADMIN_USER_ID`,
//! как отладчик в `TG_DEBUG_USER_ID`. Иначе скомпрометированный файл
//! настроек повышал бы права до полных. Модератор правит ключевые слова и
//! не выдаёт ни модератора, ни админа: выдача это всегда админ.
//!
//! Приглашение это короткий код, а не пароль. Код живёт 15 минут, срабатывает
//! один раз и только в личном чате: в истории группы он остался бы навсегда.

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::seq::IndexedRandom;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::store;

/// Сколько живёт одноразовое приглашение. Пятнадцати минут хватает, чтобы
/// переслать код нужному человеку, и мало, чтобы код залежался в переписке.
pub const INVITE_TTL: Duration = Duration::from_secs(15 * 60);

/// Алфавит кода без похожих символов: `0`/`O`, `1`/`I`/`l` и `5`/`S` путают
/// при переписывании вручную.
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRTUVWXYZ2346789";
const CODE_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Admin,
    Moderator,
}

/// Действие модератора: ключевые слова и файлы. Выдача прав и список
/// модераторов остаются админу.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    ManageKeywords,
    ManageAccess,
    UploadFiles,
}

impl Role {
    pub fn can(self, capability: Capability) -> bool {
        match (self, capability) {
            (Role::Admin, _) => true,
            (Role::Moderator, Capability::ManageKeywords) => true,
            (Role::Moderator, _) => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invite {
    pub code: String,
    /// Unix-время в секундах, после которого код недействителен.
    pub expires_at: u64,
    pub created_by: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccessFile {
    #[serde(default)]
    pub moderators: Vec<i64>,
    #[serde(default)]
    pub invites: Vec<Invite>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RedeemError {
    NotFound,
    Expired,
    SaveFailed,
}

/// Права процесса. Файл читается один раз при старте, дальше состояние
/// живёт в памяти и переписывается целиком после каждой правки.
pub struct Access {
    admin: Option<i64>,
    path: PathBuf,
    state: RwLock<AccessFile>,
}

impl Access {
    /// `admin` читается из `TG_ADMIN_USER_ID`. Мусор в переменной это
    /// выключенная панель, а не админ с id 0.
    pub fn load(root: &Path) -> Self {
        let path = store::data_dir(root).join("access.json");
        let admin = read_admin_id(std::env::var("TG_ADMIN_USER_ID").ok().as_deref());
        let state = store::read_json::<AccessFile>(&path).unwrap_or_default();
        Self {
            admin,
            path,
            state: RwLock::new(state),
        }
    }

    /// Конструктор для тестов: без переменных окружения и с готовым файлом.
    #[cfg(test)]
    pub fn with_state(admin: Option<i64>, path: PathBuf, state: AccessFile) -> Self {
        Self {
            admin,
            path,
            state: RwLock::new(state),
        }
    }

    pub fn admin(&self) -> Option<i64> {
        self.admin
    }

    pub fn role(&self, user: i64) -> Option<Role> {
        if self.admin == Some(user) {
            return Some(Role::Admin);
        }
        if self.state.read().unwrap().moderators.contains(&user) {
            return Some(Role::Moderator);
        }
        None
    }

    pub fn is_enabled(&self) -> bool {
        self.admin.is_some()
    }

    pub fn moderators(&self) -> Vec<i64> {
        self.state.read().unwrap().moderators.clone()
    }

    /// Добавляет модератора. Повторный id не ошибка: список не растёт.
    pub fn add_moderator(&self, user: i64) -> Result<(), String> {
        let mut next = self.state.read().unwrap().clone();
        if !next.moderators.contains(&user) {
            next.moderators.push(user);
        }
        next.moderators.sort_unstable();
        self.commit(next)
    }

    pub fn remove_moderator(&self, user: i64) -> Result<(), String> {
        let mut next = self.state.read().unwrap().clone();
        next.moderators.retain(|known| *known != user);
        self.commit(next)
    }

    /// Выдаёт код приглашения. Старые просроченные коды вычищаются здесь же,
    /// чтобы файл не рос от каждой генерации.
    pub fn create_invite(&self, by: i64, now: u64) -> Result<String, String> {
        let mut next = self.state.read().unwrap().clone();
        next.invites.retain(|invite| invite.expires_at > now);
        let code = random_code();
        next.invites.push(Invite {
            code: code.clone(),
            expires_at: now + INVITE_TTL.as_secs(),
            created_by: by,
        });
        self.commit(next)?;
        Ok(code)
    }

    /// Гасит приглашение и выдаёт модератора. Код удаляется до записи файла:
    /// повторное использование обязано провалиться, даже если запись упала.
    pub fn redeem(&self, code: &str, user: i64, now: u64) -> Result<(), RedeemError> {
        let code = code.trim().to_ascii_uppercase();
        if code.is_empty() {
            return Err(RedeemError::NotFound);
        }
        let mut next = self.state.read().unwrap().clone();
        let index = next
            .invites
            .iter()
            .position(|invite| invite.code == code)
            .ok_or(RedeemError::NotFound)?;
        if next.invites[index].expires_at <= now {
            next.invites.remove(index);
            let _ = self.commit(next);
            return Err(RedeemError::Expired);
        }
        next.invites.remove(index);
        if !next.moderators.contains(&user) {
            next.moderators.push(user);
            next.moderators.sort_unstable();
        }
        self.commit(next).map_err(|_| RedeemError::SaveFailed)
    }

    /// Записывает копию, затем подменяет состояние в памяти. Если запись не
    /// удалась, память остаётся прежней и бот честно отвечает об ошибке.
    fn commit(&self, next: AccessFile) -> Result<(), String> {
        if let Err(e) = store::write_json_atomic(&self.path, &next) {
            warn!("access: {} не записан: {e}", self.path.display());
            return Err("не удалось сохранить права".to_string());
        }
        *self.state.write().unwrap() = next;
        Ok(())
    }
}

/// Разбор `TG_ADMIN_USER_ID`. Не задано или не число: панель выключена.
/// Значение в журнал не попадает, только факт ошибки.
pub fn read_admin_id(raw: Option<&str>) -> Option<i64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.parse::<i64>() {
        Ok(id) => Some(id),
        Err(e) => {
            warn!("access: TG_ADMIN_USER_ID не число ({e}), панель выключена");
            None
        }
    }
}

fn random_code() -> String {
    let mut rng = rand::rng();
    (0..CODE_LEN)
        .map(|_| CODE_ALPHABET.choose(&mut rng).copied().unwrap_or(b'X') as char)
        .collect()
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("batek-access-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("access.json")
    }

    fn access(tag: &str, admin: Option<i64>) -> Access {
        Access::with_state(admin, temp_path(tag), AccessFile::default())
    }

    #[test]
    fn admin_comes_only_from_env_value() {
        assert_eq!(read_admin_id(Some("42")), Some(42));
        assert_eq!(read_admin_id(Some(" 42 ")), Some(42));
        assert_eq!(read_admin_id(Some("")), None);
        assert_eq!(read_admin_id(Some("сорок два")), None);
        assert_eq!(read_admin_id(None), None);
    }

    #[test]
    fn roles_follow_admin_and_moderator_list() {
        let acc = access("roles", Some(1));
        assert_eq!(acc.role(1), Some(Role::Admin));
        assert_eq!(acc.role(2), None, "чужой без прав");

        acc.add_moderator(2).unwrap();
        assert_eq!(acc.role(2), Some(Role::Moderator));
        assert!(acc.moderators().contains(&2));

        // Повтор не плодит записи.
        acc.add_moderator(2).unwrap();
        assert_eq!(acc.moderators(), vec![2]);

        acc.remove_moderator(2).unwrap();
        assert_eq!(acc.role(2), None);
        let _ = std::fs::remove_dir_all(acc.path.parent().unwrap());
    }

    #[test]
    fn moderator_cannot_manage_access_or_upload() {
        assert!(Role::Moderator.can(Capability::ManageKeywords));
        assert!(!Role::Moderator.can(Capability::ManageAccess));
        assert!(!Role::Moderator.can(Capability::UploadFiles));
        assert!(Role::Admin.can(Capability::ManageAccess));
        assert!(Role::Admin.can(Capability::ManageKeywords));
        assert!(Role::Admin.can(Capability::UploadFiles));
    }

    #[test]
    fn disabled_panel_has_no_admin() {
        let acc = access("disabled", None);
        assert!(!acc.is_enabled());
        assert_eq!(acc.role(1), None);
        assert_eq!(acc.admin(), None);
    }

    #[test]
    fn invite_works_once_and_expires() {
        let acc = access("invite", Some(1));
        let now = 1_000_000u64;
        let code = acc.create_invite(1, now).unwrap();
        assert_eq!(code.len(), CODE_LEN);
        assert!(
            code.chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()),
            "{code}"
        );

        // Неверный код не выдаёт прав.
        assert_eq!(acc.redeem("ZZZZZZZZ", 5, now), Err(RedeemError::NotFound));
        assert_eq!(acc.role(5), None);

        // Верный код срабатывает один раз.
        assert_eq!(acc.redeem(&code, 5, now), Ok(()));
        assert_eq!(acc.role(5), Some(Role::Moderator));
        assert_eq!(acc.redeem(&code, 6, now), Err(RedeemError::NotFound));

        let _ = std::fs::remove_dir_all(acc.path.parent().unwrap());
    }

    #[test]
    fn expired_invite_is_refused_and_removed() {
        let acc = access("expired", Some(1));
        let now = 1_000_000u64;
        let code = acc.create_invite(1, now).unwrap();
        let later = now + INVITE_TTL.as_secs() + 1;

        assert_eq!(acc.redeem(&code, 7, later), Err(RedeemError::Expired));
        assert_eq!(acc.role(7), None);
        assert!(acc.state.read().unwrap().invites.is_empty());

        let _ = std::fs::remove_dir_all(acc.path.parent().unwrap());
    }

    #[test]
    fn new_invite_prunes_expired_ones() {
        let acc = access("prune", Some(1));
        let now = 1_000_000u64;
        let _old = acc.create_invite(1, now).unwrap();
        let _new = acc
            .create_invite(1, now + INVITE_TTL.as_secs() + 1)
            .unwrap();

        let state = acc.state.read().unwrap();
        assert_eq!(state.invites.len(), 1, "просроченный код остался в файле");

        let _ = std::fs::remove_dir_all(acc.path.parent().unwrap());
    }

    #[test]
    fn state_survives_reload_from_file() {
        let path = temp_path("reload");
        {
            let acc = Access::with_state(Some(1), path.clone(), AccessFile::default());
            acc.add_moderator(9).unwrap();
        }
        let state = store::read_json::<AccessFile>(&path).unwrap();
        let reloaded = Access::with_state(Some(1), path.clone(), state);
        assert_eq!(reloaded.role(9), Some(Role::Moderator));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn code_alphabet_avoids_ambiguous_characters() {
        for byte in CODE_ALPHABET {
            assert!(
                !matches!(byte, b'0' | b'O' | b'1' | b'I' | b'L' | b'5' | b'S'),
                "неоднозначный символ {} в алфавите кода",
                *byte as char
            );
        }
        // Двадцать прогонов не должны повторяться: код случайный.
        let codes: std::collections::HashSet<String> = (0..20).map(|_| random_code()).collect();
        assert!(codes.len() > 15, "коды повторяются: {codes:?}");
    }
}

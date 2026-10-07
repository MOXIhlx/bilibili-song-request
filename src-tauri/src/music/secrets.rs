//! 敏感凭据存储（Windows 凭据管理器 / macOS Keychain / Linux Secret Service）。
//!
//! ## 为什么要有回退
//! `keyring` 依赖系统凭据服务。在以下场景里它可能不可用：
//!  - Linux 桌面缺少 Secret Service（无 D-Bus / 无 gnome-keyring）
//!  - Windows 凭据管理器策略被限制
//!  - 便携版 exe 运行在受限账户下
//!
//! 如果此时直接失败，用户就完全无法登录音乐平台。因此本模块的策略是：
//!  **优先写系统凭据库；不可用时退回到本地文件**，并在返回结果里明确告知位置，
//!  同时把文件权限收紧到「仅当前用户」。
//!
//! ⚠️ 回退到文件时，凭据是**明文**存储的（只是权限受限）。这一点必须让用户知道，
//! 所以 `save` 会返回 [`StoredIn`] 让上层写日志、前端显示提示。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// keyring 服务名。
pub const KEYRING_SERVICE: &str = "bilibili-song-request";

/// 网易云 Cookie 的条目名。
pub const KEYRING_ENTRY_NETEASE_COOKIE: &str = "music.netease.cookie";
/// QQ 音乐 Cookie 的条目名。
pub const KEYRING_ENTRY_QQ_COOKIE: &str = "music.qq.cookie";
/// B 站 access_key_secret 的条目名（阶段 5 起迁入凭据库）。
pub const KEYRING_ENTRY_BILIBILI_SECRET: &str = "bilibili.access_key_secret";

/// 凭据最终存放位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoredIn {
    /// 系统凭据库（推荐）。
    Keyring,
    /// 本地受限权限文件（回退方案，明文）。
    File,
}

impl StoredIn {
    /// 给用户看的中文说明。
    pub fn describe(self) -> &'static str {
        match self {
            StoredIn::Keyring => "已保存到系统凭据管理器",
            StoredIn::File => "系统凭据管理器不可用，已保存到受限权限的本地文件（明文）",
        }
    }
}

/// 凭据读写。
pub struct SecretStore {
    /// keyring 条目名。
    entry: String,
    /// 回退文件路径。
    fallback_path: PathBuf,
    /// 是否跳过系统凭据库（测试用）。
    ///
    /// 为什么需要它：系统凭据库是按「服务名 + 条目名」全局索引的，
    /// 并行测试如果都用同一个条目名，就会互相覆盖（一个用例 clear 掉另一个刚存的
    /// Cookie），表现为随机失败。测试场景下显式跳过凭据库即可彻底隔离。
    skip_keyring: bool,
    /// 最近一次读取到凭据的位置。
    ///
    /// 为什么需要它：前端要显示「Cookie 存到哪里了」。凭据库有单条长度上限
    /// （Windows 是 2560 字节，且按 UTF-16 计算），超长会静默退化成明文文件——
    /// 这时必须明确告诉用户，否则他们以为凭据在系统凭据库里，实际躺在一个 txt 里。
    last_stored_in: std::sync::Mutex<Option<StoredIn>>,
}

impl SecretStore {
    /// 创建凭据存取器。
    ///
    /// `entry` 是条目名（例如 `music.netease.cookie`），
    /// 回退文件放在 `Config::config_dir()/secrets/<entry>.txt`。
    pub fn new(entry: impl Into<String>) -> Self {
        let entry = entry.into();
        let dir = crate::config::Config::config_dir().join("secrets");
        let _ = std::fs::create_dir_all(&dir);
        Self {
            fallback_path: dir.join(format!("{entry}.txt")),
            entry,
            skip_keyring: false,
            last_stored_in: std::sync::Mutex::new(None),
        }
    }

    /// 指定回退文件目录（测试用，避免写入真实配置目录）。
    pub fn with_dir(entry: impl Into<String>, dir: impl AsRef<Path>) -> Self {
        let entry = entry.into();
        let dir = dir.as_ref().to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        Self {
            fallback_path: dir.join(format!("{entry}.txt")),
            entry,
            skip_keyring: false,
            last_stored_in: std::sync::Mutex::new(None),
        }
    }

    /// **仅使用回退文件**，完全不触碰系统凭据库。
    ///
    /// 专供测试：避免并行用例争抢同名凭据库条目。
    pub fn file_only(entry: impl Into<String>, dir: impl AsRef<Path>) -> Self {
        let mut store = Self::with_dir(entry, dir);
        store.skip_keyring = true;
        store
    }

    /// 回退文件路径（用于日志与界面提示）。
    pub fn fallback_path(&self) -> &Path {
        &self.fallback_path
    }

    /// 读取凭据：先查系统凭据库，再查回退文件。
    ///
    /// 同时记录这次是从哪里读到的（供界面显示「存到哪里了」）。
    pub fn load(&self) -> Option<String> {
        if !self.skip_keyring {
            match keyring::Entry::new(KEYRING_SERVICE, &self.entry) {
                Ok(entry) => match entry.get_password() {
                    Ok(value) if !value.trim().is_empty() => {
                        debug!(entry = %self.entry, "已从系统凭据库读取凭据");
                        self.remember(StoredIn::Keyring);
                        return Some(value);
                    }
                    Ok(_) => {}
                    Err(keyring::Error::NoEntry) => {}
                    Err(err) => debug!(error = %err, "读取系统凭据库失败"),
                },
                Err(err) => debug!(error = %err, "访问系统凭据库失败"),
            }
        }

        match std::fs::read_to_string(&self.fallback_path) {
            Ok(value) if !value.trim().is_empty() => {
                debug!(path = %self.fallback_path.display(), "已从回退文件读取凭据");
                self.remember(StoredIn::File);
                Some(value)
            }
            Ok(_) => None,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => {
                warn!(error = %err, "读取凭据文件失败");
                None
            }
        }
    }

    /// 记录凭据实际存放位置。
    fn remember(&self, location: StoredIn) {
        *self
            .last_stored_in
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(location);
    }

    /// 最近一次读取到凭据的位置（没读到过则为 `None`）。
    pub fn stored_in(&self) -> Option<StoredIn> {
        *self
            .last_stored_in
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 保存凭据，返回实际存放位置。
    pub fn save(&self, value: &str) -> Result<StoredIn> {
        if !self.skip_keyring {
            match keyring::Entry::new(KEYRING_SERVICE, &self.entry) {
                Ok(entry) => match entry.set_password(value) {
                    Ok(()) => {
                        info!(entry = %self.entry, "凭据已写入系统凭据库");
                        // 写入成功后清理可能存在的旧回退文件，避免两份不一致。
                        let _ = std::fs::remove_file(&self.fallback_path);
                        return Ok(StoredIn::Keyring);
                    }
                    Err(err) => warn!(error = %err, "写入系统凭据库失败，改用回退文件"),
                },
                Err(err) => warn!(error = %err, "访问系统凭据库失败，改用回退文件"),
            }
        }

        self.save_to_file(value)
            .with_context(|| format!("写入凭据文件 {} 失败", self.fallback_path.display()))?;
        info!(
            path = %self.fallback_path.display(),
            "凭据已写入受限权限的本地文件（明文）"
        );
        Ok(StoredIn::File)
    }

    /// 删除凭据（两处都清）。
    pub fn clear(&self) -> Result<()> {
        if !self.skip_keyring {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &self.entry) {
                match entry.delete_credential() {
                    Ok(()) => info!(entry = %self.entry, "已从系统凭据库删除凭据"),
                    Err(keyring::Error::NoEntry) => {}
                    Err(err) => debug!(error = %err, "删除系统凭据失败（可能本就不存在）"),
                }
            }
        }
        match std::fs::remove_file(&self.fallback_path) {
            Ok(()) => info!(path = %self.fallback_path.display(), "已删除回退凭据文件"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => warn!(error = %err, "删除回退凭据文件失败"),
        }
        Ok(())
    }

    /// 是否已保存凭据。
    pub fn exists(&self) -> bool {
        self.load().is_some()
    }

    /// 写回退文件并把权限收紧到当前用户。
    fn save_to_file(&self, value: &str) -> Result<()> {
        if let Some(parent) = self.fallback_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.fallback_path, value)?;
        restrict_permissions(&self.fallback_path)?;
        Ok(())
    }
}

/// 把文件权限限制为「仅当前用户可读写」。
#[cfg(windows)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::process::Command;

    // Windows 上不做继承 ACL 的精细操作（需要 winapi），改用 icacls 收紧：
    // 关闭继承、移除继承来的 ACE，只保留当前用户。
    let user = std::env::var("USERNAME").unwrap_or_default();
    if user.is_empty() {
        return Ok(());
    }
    let output = Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{user}:F"))
        .output();
    match output {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "icacls 收紧权限失败（凭据文件仍可读）"
            );
            Ok(())
        }
        Err(err) => {
            warn!(error = %err, "调用 icacls 失败（凭据文件仍可读）");
            Ok(())
        }
    }
}

/// Unix：chmod 600。
#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// 其他平台：不做处理。
#[cfg(not(any(windows, unix)))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bsr-secrets-{:?}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_fallback_roundtrip() {
        let dir = temp_dir();
        // 用 file_only：真实凭据库是全局共享的，测试里用它会导致
        // 「存进去 → 被别的用例 clear 掉」这类随机失败。
        let store = SecretStore::file_only("test.cookie", &dir);
        assert_eq!(store.save("MUSIC_U=abc123; __csrf=xyz").unwrap(), StoredIn::File);
        assert_eq!(
            store.load().expect("应读到凭据"),
            "MUSIC_U=abc123; __csrf=xyz"
        );
        assert!(store.exists());
        store.clear().expect("清除应成功");
        assert!(!store.exists());
    }

    #[test]
    fn file_fallback_writes_plain_file_when_keyring_unavailable() {
        let dir = temp_dir();
        let store = SecretStore::with_dir("test.file", &dir);
        // 直接走文件路径，验证内容与权限收紧逻辑不报错
        store.save_to_file("secret-value").expect("写文件应成功");
        assert!(store.fallback_path().is_file());
        assert_eq!(std::fs::read_to_string(store.fallback_path()).unwrap(), "secret-value");
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = temp_dir();
        let store = SecretStore::with_dir("test.clear", &dir);
        store.clear().unwrap();
        store.clear().unwrap();
    }

    #[test]
    fn missing_entry_returns_none() {
        let dir = temp_dir();
        let store = SecretStore::file_only("test.missing", &dir);
        assert!(store.load().is_none());
        assert!(!store.exists());
    }

    #[test]
    fn keyring_entry_naming_is_stable() {
        // 条目名是持久化契约：改名会让已保存的凭据「消失」。
        assert_eq!(KEYRING_ENTRY_NETEASE_COOKIE, "music.netease.cookie");
        assert_eq!(KEYRING_ENTRY_QQ_COOKIE, "music.qq.cookie");
        assert_eq!(KEYRING_ENTRY_BILIBILI_SECRET, "bilibili.access_key_secret");
        assert_eq!(KEYRING_SERVICE, "bilibili-song-request");
    }

    #[test]
    fn stored_in_describes_location() {
        assert!(StoredIn::Keyring.describe().contains("凭据管理器"));
        assert!(StoredIn::File.describe().contains("明文"));
    }
}

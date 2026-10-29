//! Everything the user can tune, persisted as JSON so the configuration page
//! and the tray always agree.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::service;

/// Stored in `%LOCALAPPDATA%\DshLauncher\settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Port the `dsh web` service listens on.
    pub port: u16,
    /// Address the service binds to.
    pub host: String,
    /// Empty means "find it automatically".
    pub node_exe: String,
    /// Empty means "find it automatically".
    pub entry_script: String,
    /// Empty means `%USERPROFILE%\.dsh\logs`.
    pub log_directory: String,
    /// Upper bound while waiting for the service to answer.
    pub ready_timeout: u64,
    /// Upper bound while waiting for the port to be released.
    pub stop_timeout: u64,
    /// `--max-old-space-size=<n>` handed to node; 0 leaves the default.
    pub heap_mb: u32,
    /// Start the service as soon as the tray comes up.
    pub autostart: bool,
    /// Port of this configuration page.
    pub admin_port: u16,
    /// Serve the configuration page at all.
    pub admin_enabled: bool,
    /// dsh profile to boot: a stack of plugin patches under `$DSH_HOME\profiles`.
    pub profile: String,
    /// Extra `--patch` overlays, applied after the profile layer.
    pub patches: Vec<String>,
    /// `--public-url`; empty leaves the flag off.
    pub public_url: String,
    /// Extra `--trusted-host` authorities for the /api trust fence.
    pub trusted_hosts: Vec<String>,
    /// `--tls-cert` / `--tls-key`: both or neither.
    pub tls_cert: String,
    pub tls_key: String,
    /// Raw extra arguments for the web app, one per entry.
    pub extra_args: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: 3080,
            host: "127.0.0.1".to_string(),
            node_exe: String::new(),
            entry_script: String::new(),
            log_directory: String::new(),
            ready_timeout: 120,
            stop_timeout: 30,
            heap_mb: 0,
            autostart: true,
            admin_port: 3089,
            admin_enabled: true,
            profile: "web".to_string(),
            patches: Vec::new(),
            public_url: String::new(),
            trusted_hosts: Vec::new(),
            tls_cert: String::new(),
            tls_key: String::new(),
            extra_args: Vec::new(),
        }
    }
}

impl Settings {
    pub fn file() -> PathBuf {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        base.join("DshLauncher").join("settings.json")
    }

    /// Never fails: a broken file simply falls back to the defaults.
    pub fn load() -> Self {
        let path = Self::file();
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => return Self::default(),
        };
        match serde_json::from_str::<Settings>(&text) {
            Ok(settings) => {
                let mut settings = settings;
                if settings.validate().is_err() {
                    settings.repair();
                }
                settings
            }
            Err(error) => {
                crate::diag::write(&format!(
                    "settings.json is not valid ({error}), using defaults"
                ));
                Self::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::file();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建配置目录失败：{e}"))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|e| format!("序列化配置失败：{e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("写入 {} 失败：{e}", path.display()))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.port == 0 {
            return Err("服务端口不能为 0".to_string());
        }
        if self.admin_enabled && self.admin_port == 0 {
            return Err("配置页端口不能为 0".to_string());
        }
        if self.admin_enabled && self.admin_port == self.port {
            return Err("配置页端口不能和服务端口相同".to_string());
        }
        if self.heap_mb != 0 && !(256..=32768).contains(&self.heap_mb) {
            return Err("堆内存上限要么留空，要么在 256 到 32768 MB 之间".to_string());
        }
        if !(10..=900).contains(&self.ready_timeout) {
            return Err("启动超时要在 10 到 900 秒之间".to_string());
        }
        if !(5..=300).contains(&self.stop_timeout) {
            return Err("停止超时要在 5 到 300 秒之间".to_string());
        }
        if self.host.trim().is_empty() {
            return Err("监听地址不能为空".to_string());
        }
        if self.profile.trim().is_empty() {
            return Err("profile 名不能为空（默认是 web）".to_string());
        }
        if !self
            .profile
            .trim()
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err("profile 名只能用字母、数字和 - _ .".to_string());
        }
        if self.tls_cert.trim().is_empty() != self.tls_key.trim().is_empty() {
            return Err("TLS 证书和私钥要成对填写".to_string());
        }
        if self
            .extra_args
            .iter()
            .any(|arg| arg.trim().is_empty() || arg.contains('\n'))
        {
            return Err("附加参数里不能留空行".to_string());
        }
        Ok(())
    }

    /// Clamps out-of-range values instead of failing, used when loading a file
    /// that was hand-edited.
    fn repair(&mut self) {
        if self.heap_mb != 0 {
            self.heap_mb = ((self.heap_mb.clamp(256, 32768) + 127) / 256) * 256;
        }
        self.ready_timeout = self.ready_timeout.clamp(10, 900);
        self.stop_timeout = self.stop_timeout.clamp(5, 300);
        if self.host.trim().is_empty() {
            self.host = "127.0.0.1".to_string();
        }
        if self.admin_port == 0 || self.admin_port == self.port {
            self.admin_port = if self.port == DEFAULT_ADMIN_PORT {
                self.port.saturating_add(1)
            } else {
                DEFAULT_ADMIN_PORT
            };
        }
    }

    pub fn service_config(&self) -> service::Config {
        let mut config = service::Config::default();
        config.port = self.port;
        config.host = self.host.clone();
        config.node = non_empty_path(&self.node_exe);
        config.entry = non_empty_path(&self.entry_script);
        config.log_dir = non_empty_path(&self.log_directory);
        config.ready_timeout = Duration::from_secs(self.ready_timeout);
        config.stop_timeout = Duration::from_secs(self.stop_timeout);
        config.heap_mb = self.heap_mb;
        config.profile = self.profile.trim().to_string();
        config.patches = clean_list(&self.patches);
        config.public_url = self.public_url.trim().to_string();
        config.trusted_hosts = clean_list(&self.trusted_hosts);
        config.tls_cert = self.tls_cert.trim().to_string();
        config.tls_key = self.tls_key.trim().to_string();
        config.extra_args = clean_list(&self.extra_args);
        config
    }

    pub fn log_dir(&self) -> PathBuf {
        self.service_config().log_dir()
    }
}

pub const DEFAULT_ADMIN_PORT: u16 = 3089;

fn clean_list(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn non_empty_path(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_settings_files_still_load() {
        let json = r#"{"port":3080,"host":"127.0.0.1"}"#;
        let settings: Settings = serde_json::from_str(json).expect("legacy file must load");
        assert_eq!(settings.profile, "web");
        assert!(settings.patches.is_empty());
        assert!(settings.extra_args.is_empty());
    }

    #[test]
    fn tls_needs_both_halves() {
        let mut settings = Settings::default();
        settings.tls_cert = "server.pem".to_string();
        assert!(settings.validate().is_err());
        settings.tls_key = "server.key".to_string();
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn profile_name_is_restricted() {
        let mut settings = Settings::default();
        settings.profile = "web profile".to_string();
        assert!(settings.validate().is_err());
    }
}

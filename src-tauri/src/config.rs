use std::path::PathBuf;

use crate::types::{ArchiveFormat, ProxyMode};

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub cookie: String,
    pub download_dir: PathBuf,
    pub archive_format: ArchiveFormat,
    pub last_update_check_ts: i64,
    pub proxy_mode: ProxyMode,
    pub proxy_host: String,
    pub proxy_port: u16,
}

impl Config {
    pub fn new(app: &AppHandle) -> anyhow::Result<Self> {
        let app_data_dir = app.path().app_data_dir()?;
        let config_path = app_data_dir.join("config.json");
        // TODO: 实现Default trait以替代这种写法
        let default_config = Config {
            cookie: String::new(),
            download_dir: app_data_dir.join("漫画下载"),
            archive_format: ArchiveFormat::default(),
            last_update_check_ts: 0,
            proxy_mode: ProxyMode::default(),
            proxy_host: String::new(),
            proxy_port: 7890,
        };
        // 如果配置文件存在且能够解析，则使用配置文件中的配置，否则使用默认配置
        let config = if config_path.exists() {
            let config_string = std::fs::read_to_string(config_path)?;
            serde_json::from_str(&config_string).unwrap_or(default_config)
        } else {
            default_config
        };
        config.save(app)?;
        Ok(config)
    }

    pub fn save(&self, app: &AppHandle) -> anyhow::Result<()> {
        let app_data_dir = app.path().app_data_dir()?;
        let config_path = app_data_dir.join("config.json");
        let config_string = serde_json::to_string_pretty(self)?;
        std::fs::write(config_path, config_string)?;
        Ok(())
    }
}

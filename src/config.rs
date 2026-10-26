//! `app.json`：环境参数与默认设置。
//!
//! 文件与 `accounts.json` 同放在可执行文件所在目录（见 [`crate::paths`]）。
//!
//! # 兼容策略
//!
//! * 文件不存在 → 用默认值创建；
//! * 字段缺失 → 用默认值补齐（`#[serde(default)]`），**不**整份重置，
//!   这样新版本加字段不会让老文件失效；
//! * JSON 语法损坏 → 把原文件改名备份（`app.json.corrupt-<时间戳>`），再用默认值重建；
//! * 语义非法（例如刷新间隔为 0）→ 回落到默认值并记录 `warn`，不阻断启动。
//!
//! 修改本文件后需要**重启**程序生效：本计划（0002）明确不做热加载，
//! 以减少文件监听、半写读与线程同步的复杂度。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

use crate::paths;

/// 当前 `app.json` 的 schema 版本。
///
/// 将来字段含义发生不兼容变化时递增，并在这里写好迁移逻辑。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 刷新间隔默认值：300 秒。
pub const DEFAULT_REFRESH_INTERVAL_SECONDS: u64 = 300;

/// 单次 HTTP 请求默认超时：10 秒。
pub const DEFAULT_REQUEST_TIMEOUT_SECONDS: u64 = 10;

/// 刷新间隔下限：30 秒。再小会把实例与托盘菜单都压垮。
const MIN_REFRESH_INTERVAL_SECONDS: u64 = 30;

/// 刷新间隔上限：24 小时。
const MAX_REFRESH_INTERVAL_SECONDS: u64 = 24 * 60 * 60;

/// 请求超时下限：1 秒。
const MIN_REQUEST_TIMEOUT_SECONDS: u64 = 1;

/// 请求超时上限：120 秒。
const MAX_REQUEST_TIMEOUT_SECONDS: u64 = 120;

/// `app.json` 的内存表示。
///
/// 字段顺序即写盘顺序，方便人工阅读。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// schema 版本，便于将来迁移。
    pub schema_version: u32,

    /// 账户名是否打码展示（只影响 UI，不影响存储与请求）。
    ///
    /// 需求方确认：默认关闭。
    pub mask_accounts: bool,

    /// 后台刷新登录态与余额/订阅摘要的间隔（秒）。
    pub refresh_interval_seconds: u64,

    /// 单次 HTTP 请求超时（秒）。
    pub request_timeout_seconds: u64,

    /// 面板是否被"钉住"（置顶显示）。
    ///
    /// 钉住时点面板外部不会自动收起，方便边看数据边操作别的窗口。
    /// 这是 UI 偏好，但放 `app.json` 才能跨重启保留。
    pub panel_pinned: bool,

    /// 界面主题：`dark`（夜间）或 `light`（日间）。
    ///
    /// 用字符串而不是 bool，是为了将来可平滑扩展 `system` 等模式；
    /// 非法值在 [`AppConfig::sanitize`] 中回落到夜间。
    pub theme: String,

    /// 是否显示桌面悬浮球。
    pub floating_ball_visible: bool,

    /// 悬浮球是否启用贴边自动收起。
    pub floating_ball_edge_hide: bool,

    /// 悬浮球主账号 id。
    pub floating_ball_account_id: Option<String>,

    /// 悬浮球主 API Key id；为空表示展示账号余额。
    pub floating_ball_api_key_id: Option<String>,

    /// 悬浮球尺寸：`small` / `medium` / `large`。
    pub floating_ball_size: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            // 需求方确认：默认不打码
            mask_accounts: false,
            refresh_interval_seconds: DEFAULT_REFRESH_INTERVAL_SECONDS,
            request_timeout_seconds: DEFAULT_REQUEST_TIMEOUT_SECONDS,
            // 默认不钉住：托盘面板的常见预期是"点别处就收起"
            panel_pinned: false,
            // 保持现有观感，老用户升级后不会突然变成浅色
            theme: "dark".to_owned(),
            floating_ball_visible: false,
            floating_ball_edge_hide: true,
            floating_ball_account_id: None,
            floating_ball_api_key_id: None,
            floating_ball_size: "medium".to_owned(),
        }
    }
}

/// 加载结果：告诉调用方"发生了什么事"，便于写日志与在界面上提示。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ConfigLoadReport {
    /// 文件原本不存在，本次用默认值创建。
    pub created: bool,
    /// 文件损坏，已改名备份并用默认值重建。
    pub recovered_from_corruption: bool,
    /// 语义非法的字段及实际采用值，例如 `["refreshIntervalSeconds: 0 → 300"]`。
    pub adjustments: Vec<String>,
}

impl AppConfig {
    /// 把语义非法的字段纠正为合法值，并返回纠正说明。
    ///
    /// 这里不返回 `Err`：配置有问题是"可恢复"的，程序应当继续可用，
    /// 只需要把实际采用值写进日志，避免用户改了配置却不知道没生效。
    fn sanitize(&mut self) -> Vec<String> {
        let mut adjustments = Vec::new();

        if self.schema_version != CURRENT_SCHEMA_VERSION {
            adjustments.push(format!(
                "schemaVersion: {} → {}",
                self.schema_version, CURRENT_SCHEMA_VERSION
            ));
            self.schema_version = CURRENT_SCHEMA_VERSION;
        }

        if !(MIN_REFRESH_INTERVAL_SECONDS..=MAX_REFRESH_INTERVAL_SECONDS)
            .contains(&self.refresh_interval_seconds)
        {
            let actual = DEFAULT_REFRESH_INTERVAL_SECONDS;
            adjustments.push(format!(
                "refreshIntervalSeconds: {} → {}（合法范围 {MIN_REFRESH_INTERVAL_SECONDS}~{MAX_REFRESH_INTERVAL_SECONDS}）",
                self.refresh_interval_seconds, actual
            ));
            self.refresh_interval_seconds = actual;
        }

        if !(MIN_REQUEST_TIMEOUT_SECONDS..=MAX_REQUEST_TIMEOUT_SECONDS)
            .contains(&self.request_timeout_seconds)
        {
            let actual = DEFAULT_REQUEST_TIMEOUT_SECONDS;
            adjustments.push(format!(
                "requestTimeoutSeconds: {} → {}（合法范围 {MIN_REQUEST_TIMEOUT_SECONDS}~{MAX_REQUEST_TIMEOUT_SECONDS}）",
                self.request_timeout_seconds, actual
            ));
            self.request_timeout_seconds = actual;
        }

        if self.theme != "dark" && self.theme != "light" {
            let actual = "dark".to_owned();
            adjustments.push(format!("theme: {} -> {actual}", self.theme));
            self.theme = actual;
        }

        if !matches!(
            self.floating_ball_size.as_str(),
            "small" | "medium" | "large"
        ) {
            let actual = "medium".to_owned();
            adjustments.push(format!(
                "floatingBallSize: {} -> {actual}",
                self.floating_ball_size
            ));
            self.floating_ball_size = actual;
        }

        adjustments
    }

    /// 正式、可读的 JSON 文本（带缩进与结尾换行）。
    fn to_json(&self) -> Result<String> {
        let mut text = serde_json::to_string_pretty(self).context("序列化 app.json 失败")?;
        text.push('\n');
        Ok(text)
    }
}

/// `app.json` 的完整路径。
/// 把配置写回 `app.json`（原子写，先写临时文件再改名）。
///
/// 设置页保存时调用；写入失败会把错误抛给界面，由它提示用户（例如目录只读）。
pub fn save(dir: &Path, config: &AppConfig) -> Result<()> {
    let path = config_path(dir);
    let mut text = serde_json::to_string_pretty(config).context("序列化 app.json 失败")?;
    text.push('\n');
    crate::paths::write_atomic(&path, &text)
}

pub fn config_path(dir: &Path) -> PathBuf {
    dir.join(paths::APP_CONFIG_FILE_NAME)
}

/// 加载配置；必要时创建或修复文件。
///
/// 调用前应先用 [`paths::ensure_writable`] 确认目录可写，
/// 否则这里的写入会以一个相对难懂的错误失败。
pub fn load_or_create(dir: &Path) -> Result<(AppConfig, ConfigLoadReport)> {
    let path = config_path(dir);
    let mut report = ConfigLoadReport::default();

    // 1) 文件不存在 → 用默认值创建
    let Some(text) = paths::read_to_string_if_exists(&path)? else {
        let config = AppConfig::default();
        paths::write_atomic(&path, &config.to_json()?)?;
        report.created = true;
        log::info!("已创建默认配置文件：{}", path.display());
        return Ok((config, report));
    };

    // 2) 解析；失败就把坏文件隔离掉再重建，绝不让程序因为一个逗号而无法启动
    let mut config: AppConfig = match serde_json::from_str(&text) {
        Ok(config) => config,
        Err(err) => {
            log::warn!("app.json 解析失败（{err}），将备份后用默认值重建");
            let backup = paths::quarantine_corrupt_file(&path)?;
            log::warn!("损坏的配置文件已备份为：{}", backup.display());
            report.recovered_from_corruption = true;

            let config = AppConfig::default();
            paths::write_atomic(&path, &config.to_json()?)?;
            return Ok((config, report));
        }
    };

    // 3) 语义纠正；有变化就回写，保证文件里看到的就是实际生效值
    report.adjustments = config.sanitize();
    if !report.adjustments.is_empty() {
        for adjustment in &report.adjustments {
            log::warn!("app.json 字段非法，已纠正：{adjustment}");
        }
        paths::write_atomic(&path, &config.to_json()?)?;
    }

    Ok((config, report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 非法悬浮球尺寸回落到中档() {
        let mut config = AppConfig {
            floating_ball_size: "huge".to_owned(),
            ..AppConfig::default()
        };
        let adjustments = config.sanitize();
        assert_eq!(config.floating_ball_size, "medium");
        assert!(
            adjustments
                .iter()
                .any(|item| item.contains("floatingBallSize"))
        );
    }

    #[test]
    fn 悬浮球字段缺失时使用默认值() {
        let config: AppConfig = serde_json::from_str(r#"{"schemaVersion":1}"#).unwrap();
        assert!(!config.floating_ball_visible);
        assert!(config.floating_ball_edge_hide);
        assert!(config.floating_ball_account_id.is_none());
        assert!(config.floating_ball_api_key_id.is_none());
        assert_eq!(config.floating_ball_size, "medium");
    }

    #[test]
    fn 文件不存在时创建默认配置() {
        let dir = tempfile::tempdir().unwrap();
        let (config, report) = load_or_create(dir.path()).unwrap();

        assert!(report.created);
        assert_eq!(config, AppConfig::default());
        assert!(!config.mask_accounts, "默认不打码");

        let text = std::fs::read_to_string(config_path(dir.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["refreshIntervalSeconds"], 300);
        assert_eq!(parsed["requestTimeoutSeconds"], 10);
        assert_eq!(parsed["maskAccounts"], false);
        assert_eq!(config.theme, "dark");
        assert_eq!(parsed["theme"], "dark");
    }

    #[test]
    fn 缺失字段用默认值补齐() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path()), "{\"maskAccounts\": true}").unwrap();

        let (config, report) = load_or_create(dir.path()).unwrap();

        assert!(config.mask_accounts);
        assert_eq!(
            config.refresh_interval_seconds,
            DEFAULT_REFRESH_INTERVAL_SECONDS
        );
        assert!(!report.created);
        assert!(report.adjustments.is_empty());
    }

    #[test]
    fn 坏文件被隔离并重建() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path()), "{ this is not json").unwrap();

        let (config, report) = load_or_create(dir.path()).unwrap();

        assert!(report.recovered_from_corruption);
        assert_eq!(config, AppConfig::default());

        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("app.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "应当留下一个备份文件");
    }

    #[test]
    fn 非法刷新间隔回落并回写() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(dir.path()),
            "{\"refreshIntervalSeconds\": 0, \"maskAccounts\": true}",
        )
        .unwrap();

        let (config, report) = load_or_create(dir.path()).unwrap();

        assert_eq!(
            config.refresh_interval_seconds,
            DEFAULT_REFRESH_INTERVAL_SECONDS
        );
        assert!(config.mask_accounts, "其它合法字段不应被重置");
        assert_eq!(report.adjustments.len(), 1);

        // 回写后的文件应当是"实际生效值"
        let text = std::fs::read_to_string(config_path(dir.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["refreshIntervalSeconds"], 300);
    }

    #[test]
    fn 主题只接受日间与夜间() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path()), "{\"theme\":\"light\"}").unwrap();
        let (light, report) = load_or_create(dir.path()).unwrap();
        assert_eq!(light.theme, "light");
        assert!(report.adjustments.is_empty());

        std::fs::write(config_path(dir.path()), "{\"theme\":\"sepia\"}").unwrap();
        let (fallback, report) = load_or_create(dir.path()).unwrap();
        assert_eq!(fallback.theme, "dark");
        assert_eq!(report.adjustments.len(), 1);
    }

    #[test]
    fn 超时越界也回落() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(config_path(dir.path()), "{\"requestTimeoutSeconds\": 9999}").unwrap();

        let (config, report) = load_or_create(dir.path()).unwrap();

        assert_eq!(
            config.request_timeout_seconds,
            DEFAULT_REQUEST_TIMEOUT_SECONDS
        );
        assert_eq!(report.adjustments.len(), 1);
    }
}

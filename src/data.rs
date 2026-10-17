//! 数据层门面：把 `app.json`、`accounts.json` 与系统凭据存储组装成一个入口。
//!
//! 上层（面板、托盘菜单、登录流程）只跟 [`DataStore`] 打交道，不需要知道
//! 文件在哪、怎么加密、怎么排序。这样做的好处：
//!
//! * 0003 的登录流程只需要 `accounts_mut().add(...)` 与 `secrets()`；
//! * 0004 的菜单只需要 `accounts()`（已按添加时间排序）；
//! * 单元测试可以只测数据层，不启动界面。
//!
//! 初始化失败（典型：exe 放在只读目录）不会让进程退出，而是返回
//! [`DataLayer::Unavailable`]：面板会显示一条红色提示，登录入口保持不可用。

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::accounts::{self, Account, AccountsStore};
use crate::config::{self, AppConfig};
use crate::mask;
use crate::paths;
use crate::secrets::{self, SecretStore};

/// 数据层的两种状态。
pub enum DataLayer {
    /// 初始化成功，可以读写账号。
    Ready(DataStore),
    /// 初始化失败，原因用于界面提示（不包含敏感信息）。
    Unavailable(String),
}

/// 数据层主入口。
pub struct DataStore {
    /// 数据目录（可执行文件所在目录）。
    dir: PathBuf,
    /// 生效中的 `app.json`。
    config: AppConfig,
    /// 账号存储（已按添加时间排序）。
    accounts: AccountsStore,
    /// 系统凭据存储（DPAPI / 钥匙串）。
    secrets: Box<dyn SecretStore>,
}

impl DataStore {
    /// 按"解析目录 → 探测可写 → 加载配置 → 加载账号 → 准备凭据存储"的顺序初始化。
    ///
    /// 任何一步失败都会直接把错误抛给调用方；是否降级由 [`init`] 统一决定。
    pub fn open() -> Result<Self> {
        let dir = paths::data_dir()?;
        // 先探测可写：不可写时连文件都不应该创建（已与需求方确认不做静默回退）
        paths::ensure_writable(&dir)?;

        let (config, config_report) = config::load_or_create(&dir)?;
        let (accounts, accounts_report) = accounts::load_or_create(&dir)?;

        if config_report.recovered_from_corruption {
            log::warn!("app.json 已重建为默认配置");
        }
        if accounts_report.recovered_from_corruption {
            log::warn!("accounts.json 已重建为空列表");
        }

        Ok(Self {
            dir,
            config,
            accounts,
            secrets: secrets::system_store(),
        })
    }

    /// 数据目录。便携式的核心信息，面板底部会展示它。
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 生效中的配置。
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// 已登录账号，按 `createdAt` 升序（添加从早到晚）。
    pub fn accounts(&self) -> &[Account] {
        self.accounts.accounts()
    }

    /// 可变账号存储：登录成功后落库、删除账号时使用。
    ///
    /// 目前（0002）还没有调用方，留给 0003/0004。
    #[allow(dead_code, reason = "0003 登录落库与 0004 删除账号使用")]
    pub fn accounts_mut(&mut self) -> &mut AccountsStore {
        &mut self.accounts
    }

    /// 系统凭据存储：加密保存密码/token、删除账号时清钥匙串条目。
    ///
    /// 目前（0002）还没有调用方，留给 0003/0004。
    #[allow(dead_code, reason = "0003 保存凭据与 0004 删除凭据使用")]
    pub fn secrets(&self) -> &dyn SecretStore {
        self.secrets.as_ref()
    }

    /// 按 `app.json` 的 `maskAccounts` 决定是否打码账号名。
    ///
    /// 目前（0002）还没有调用方，留给 0004 的菜单标题与面板列表。
    #[allow(dead_code, reason = "0004 托盘菜单与面板展示使用")]
    pub fn display_email(&self, email: &str) -> String {
        mask::mask_if(self.config.mask_accounts, email)
    }
}

/// 初始化数据层；失败时返回可展示的错误原因，而不是直接终止进程。
pub fn init() -> DataLayer {
    match DataStore::open() {
        Ok(store) => {
            log::info!("数据目录：{}", store.dir().display());
            log::info!(
                "已加载 {} 个账号；maskAccounts={}；refreshIntervalSeconds={}",
                store.accounts().len(),
                store.config().mask_accounts,
                store.config().refresh_interval_seconds
            );
            DataLayer::Ready(store)
        }
        Err(err) => {
            let reason = format!("{err:#}");
            log::error!("数据层初始化失败：{reason}");
            DataLayer::Unavailable(reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 数据层在真实目录上能否串起来（用临时目录替代 exe 目录）。
    ///
    /// 直接测 `DataStore::open` 会写到 target/debug，这里改为分别调用
    /// `config::load_or_create` 与 `accounts::load_or_create`，验证两者能共存于同一目录。
    #[test]
    fn 配置与账号文件可以共存于同一目录() {
        let dir = tempfile::tempdir().unwrap();

        let (config, _) = config::load_or_create(dir.path()).unwrap();
        let (mut accounts, _) = accounts::load_or_create(dir.path()).unwrap();

        let account = Account::new(
            "https://sub2api.example.com".to_owned(),
            "user@example.com".to_owned(),
            accounts::AccountRole::User,
            "enc:test:v1:opaque".to_owned(),
        );
        accounts.add(account).unwrap();

        assert!(!config.mask_accounts);
        assert_eq!(accounts.accounts().len(), 1);
        assert!(dir.path().join(paths::APP_CONFIG_FILE_NAME).exists());
        assert!(dir.path().join(paths::ACCOUNTS_FILE_NAME).exists());
    }

    #[test]
    fn 打码开关通过数据层生效() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(paths::APP_CONFIG_FILE_NAME),
            "{\"maskAccounts\": true}",
        )
        .unwrap();

        let (config, _) = config::load_or_create(dir.path()).unwrap();
        let masked = mask::mask_if(config.mask_accounts, "zhangwei@example.com");
        assert_eq!(masked, "zh***@ex***.com");
    }
}

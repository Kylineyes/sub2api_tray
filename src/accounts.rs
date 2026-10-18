//! `accounts.json`：账号与（将来的）API Key 的**唯一**持久化文件。
//!
// 0002 交付的是"数据层 API"，真正的调用方（0003 登录落库、0004 托盘菜单）还没接入，
// 因此这里整体放开 dead_code，避免每个 `pub fn` 上挂一条 `#[allow]` 把文件淹没。
// 0003/0004 接入后应当删掉这一行，让编译器重新帮我们找出真正没用的代码。
#![allow(dead_code, reason = "数据层 API 由 0003/0004 消费，尚未接入")]
//!
//! # 文件形状
//!
//! ```jsonc
//! {
//!   "schemaVersion": 1,
//!   "accounts": [
//!     {
//!       "id": "<uuid v4>",
//!       "baseUrl": "https://sub2api.example.com",
//!       "email": "user@example.com",
//!       "role": "admin",
//!       "secrets": "enc:dpapi:v1:<base64>",
//!       "createdAt": "2026-10-18T10:00:00Z",
//!       "lastLoginAt": "2026-10-18T10:00:03Z",
//!       "tokenExpiresAt": "2026-10-19T10:00:03Z",
//!       "apiKeys": []
//!     }
//!   ]
//! }
//! ```
//!
//! # 不变量
//!
//! * **展示顺序 = `createdAt` 升序**（也就是"添加从早到晚"），由 [`AccountsStore`] 维护；
//! * `baseUrl + email` 在归一化后必须唯一（`baseUrl` 忽略尾部斜杠与主机名大小写，
//!   `email` 忽略大小写），重复添加会被拒绝；
//! * `secrets` 是**不透明字符串**，本模块不解析、不解密——加解密属于 [`crate::secrets`]，
//!   这样数据层不依赖任何系统 API，测试可以用假数据；
//! * `apiKeys` 目前恒为空数组，只为将来"在同一个文件里管理 API Key"预留结构，
//!   用 `serde_json::Value` 保存以便将来放宽字段时不会丢数据。

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::paths;

/// 当前 `accounts.json` 的 schema 版本。
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 账号在 Sub2API 里的角色，来自 `/api/v1/auth/me`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountRole {
    /// 管理员：能看到网关全局数据。
    Admin,
    /// 普通用户：只能看到自己的余额/订阅/用量。
    User,
    /// 尚未成功调用过 `/auth/me`，或服务端返回了未知角色。
    #[default]
    Unknown,
}

impl AccountRole {
    /// 面向界面的中文标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Admin => "管理员",
            Self::User => "用户",
            Self::Unknown => "未知",
        }
    }
}

/// 单个已登录账号。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    /// UUID v4，创建后不再变化；删除/更新都靠它定位。
    pub id: String,
    /// 实例地址，已归一化（无尾部斜杠、无查询参数）。
    pub base_url: String,
    /// 登录邮箱，原样保存（展示时才按开关打码）。
    pub email: String,
    /// 角色。
    pub role: AccountRole,
    /// 加密后的凭据块，格式由 [`crate::secrets`] 决定。
    pub secrets: String,
    /// 添加时间（RFC3339 UTC），决定展示顺序。
    pub created_at: String,
    /// 最近一次成功登录的时间。
    #[serde(default)]
    pub last_login_at: Option<String>,
    /// access token 的过期时间；用来决定什么时候主动走恢复链。
    #[serde(default)]
    pub token_expires_at: Option<String>,
    /// 面板里是否保持展开（Key 明细）状态。
    ///
    /// 纯 UI 状态，但需要跨重启保留，所以放进账号记录：删除账号时状态自然一起消失，
    /// 也不必为一行展示状态单独开文件。
    #[serde(default)]
    pub expanded: bool,
    /// 预留：该账号下的 API Key 列表。
    #[serde(default)]
    pub api_keys: Vec<serde_json::Value>,
}

impl Account {
    /// 创建一个新账号记录。
    ///
    /// `id` 与 `createdAt` 由本函数生成，调用方不需要（也不应该）自己造：
    /// 顺序与唯一性都依赖这两个字段。
    pub fn new(base_url: String, email: String, role: AccountRole, secrets: String) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            base_url,
            email,
            role,
            secrets,
            created_at: now_rfc3339(),
            last_login_at: None,
            token_expires_at: None,
            // 新账号默认收起，登录后不会自动铺开一屏
            expanded: false,
            api_keys: Vec::new(),
        }
    }
}

/// `accounts.json` 的顶层结构。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountsFile {
    /// schema 版本。
    pub schema_version: u32,
    /// 账号列表。
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Default for AccountsFile {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            accounts: Vec::new(),
        }
    }
}

/// 加载结果，用于日志与界面提示。
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AccountsLoadReport {
    /// 文件原本不存在，本次创建了空文件。
    pub created: bool,
    /// 文件损坏，已改名备份并重建为空文件。
    pub recovered_from_corruption: bool,
    /// 存在无法解析的 `createdAt`，排序回落到文件顺序。
    pub invalid_created_at: bool,
}

/// 账号存储：负责读、写、排序与唯一性。
///
/// 内部维持"已按 `createdAt` 升序"的不变量，因此 [`Self::accounts`] 可以直接拿去渲染菜单。
pub struct AccountsStore {
    path: PathBuf,
    accounts: Vec<Account>,
}

impl AccountsStore {
    /// 当前所有账号，顺序为添加时间从早到晚。
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    /// 数据文件路径（日志与排障用）。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 查找重复账号：`baseUrl` 归一化后忽略大小写，`email` 忽略大小写。
    pub fn find_duplicate(&self, base_url: &str, email: &str) -> Option<&Account> {
        let target_url =
            normalize_base_url(base_url).unwrap_or_else(|_| base_url.trim().to_owned());
        let target_email = email.trim().to_lowercase();
        self.accounts.iter().find(|account| {
            account.base_url.eq_ignore_ascii_case(&target_url)
                && account.email.trim().to_lowercase() == target_email
        })
    }

    /// 追加一个账号并落盘。
    ///
    /// 顺序：校验 → 去重 → 排序 → 写盘 → 提交内存状态。
    /// 写盘失败时内存状态**保持不变**，避免"界面里有、文件里没有"。
    pub fn add(&mut self, account: Account) -> Result<()> {
        let mut account = account;
        account.base_url = normalize_base_url(&account.base_url)?;
        if !is_valid_email(&account.email) {
            bail!("邮箱格式不正确：{}", account.email);
        }
        if let Some(existing) = self.find_duplicate(&account.base_url, &account.email) {
            bail!("该账号已存在：{}（{}）", existing.email, existing.base_url);
        }

        let mut next = self.accounts.clone();
        next.push(account);
        sort_accounts(&mut next);
        self.persist(&next)?;
        self.accounts = next;
        Ok(())
    }

    /// 按 id 删除账号；返回被删除的记录（不存在时返回 `None`）。
    ///
    /// 与 [`Self::add`] 一样：先写盘成功，再更新内存。
    pub fn remove(&mut self, id: &str) -> Result<Option<Account>> {
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            return Ok(None);
        };

        let mut next = self.accounts.clone();
        let removed = next.remove(index);
        self.persist(&next)?;
        self.accounts = next;
        Ok(Some(removed))
    }

    /// 更新某个账号的加密凭据与时间戳（token 续期 / 自动重登之后调用）。
    ///
    /// 返回 `false` 表示账号已不存在（例如刷新期间被用户删除），调用方忽略即可。
    /// 与其它写操作一致：先落盘成功，再提交内存状态。
    pub fn update_credentials(
        &mut self,
        id: &str,
        secrets: String,
        token_expires_at: Option<String>,
        last_login_at: Option<String>,
    ) -> Result<bool> {
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            return Ok(false);
        };

        let mut next = self.accounts.clone();
        next[index].secrets = secrets;
        next[index].token_expires_at = token_expires_at;
        if last_login_at.is_some() {
            next[index].last_login_at = last_login_at;
        }

        self.persist(&next)?;
        self.accounts = next;
        Ok(true)
    }

    /// 记录某个账号的展开/收起状态（写盘，供下次启动恢复）。
    ///
    /// 返回 `false` 表示账号已不存在。失败只影响"下次启动记不记得"，不阻塞交互，
    /// 因此调用方拿到 `Err` 时记录日志即可。
    pub fn set_expanded(&mut self, id: &str, expanded: bool) -> Result<bool> {
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            return Ok(false);
        };
        if self.accounts[index].expanded == expanded {
            return Ok(true);
        }

        let mut next = self.accounts.clone();
        next[index].expanded = expanded;
        self.persist(&next)?;
        self.accounts = next;
        Ok(true)
    }

    /// 覆盖某个账号的 API Key 缓存（`apiKeys` 数组）。
    ///
    /// 缓存内容由 [`crate::api::ApiKeyInfo::to_cache_value`] 生成，**不含完整 Key**。
    /// 返回 `false` 表示账号已不存在。
    pub fn set_api_keys(&mut self, id: &str, api_keys: Vec<serde_json::Value>) -> Result<bool> {
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            return Ok(false);
        };

        let mut next = self.accounts.clone();
        next[index].api_keys = api_keys;
        self.persist(&next)?;
        self.accounts = next;
        Ok(true)
    }

    /// 把当前内存状态写回磁盘。
    pub fn save(&self) -> Result<()> {
        self.persist(&self.accounts)
    }

    /// 把给定列表写盘（不修改内存），供 add/remove 做"先落盘、后提交"。
    fn persist(&self, accounts: &[Account]) -> Result<()> {
        let file = AccountsFile {
            schema_version: CURRENT_SCHEMA_VERSION,
            accounts: accounts.to_vec(),
        };
        let mut text = serde_json::to_string_pretty(&file).context("序列化 accounts.json 失败")?;
        text.push('\n');
        paths::write_atomic(&self.path, &text)
    }
}

/// `accounts.json` 的完整路径。
pub fn accounts_path(dir: &Path) -> PathBuf {
    dir.join(paths::ACCOUNTS_FILE_NAME)
}

/// 加载账号文件；必要时创建或修复。
///
/// 调用前应先用 [`paths::ensure_writable`] 确认目录可写。
pub fn load_or_create(dir: &Path) -> Result<(AccountsStore, AccountsLoadReport)> {
    let path = accounts_path(dir);
    let mut report = AccountsLoadReport::default();

    // 1) 首次运行：创建一个空文件，让用户能直观看到"数据存在哪"
    let Some(text) = paths::read_to_string_if_exists(&path)? else {
        let store = AccountsStore {
            path,
            accounts: Vec::new(),
        };
        store.save()?;
        report.created = true;
        log::info!("已创建默认账号文件：{}", store.path.display());
        return Ok((store, report));
    };

    // 2) 语法损坏：备份后用空列表重建（单个逗号不应该让程序无法启动）
    let mut file: AccountsFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(err) => {
            log::warn!("accounts.json 解析失败（{err}），将备份后用空列表重建");
            let backup = paths::quarantine_corrupt_file(&path)?;
            log::warn!("损坏的账号文件已备份为：{}", backup.display());
            report.recovered_from_corruption = true;

            let store = AccountsStore {
                path,
                accounts: Vec::new(),
            };
            store.save()?;
            return Ok((store, report));
        }
    };

    // 3) 版本与排序
    if file.schema_version != CURRENT_SCHEMA_VERSION {
        log::warn!(
            "accounts.json 的 schemaVersion 为 {}，当前程序支持 {}，将按当前版本写回",
            file.schema_version,
            CURRENT_SCHEMA_VERSION
        );
        file.schema_version = CURRENT_SCHEMA_VERSION;
    }

    report.invalid_created_at = sort_accounts(&mut file.accounts);
    if report.invalid_created_at {
        log::warn!("部分账号的 createdAt 无法解析，这些账号保持文件中的相对顺序");
    }

    Ok((
        AccountsStore {
            path,
            accounts: file.accounts,
        },
        report,
    ))
}

/// 按 `createdAt` 升序排序；返回是否发现无法解析的时间。
///
/// `sort_by` 是稳定排序：两个时间都无法解析时保持它们在文件里的相对顺序，
/// 这样即便用户手工改坏了时间，也不会出现"每次启动顺序都变"。
fn sort_accounts(accounts: &mut [Account]) -> bool {
    let invalid = accounts
        .iter()
        .any(|account| parse_time(&account.created_at).is_none());
    accounts.sort_by(|left, right| {
        match (parse_time(&left.created_at), parse_time(&right.created_at)) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => Ordering::Equal,
        }
    });
    invalid
}

/// 解析 RFC3339 时间；失败返回 `None`（调用方决定回退策略）。
fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

/// 当前时间的 RFC3339 表示（UTC）。
pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        // Rfc3339 格式化对本机时间不可能失败；真失败了也用 epoch 兜底，绝不 panic
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned())
}

/// 归一化实例地址。
///
/// * 只接受 `http` / `https`；
/// * 要求有主机名；
/// * 去掉路径末尾的斜杠（否则拼接 `/api/v1/auth/login` 会出现双斜杠）；
/// * 拒绝查询参数与锚点（它们对实例地址没有意义，容易掩盖粘贴错误）。
pub fn normalize_base_url(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("链接不能为空");
    }

    let mut parsed = url::Url::parse(trimmed)
        .with_context(|| format!("链接格式不正确（需要以 http:// 或 https:// 开头）：{trimmed}"))?;

    match parsed.scheme() {
        "http" | "https" => {}
        other => bail!("链接只支持 http/https，当前是 {other}"),
    }
    if parsed.host_str().is_none() {
        bail!("链接缺少主机名：{trimmed}");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        bail!("链接不能带查询参数（?）或锚点（#）");
    }

    // 路径末尾的斜杠统一去掉；子路径（例如反向代理下的 /sub2api）保留
    let path = parsed.path().trim_end_matches('/').to_owned();
    parsed.set_path(&path);

    // `Url::as_str` 对空路径会补一个 "/"，这里再统一收尾
    Ok(parsed.as_str().trim_end_matches('/').to_owned())
}

/// 邮箱格式校验（宽松版）。
///
/// 服务端用 `binding:"required,email"` 做最终裁决，这里只做"明显不是邮箱"的拦截，
/// 目的是在发请求前给用户即时反馈。规则：
/// 有且仅有本地部分与域名各一段、无空白、域名含点、顶级域至少两位字母。
pub fn is_valid_email(input: &str) -> bool {
    let email = input.trim();
    if email.is_empty() || email.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    if !domain.contains('.') || domain.starts_with('.') || domain.ends_with('.') {
        return false;
    }
    if domain.contains("..") {
        return false;
    }
    let tld = domain.rsplit('.').next().unwrap_or_default();
    tld.len() >= 2 && tld.chars().all(|ch| ch.is_ascii_alphabetic())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个测试账号；`created_at` 允许手工指定，便于测试排序。
    fn account(email: &str, base_url: &str, created_at: &str) -> Account {
        let mut account = Account::new(
            base_url.to_owned(),
            email.to_owned(),
            AccountRole::User,
            "enc:test:v1:opaque".to_owned(),
        );
        account.created_at = created_at.to_owned();
        account
    }

    #[test]
    fn 首次运行创建空文件() {
        let dir = tempfile::tempdir().unwrap();
        let (store, report) = load_or_create(dir.path()).unwrap();

        assert!(report.created);
        assert!(store.accounts().is_empty());

        let text = std::fs::read_to_string(accounts_path(dir.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["schemaVersion"], 1);
        assert_eq!(parsed["accounts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn 添加账号并重新加载后字段完整() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();

        let mut draft = account(
            "User@Example.com",
            "https://sub2api.example.com/",
            "2026-10-18T10:00:00Z",
        );
        draft.last_login_at = Some("2026-10-18T10:00:03Z".to_owned());
        store.add(draft).unwrap();

        let (reloaded, _) = load_or_create(dir.path()).unwrap();
        let accounts = reloaded.accounts();
        assert_eq!(accounts.len(), 1);
        assert_eq!(
            accounts[0].email, "User@Example.com",
            "邮箱原样保存，不在这里改写大小写"
        );
        assert_eq!(
            accounts[0].base_url, "https://sub2api.example.com",
            "尾部斜杠已去掉"
        );
        assert_eq!(accounts[0].role, AccountRole::User);
        assert_eq!(accounts[0].secrets, "enc:test:v1:opaque");
        assert_eq!(
            accounts[0].last_login_at.as_deref(),
            Some("2026-10-18T10:00:03Z")
        );
        assert!(accounts[0].api_keys.is_empty());
    }

    #[test]
    fn 重复账号被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();
        store
            .add(account(
                "user@example.com",
                "https://example.com",
                "2026-10-18T10:00:00Z",
            ))
            .unwrap();

        // 邮箱大小写不同 + 地址多一个尾斜杠，仍然算同一个账号
        let duplicate = account(
            "USER@example.com",
            "https://example.com/",
            "2026-10-18T11:00:00Z",
        );
        let err = store.add(duplicate).unwrap_err().to_string();

        assert!(err.contains("已存在"), "实际错误：{err}");
        assert_eq!(store.accounts().len(), 1);
    }

    #[test]
    fn 展示顺序按添加时间升序() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();

        // 故意乱序添加
        store
            .add(account(
                "c@example.com",
                "https://a.example.com",
                "2026-10-18T12:00:00Z",
            ))
            .unwrap();
        store
            .add(account(
                "a@example.com",
                "https://a.example.com",
                "2026-10-18T10:00:00Z",
            ))
            .unwrap();
        store
            .add(account(
                "b@example.com",
                "https://a.example.com",
                "2026-10-18T11:00:00Z",
            ))
            .unwrap();

        let order: Vec<_> = store
            .accounts()
            .iter()
            .map(|account| account.email.as_str())
            .collect();
        assert_eq!(order, ["a@example.com", "b@example.com", "c@example.com"]);

        // 重新加载后顺序不变
        let (reloaded, _) = load_or_create(dir.path()).unwrap();
        let order: Vec<_> = reloaded
            .accounts()
            .iter()
            .map(|account| account.email.as_str())
            .collect();
        assert_eq!(order, ["a@example.com", "b@example.com", "c@example.com"]);
    }

    #[test]
    fn 删除账号会落盘() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();
        store
            .add(account(
                "a@example.com",
                "https://example.com",
                "2026-10-18T10:00:00Z",
            ))
            .unwrap();
        let id = store.accounts()[0].id.clone();

        let removed = store.remove(&id).unwrap();

        assert!(removed.is_some());
        assert!(store.accounts().is_empty());
        let (reloaded, _) = load_or_create(dir.path()).unwrap();
        assert!(reloaded.accounts().is_empty());
    }

    #[test]
    fn 删除不存在的账号返回none() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();
        assert!(store.remove("not-a-real-id").unwrap().is_none());
    }

    #[test]
    fn 损坏文件被隔离并重建为空列表() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(accounts_path(dir.path()), "{broken json").unwrap();

        let (store, report) = load_or_create(dir.path()).unwrap();

        assert!(report.recovered_from_corruption);
        assert!(store.accounts().is_empty());

        let backups = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("accounts.json.corrupt-")
            })
            .count();
        assert_eq!(backups, 1);
    }

    #[test]
    fn 非法链接被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();

        for bad in [
            "",
            "not a url",
            "ftp://example.com",
            "https://example.com?x=1",
        ] {
            let result = store.add(account("a@example.com", bad, "2026-10-18T10:00:00Z"));
            assert!(result.is_err(), "应当拒绝：{bad}");
        }
        assert!(store.accounts().is_empty());
    }

    #[test]
    fn 非法邮箱被拒绝() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, _) = load_or_create(dir.path()).unwrap();

        for bad in ["", "not-an-email", "a@b", "a@@b.com", "a b@c.com"] {
            let result = store.add(account(bad, "https://example.com", "2026-10-18T10:00:00Z"));
            assert!(result.is_err(), "应当拒绝：{bad}");
        }
    }

    #[test]
    fn 链接归一化() {
        assert_eq!(
            normalize_base_url("https://Example.COM/").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            normalize_base_url("  http://127.0.0.1:8080/  ").unwrap(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            normalize_base_url("https://example.com/sub2api/").unwrap(),
            "https://example.com/sub2api"
        );
        assert!(normalize_base_url("ftp://example.com").is_err());
        assert!(normalize_base_url("example.com").is_err());
        assert!(normalize_base_url("https://example.com/#frag").is_err());
    }

    #[test]
    fn 无法解析的创建时间保持文件顺序() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{
  "schemaVersion": 1,
  "accounts": [
    {"id":"1","baseUrl":"https://a.example.com","email":"a@example.com","role":"user","secrets":"x","createdAt":"oops"},
    {"id":"2","baseUrl":"https://a.example.com","email":"b@example.com","role":"user","secrets":"y","createdAt":"2026-10-18T10:00:00Z"}
  ]
}"#;
        std::fs::write(accounts_path(dir.path()), text).unwrap();

        let (store, report) = load_or_create(dir.path()).unwrap();

        assert!(report.invalid_created_at);
        assert_eq!(store.accounts().len(), 2);
    }

    #[test]
    fn 预留的api键字段不会在往返中丢失() {
        let dir = tempfile::tempdir().unwrap();
        let text = r#"{
  "schemaVersion": 1,
  "accounts": [
    {
      "id":"1",
      "baseUrl":"https://a.example.com",
      "email":"a@example.com",
      "role":"admin",
      "secrets":"x",
      "createdAt":"2026-10-18T10:00:00Z",
      "apiKeys": [{"id":"k1","name":"默认"}]
    }
  ]
}"#;
        std::fs::write(accounts_path(dir.path()), text).unwrap();

        let (store, _) = load_or_create(dir.path()).unwrap();
        assert_eq!(store.accounts()[0].api_keys.len(), 1);

        store.save().unwrap();
        let text = std::fs::read_to_string(accounts_path(dir.path())).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["accounts"][0]["apiKeys"][0]["name"], "默认");
    }

    #[test]
    fn 时间格式化符合rfc3339() {
        let now = now_rfc3339();
        assert!(now.ends_with('Z'), "应当是 UTC 时间：{now}");
        assert!(parse_time(&now).is_some());
    }
}

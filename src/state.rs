//! 账号运行态：登录态、余额、订阅摘要与最近刷新时间。
//!
//! # 为什么单独一层
//!
//! `accounts.json` 里存的是**持久事实**（账号、加密凭据、创建时间）；
//! 而"现在在线吗、余额多少、几分钟前刷新的"是**易失状态**，既不该落盘，
//! 也不该和持久结构混在一起。把运行态放这里，好处是：
//!
//! * 刷新线程只写内存，不会因为网络抖动触发磁盘写；
//! * 托盘菜单与面板渲染都从这里读，保证两处显示一致；
//! * 账号被删除时只需 `remove` 一次，不会留下"幽灵状态"。
//!
//! 状态的生命周期与进程一致：重启后一律回到 [`AccountStatus::Unknown`]，
//! 由第一轮刷新重新判定。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::accounts::AccountRole;
use crate::api::SubscriptionSummary;

/// 账号的登录态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AccountStatus {
    /// 尚未刷新过（刚启动、刚添加）。
    #[default]
    Unknown,
    /// 正在请求。
    Refreshing,
    /// 最近一次请求成功。
    Online,
    /// 网络/服务端错误（token 可能仍然有效）。
    Offline,
    /// token 与密码都无法恢复，必须重新登录。
    CredentialsInvalid,
}

impl AccountStatus {
    /// 托盘菜单标题里展示的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "未知",
            Self::Refreshing => "刷新中",
            Self::Online => "在线",
            Self::Offline => "离线",
            Self::CredentialsInvalid => "凭据损坏",
        }
    }

    /// 是否算"在线"（只用于汇总计数与颜色）。
    pub fn is_online(self) -> bool {
        matches!(self, Self::Online)
    }
}

/// 单个账号的运行态。
#[derive(Debug, Clone)]
pub struct AccountRuntime {
    /// 登录态。
    pub status: AccountStatus,
    /// 最近一次成功刷新拿到的角色（可能与持久化时不同）。
    pub role: AccountRole,
    /// 余额（币种由账号 provider 决定）。
    pub balance: Option<f64>,
    /// DeepSeek 赠送/奖励余额。
    pub granted_balance: Option<f64>,
    /// DeepSeek 充值余额。
    pub topped_up_balance: Option<f64>,
    /// 订阅摘要。
    pub summary: Option<SubscriptionSummary>,
    /// 最近一次成功刷新的时刻。
    pub last_refreshed: Option<Instant>,
    /// 最近一次失败原因（界面提示用，不含凭据）。
    pub last_error: Option<String>,
}

impl Default for AccountRuntime {
    fn default() -> Self {
        Self {
            status: AccountStatus::Unknown,
            role: AccountRole::Unknown,
            balance: None,
            granted_balance: None,
            topped_up_balance: None,
            summary: None,
            last_refreshed: None,
            last_error: None,
        }
    }
}

impl AccountRuntime {
    /// 数据是否已经过期（从未刷新过也算过期）。
    pub fn is_stale(&self, now: Instant, stale_after: Duration) -> bool {
        match self.last_refreshed {
            Some(at) => now.saturating_duration_since(at) >= stale_after,
            None => true,
        }
    }
}

/// 所有账号的运行态表。
#[derive(Debug, Default)]
pub struct RuntimeTable {
    entries: HashMap<String, AccountRuntime>,
}

impl RuntimeTable {
    /// 读取某个账号的运行态。
    pub fn get(&self, id: &str) -> Option<&AccountRuntime> {
        self.entries.get(id)
    }

    /// 读取或创建某个账号的运行态。
    ///
    /// 用 `entry().or_default()` 的语义：刷新结果回来时账号可能刚被删除，
    /// 这时创建一条空状态也无害（随后会被 `remove` 清掉）。
    pub fn entry(&mut self, id: &str) -> &mut AccountRuntime {
        self.entries.entry(id.to_owned()).or_default()
    }

    /// 只更新状态（不清空已有的余额/摘要）。
    pub fn mark(&mut self, id: &str, status: AccountStatus) {
        self.entry(id).status = status;
    }

    /// 记录一次成功刷新。
    #[allow(
        clippy::too_many_arguments,
        reason = "刷新结果需要同时记录总额、赠送余额、充值余额与订阅摘要"
    )]
    pub fn apply_online(
        &mut self,
        id: &str,
        role: AccountRole,
        balance: Option<f64>,
        granted_balance: Option<f64>,
        topped_up_balance: Option<f64>,
        summary: Option<SubscriptionSummary>,
        now: Instant,
    ) {
        let entry = self.entry(id);
        entry.status = AccountStatus::Online;
        entry.role = role;
        entry.balance = balance;
        entry.granted_balance = granted_balance;
        entry.topped_up_balance = topped_up_balance;
        entry.summary = summary;
        entry.last_refreshed = Some(now);
        entry.last_error = None;
    }

    /// 记录一次失败（离线或凭据损坏）。
    pub fn apply_error(&mut self, id: &str, status: AccountStatus, message: impl Into<String>) {
        let entry = self.entry(id);
        entry.status = status;
        entry.last_error = Some(message.into());
        // 失败不改 last_refreshed：这样"数据过期"判断仍会触发下一轮重试
    }

    /// 账号被删除时清理运行态。
    pub fn remove(&mut self, id: &str) {
        self.entries.remove(id);
    }

    /// 在线账号数量（tooltip 汇总用）。
    pub fn online_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.status.is_online())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 初始状态为未知且视为过期() {
        let table = RuntimeTable::default();
        let now = Instant::now();
        assert!(table.get("missing").is_none());

        let mut table = RuntimeTable::default();
        let runtime = table.entry("a");
        assert_eq!(runtime.status, AccountStatus::Unknown);
        assert!(runtime.is_stale(now, Duration::from_secs(60)));
    }

    #[test]
    fn 成功刷新后变为在线并记录余额() {
        let mut table = RuntimeTable::default();
        let now = Instant::now();
        table.apply_online("a", AccountRole::Admin, Some(12.5), None, None, None, now);

        let runtime = table.get("a").expect("应当存在");
        assert_eq!(runtime.status, AccountStatus::Online);
        assert_eq!(runtime.role, AccountRole::Admin);
        assert_eq!(runtime.balance, Some(12.5));
        assert!(!runtime.is_stale(now, Duration::from_secs(60)));
        assert_eq!(table.online_count(), 1);
    }

    #[test]
    fn 失败不改动刷新时间以便下一轮重试() {
        let mut table = RuntimeTable::default();
        let now = Instant::now();
        table.apply_online("a", AccountRole::User, Some(1.0), None, None, None, now);
        table.apply_error("a", AccountStatus::Offline, "网络不可达");

        let runtime = table.get("a").unwrap();
        assert_eq!(runtime.status, AccountStatus::Offline);
        assert_eq!(runtime.last_error.as_deref(), Some("网络不可达"));
        assert_eq!(runtime.last_refreshed, Some(now), "失败不应刷新时间戳");
        assert_eq!(table.online_count(), 0);
    }

    #[test]
    fn 删除账号清理运行态() {
        let mut table = RuntimeTable::default();
        table.apply_online(
            "a",
            AccountRole::User,
            None,
            None,
            None,
            None,
            Instant::now(),
        );
        table.remove("a");
        assert!(table.get("a").is_none());
        assert_eq!(table.online_count(), 0);
    }

    #[test]
    fn 状态标签是中文() {
        assert_eq!(AccountStatus::Online.label(), "在线");
        assert_eq!(AccountStatus::CredentialsInvalid.label(), "凭据损坏");
    }
}

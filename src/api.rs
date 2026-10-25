//! 与 Sub2API 实例通信的最小 HTTP 客户端（登录、2FA、当前用户）。
//!
//! # 为什么单独一个模块
//!
//! 登录涉及"网络 + 协议信封 + 错误分类"三件事，如果塞进 `login.rs`（界面状态机），
//! 就没法对错误分类做单元测试。这里把**纯解析/分类函数**与**真正发请求的函数**分开：
//!
//! * `parse_*` 与 `classify_*`：纯函数，喂 JSON 字符串即可测试，不碰网络；
//! * [`HttpClient`] 的方法：负责发请求，再交给上面的纯函数解释结果。
//!
//! # 协议事实（已核对后端源码）
//!
//! | 用途 | 请求 | 成功响应 |
//! | --- | --- | --- |
//! | 登录 | `POST /api/v1/auth/login` `{email, password}` | `{code:0, data:{access_token, refresh_token, expires_in, ...}}` |
//! | 需要 2FA | 同上 | `{code:0, data:{requires_2fa:true, temp_token, user_email_masked}}` |
//! | 完成 2FA | `POST /api/v1/auth/login/2fa` `{temp_token, totp_code}` | 同登录成功 |
//! | 当前用户 | `GET /api/v1/auth/me` `Authorization: Bearer <token>` | `{code:0, data:{email, role, balance, ...}}` |
//!
//! 所有响应都套一层 `{code, message, data}` 信封；`code != 0` 即为业务错误。
//!
//! # 线程
//!
//! 本模块全部是**阻塞**调用（`ureq`）。调用方必须在后台线程里执行，
//! 不能在 egui 的 `logic()` / `ui()` 里直接调用，否则面板会卡住。

use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::accounts::AccountRole;

/// 登录路径。
pub const LOGIN_PATH: &str = "/api/v1/auth/login";

/// 2FA 登录路径。
pub const LOGIN_2FA_PATH: &str = "/api/v1/auth/login/2fa";

/// 当前用户路径。
pub const ME_PATH: &str = "/api/v1/auth/me";

/// 刷新 token 路径（后端限流 30 次/分钟，调用方要克制）。
pub const REFRESH_PATH: &str = "/api/v1/auth/refresh";

/// 订阅摘要路径。
pub const SUBSCRIPTION_SUMMARY_PATH: &str = "/api/v1/subscriptions/summary";

/// 当前用户的 API Key 列表路径（详情为 `{API_KEYS_PATH}/{id}`）。
pub const API_KEYS_PATH: &str = "/api/v1/keys";

/// 打码后的 API Key 摘要（只用于展示，绝不落完整值）。
#[derive(Debug, Clone, PartialEq)]
pub struct ApiKeyInfo {
    /// Key id（复制完整值时要用）。
    pub id: String,
    /// 名称。
    pub name: String,
    /// 打码后的值，例如 `sk-a…9f2c`。
    pub masked: String,
    /// 状态（active / disabled 等，原样保留）。
    pub status: String,
    /// 所属分组名。
    pub group: Option<String>,
    /// 最近使用时间（服务端原样字符串）。
    pub last_used_at: Option<String>,
    /// 总额度（美元）；服务端未设置时为 `None`。
    pub quota: Option<f64>,
    /// 已用额度（美元）。
    pub quota_used: Option<f64>,
    /// 近 5 小时用量（美元）。
    ///
    /// 后端目前只提供 5 小时 / 1 天 / 7 天三个窗口，**没有 1 小时口径**，
    /// 因此界面上展示"近 5 小时"并如实标注。
    pub usage_5h: Option<f64>,
    /// 近 1 天用量（美元）。
    pub usage_1d: Option<f64>,
    /// 近 7 天用量（美元）。
    pub usage_7d: Option<f64>,
    /// 5 小时限速额度（美元，0 表示未启用该窗口）。
    ///
    /// 服务端只有在 Key 配置了限速窗口时才累计 `usage_5h` 等用量；
    /// 界面需要靠该字段区分近期真的没有请求和根本没有启用该统计。
    pub rate_limit_5h: Option<f64>,
    /// 1 天限速额度（美元，0 表示未启用该窗口）。
    pub rate_limit_1d: Option<f64>,
    /// 7 天限速额度（美元，0 表示未启用该窗口）。
    pub rate_limit_7d: Option<f64>,
}

impl ApiKeyInfo {
    /// 从 `accounts.json` 的缓存条目还原（断网时展示用）。
    ///
    /// 缓存里只有打码值与元信息，所以还原出来的对象不能用于"复制完整 Key"，
    /// 调用方需要把 [`AccountKeysState::cached`] 标出来提示用户。
    pub fn from_cache_value(value: &serde_json::Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_owned();
        Some(Self {
            id,
            name: value
                .get("name")
                .and_then(|item| item.as_str())
                .unwrap_or("未命名")
                .to_owned(),
            masked: value
                .get("maskedKey")
                .and_then(|item| item.as_str())
                .unwrap_or("***")
                .to_owned(),
            status: value
                .get("status")
                .and_then(|item| item.as_str())
                .unwrap_or("unknown")
                .to_owned(),
            group: value
                .get("groupName")
                .and_then(|item| item.as_str())
                .map(ToOwned::to_owned),
            last_used_at: value
                .get("lastUsedAt")
                .and_then(|item| item.as_str())
                .map(ToOwned::to_owned),
            quota: value.get("quota").and_then(|item| item.as_f64()),
            quota_used: value.get("quotaUsed").and_then(|item| item.as_f64()),
            usage_5h: value.get("usage5h").and_then(|item| item.as_f64()),
            usage_1d: value.get("usage1d").and_then(|item| item.as_f64()),
            usage_7d: value.get("usage7d").and_then(|item| item.as_f64()),
            rate_limit_5h: value.get("rateLimit5h").and_then(|item| item.as_f64()),
            rate_limit_1d: value.get("rateLimit1d").and_then(|item| item.as_f64()),
            rate_limit_7d: value.get("rateLimit7d").and_then(|item| item.as_f64()),
        })
    }

    /// 转成写进 `accounts.json` 的缓存 JSON（**不含完整 Key**）。
    pub fn to_cache_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "name": self.name,
            "maskedKey": self.masked,
            "status": self.status,
            "groupName": self.group,
            "lastUsedAt": self.last_used_at,
            "quota": self.quota,
            "quotaUsed": self.quota_used,
            "usage5h": self.usage_5h,
            "usage1d": self.usage_1d,
            "usage7d": self.usage_7d,
            "rateLimit5h": self.rate_limit_5h,
            "rateLimit1d": self.rate_limit_1d,
            "rateLimit7d": self.rate_limit_7d,
            "fetchedAt": crate::accounts::now_rfc3339(),
        })
    }
}

/// 用户可绑定的 API Key 分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyGroup {
    /// 分组 id，创建 Key 时作为 `group_id` 提交。
    pub id: i64,
    /// 分组名称。
    pub name: String,
    /// 所属平台（可能为空，仅用于辅助展示）。
    pub platform: Option<String>,
}

/// 创建 API Key 成功后的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CreatedApiKey {
    /// 打码后的列表摘要。
    pub info: ApiKeyInfo,
    /// 完整 Key 值。只允许短暂留在内存/剪贴板，绝不写入缓存或日志。
    pub full_key: String,
    /// 客户端侧警告，例如创建成功但精确结束时间写入失败。
    pub warning: Option<String>,
}

/// 把 Key 打码成"前 4 + … + 后 4"。
///
/// 太短的 Key（≤8）只露首尾各 2 个字符，避免打码后反而泄漏大部分内容。
pub fn mask_api_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    let length = chars.len();
    if length == 0 {
        return "***".to_owned();
    }
    if length <= 8 {
        let head: String = chars.iter().take(2).collect();
        let tail: String = chars.iter().skip(length.saturating_sub(2)).collect();
        return format!("{head}…{tail}");
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[length - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// 从列表项里挑出第一个数值字段。
fn first_f64(item: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(number) = item.get(*key).and_then(|value| value.as_f64()) {
            return Some(number);
        }
    }
    None
}

/// 从列表项里挑出第一个非空字符串字段（也接受数字 id）。
fn first_string(item: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        let Some(value) = item.get(*key) else {
            continue;
        };

        if let Some(text) = value.as_str()
            && !text.is_empty()
        {
            return Some(text.to_owned());
        }
        // id 可能是数字
        if let Some(number) = value.as_i64() {
            return Some(number.to_string());
        }
    }
    None
}

/// 请求种类。错误分类需要它来区分"密码错"和"验证码错"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    /// 账号密码登录。
    Login,
    /// 6 位 TOTP 验证码登录。
    LoginTotp,
    /// 读取当前用户信息。
    Me,
}

/// 面向用户的错误分类。
///
/// 需求方确认按"三类提示"：连不上地址 / 凭据错误 / 频率限制；
/// 2FA、人机验证、非 Sub2API 地址这类特殊情况单独成类，避免把网络问题误报成密码错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    /// 连接层失败：DNS、连接被拒、超时、TLS 等。
    Network(String),
    /// 401/403（且不是验证码错误）：账号或密码不对。
    Unauthorized,
    /// 429：请求过于频繁。
    RateLimited,
    /// 地址存在但不像 Sub2API（404 或响应不是本协议的信封）。
    NotSub2Api,
    /// 服务端要求人机验证（Turnstile / 腾讯验证码）。
    CaptchaRequired,
    /// 2FA 验证码错误或已过期。
    TotpInvalid,
    /// 其它 4xx / 5xx。
    Server(String),
    /// 本地输入不合法（地址格式、邮箱格式等），根本没发请求。
    InvalidInput(String),
}

impl ApiError {
    /// 转换成弹窗里直接展示的中文文案。
    ///
    /// 文案与 `docs/plans/0003-account-login.md` 第 4.3 节的映射表一一对应。
    pub fn user_message(&self) -> String {
        match self {
            Self::Network(_) => "无法连接到该地址，请检查链接与网络".to_owned(),
            Self::Unauthorized => "账号或密码可能有误".to_owned(),
            Self::RateLimited => "请求过于频繁，请稍后再试".to_owned(),
            Self::NotSub2Api => "该地址不是有效的 Sub2API 实例".to_owned(),
            Self::CaptchaRequired => "该实例要求人机验证，请先在 Web 端登录或关闭验证".to_owned(),
            Self::TotpInvalid => "验证码不正确或已过期".to_owned(),
            Self::Server(detail) => format!("实例返回异常：{detail}"),
            Self::InvalidInput(detail) => detail.clone(),
        }
    }

    /// 详细的错误原因（写日志用，不进界面）。
    pub fn detail(&self) -> String {
        match self {
            Self::Network(detail) | Self::Server(detail) | Self::InvalidInput(detail) => {
                detail.clone()
            }
            other => format!("{other:?}"),
        }
    }
}

/// 登录成功后的 token 集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginTokens {
    /// access token。
    pub access_token: String,
    /// refresh token（后端可能为空）。
    pub refresh_token: String,
    /// access token 过期时间（RFC3339 UTC）；后端没给 `expires_in` 时为 `None`。
    pub expires_at: Option<String>,
}

/// 一次登录调用可能得到的两种结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOutcome {
    /// 直接拿到 token。
    Tokens(LoginTokens),
    /// 该账号开启了两步验证，需要用 `temp_token` + 6 位验证码继续。
    NeedsTotp {
        /// 2FA 会话令牌，过期后需要重新登录。
        temp_token: String,
        /// 后端已经打码过的邮箱（仅供提示展示）。
        masked_email: Option<String>,
    },
}

/// `/auth/me` 的关键信息。
#[derive(Debug, Clone, PartialEq)]
pub struct MeInfo {
    /// 角色，用于后续"管理员 / 普通用户"分岔展示。
    pub role: AccountRole,
    /// 服务端返回的邮箱（可能与用户输入的大小写不同）。
    pub email: Option<String>,
    /// 余额（美元）。后端一定返回，但为了容错用 `Option`。
    pub balance: Option<f64>,
    /// DeepSeek 赠送/奖励余额；Sub2API 为空。
    pub granted_balance: Option<f64>,
    /// DeepSeek 充值余额；Sub2API 为空。
    pub topped_up_balance: Option<f64>,
}

/// `/subscriptions/summary` 的摘要信息（0004 的面板展示用）。
///
/// 真实实例的 `data` 形如：
/// `{"active_count":0,"total_used_usd":0,"subscriptions":[]}`；
/// `subscriptions[]` 里每一项带有分组名与日/周/月用量，这里只提取"分组名列表"，
/// 面板展示用得上，字段再多就该单独做详情页了。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SubscriptionSummary {
    /// 生效中的订阅数量。
    pub active_count: u32,
    /// 这些订阅的累计已用金额（美元）。
    pub total_used_usd: f64,
    /// 订阅对应的分组名（去重后按出现顺序）。
    pub group_names: Vec<String>,
}

/// 套在外面的响应信封。
#[derive(Debug, Deserialize)]
struct Envelope {
    /// 业务码：后端可能返回数字，也可能返回 `"UNAUTHORIZED"` 这类字符串。
    #[serde(default)]
    code: serde_json::Value,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

impl Envelope {
    /// 把数字或字符串业务码映射成 HTTP 语义状态；None 表示成功。
    fn code_status(&self) -> Option<u16> {
        match &self.code {
            serde_json::Value::Null => None,
            serde_json::Value::Number(number) => number
                .as_i64()
                .and_then(|value| u16::try_from(value).ok())
                .filter(|value| *value != 0),
            serde_json::Value::String(text) => {
                let upper = text.trim().to_ascii_uppercase();
                if upper.is_empty() {
                    return None;
                }
                if let Ok(value) = upper.parse::<u16>() {
                    return (value != 0).then_some(value);
                }
                match upper.as_str() {
                    "OK" | "SUCCESS" | "NO_ERROR" => None,
                    "UNAUTHORIZED" | "UNAUTHENTICATED" | "TOKEN_EXPIRED" | "TOKEN_INVALID"
                    | "INVALID_TOKEN" | "AUTH_REQUIRED" => Some(401),
                    "FORBIDDEN" | "INSUFFICIENT_PERMISSIONS" => Some(403),
                    "NOT_FOUND" | "RESOURCE_NOT_FOUND" => Some(404),
                    "RATE_LIMITED" | "RATE_LIMIT_EXCEEDED" | "TOO_MANY_REQUESTS" => Some(429),
                    _ => Some(500),
                }
            }
            _ => Some(500),
        }
    }
}

/// 登录/2FA 接口的 `data` 字段。
#[derive(Debug, Default, Deserialize)]
struct LoginData {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<i64>,
    #[serde(default)]
    requires_2fa: bool,
    #[serde(default)]
    temp_token: Option<String>,
    #[serde(default)]
    user_email_masked: Option<String>,
}

/// 把状态码 + 信封 message 分类成 [`ApiError`]。
///
/// 判定顺序很重要：
///
/// 1. 401/403：2FA 请求下认为是验证码错误，其它请求认为是凭据错误；
/// 2. 429：频率限制；
/// 3. 404：多半是把别的服务当成了 Sub2API；
/// 4. message 里出现人机验证关键字：单独提示，避免用户反复试密码；
/// 5. 其余按服务端异常兜底。
pub fn classify_error(status: u16, message: &str, kind: RequestKind) -> ApiError {
    match status {
        401 | 403 => {
            if kind == RequestKind::LoginTotp {
                ApiError::TotpInvalid
            } else {
                ApiError::Unauthorized
            }
        }
        429 => ApiError::RateLimited,
        404 => ApiError::NotSub2Api,
        _ => {
            if looks_like_captcha(message) {
                ApiError::CaptchaRequired
            } else if status >= 500 {
                ApiError::Server(format!("HTTP {status}"))
            } else {
                // 4xx 通常带服务端原因，附上去便于排查；没有就只报状态码
                let trimmed = message.trim();
                if trimmed.is_empty() {
                    ApiError::Server(format!("HTTP {status}"))
                } else {
                    ApiError::Server(format!("HTTP {status}：{trimmed}"))
                }
            }
        }
    }
}

/// 判断服务端消息是否在说"人机验证"。
///
/// 只匹配英文关键字，避免把中文"验证码"（2FA 场景）误判成人机验证。
fn looks_like_captcha(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("captcha") || lower.contains("turnstile")
}

/// 从响应体里取出信封；不是本协议格式时按"不是 Sub2API"处理。
fn parse_envelope(body: &str) -> Result<Envelope, ApiError> {
    serde_json::from_str(body).map_err(|_| ApiError::NotSub2Api)
}

/// 读取信封并处理"HTTP 200 但 code != 0"的情况，返回 `data`。
fn read_data(status: u16, body: &str, kind: RequestKind) -> Result<serde_json::Value, ApiError> {
    let envelope = parse_envelope(body)?;
    let code_status = envelope.code_status();
    if status != 200 || code_status.is_some() {
        // 业务码可能同时出现在 HTTP 状态与 code 中；code 优先，因为代理可能改写状态码。
        let effective = code_status.unwrap_or(status);
        return Err(classify_error(effective, &envelope.message, kind));
    }
    Ok(envelope.data.unwrap_or(serde_json::Value::Null))
}

/// 纯函数：把登录接口的响应解释成 [`LoginOutcome`]。
///
/// 抽出来是为了能在没有网络的情况下覆盖"直接成功 / 需要 2FA / 各种错误"这几条分支。
pub fn parse_login_response(
    status: u16,
    body: &str,
    kind: RequestKind,
) -> Result<LoginOutcome, ApiError> {
    let data = read_data(status, body, kind)?;
    let login: LoginData = serde_json::from_value(data).map_err(|_| ApiError::NotSub2Api)?;

    if login.requires_2fa {
        let temp_token = login.temp_token.ok_or(ApiError::NotSub2Api)?;
        return Ok(LoginOutcome::NeedsTotp {
            temp_token,
            masked_email: login.user_email_masked,
        });
    }

    // 没有 requires_2fa 却没有 token：响应结构不对，按"不是 Sub2API"处理，避免误报登录成功
    if login.access_token.is_empty() {
        return Err(ApiError::NotSub2Api);
    }

    Ok(LoginOutcome::Tokens(LoginTokens {
        access_token: login.access_token,
        refresh_token: login.refresh_token,
        expires_at: login.expires_in.and_then(expires_at_from_now),
    }))
}

/// 纯函数：把 `/auth/me` 的响应解释成 [`MeInfo`]。
pub fn parse_me_response(status: u16, body: &str) -> Result<MeInfo, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let role = data
        .get("role")
        .and_then(|value| value.as_str())
        .map(role_from_str)
        .unwrap_or_default();
    let email = data
        .get("email")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let balance = data.get("balance").and_then(|value| value.as_f64());

    Ok(MeInfo {
        role,
        email,
        balance,
        granted_balance: None,
        topped_up_balance: None,
    })
}

/// 纯函数：解析 `/subscriptions/summary`。
///
/// 只提取面板需要的三项：生效订阅数、累计已用金额、分组名列表。
/// `subscriptions` 缺失或不是数组时按"没有订阅"处理，不报错——
/// 余额/登录态才是主信息，订阅只是锦上添花。
pub fn parse_subscription_summary(
    status: u16,
    body: &str,
) -> Result<SubscriptionSummary, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;

    let active_count = data
        .get("active_count")
        .and_then(|value| value.as_u64())
        .unwrap_or(0)
        .min(u64::from(u32::MAX)) as u32;
    let total_used_usd = data
        .get("total_used_usd")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);

    let mut group_names = Vec::new();
    if let Some(items) = data.get("subscriptions").and_then(|value| value.as_array()) {
        for item in items {
            // 用 let-chain 一次表达三个条件：有名字、非空、未出现过
            if let Some(name) = item.get("group_name").and_then(|value| value.as_str())
                && !name.is_empty()
                && !group_names.iter().any(|existing| existing == name)
            {
                group_names.push(name.to_owned());
            }
        }
    }

    Ok(SubscriptionSummary {
        active_count,
        total_used_usd,
        group_names,
    })
}

/// 角色字符串映射；未知值统一为 `Unknown`，不让新角色导致登录失败。
pub fn role_from_str(role: &str) -> AccountRole {
    match role.to_ascii_lowercase().as_str() {
        "admin" => AccountRole::Admin,
        "user" => AccountRole::User,
        _ => AccountRole::Unknown,
    }
}

/// 把"多少秒后过期"换算成 RFC3339 时间点。
///
/// 非法值（负数、0）返回 `None`：宁可不展示过期时间，也不要算出过去的时间点。
fn expires_at_from_now(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    let expires = OffsetDateTime::now_utc() + time::Duration::seconds(seconds);
    expires.format(&Rfc3339).ok()
}

/// 用量曲线接口路径（用户态，范围就是当前登录账号自己）。
pub const USAGE_TREND_PATH: &str = "/api/v1/usage/dashboard/trend";

/// 用量补零后的最大时间桶数量。
///
/// 当前 UI 最多查询 30 天；即使服务端误返回小时粒度，30 天也只有 720 个桶。
/// 留出余量后仍限制上限，避免异常响应让界面分配过大的向量。
const MAX_USAGE_BUCKETS: i64 = 2_048;

/// 用量曲线上的一个时间桶。
///
/// 服务端只返回有数据的时间桶（稀疏），展示前由 [`fill_usage_gaps`] 补齐。
#[derive(Debug, Clone, PartialEq)]
pub struct UsagePoint {
    /// 时间标签：小时粒度是 `2026-10-18 02:00`，天粒度是 `2026-10-18`。
    pub label: String,
    /// 该时间桶的请求数。
    pub requests: i64,
    /// 输入 token 数。
    pub input_tokens: i64,
    /// 输出 token 数。
    pub output_tokens: i64,
    /// Cache 写入 token 数。
    pub cache_creation_tokens: i64,
    /// Cache 读取 token 数。
    pub cache_read_tokens: i64,
    /// 该时间桶消耗的总 token 数。
    pub total_tokens: i64,
    /// 标准计费（美元）。
    pub cost: f64,
    /// 实际扣费（美元）。
    pub actual_cost: f64,
}

impl UsagePoint {
    /// 按网页端一致的口径计算 Cache 命中率百分比。
    ///
    /// 分母是全部 prompt token：输入 + Cache 写入 + Cache 读取。
    /// 没有 prompt token 时返回 0，避免除零。
    pub fn cache_hit_rate_percent(&self) -> f64 {
        let denominator = self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens;
        if denominator <= 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / denominator as f64 * 100.0
        }
    }
}

/// 用量接口返回的完整趋势数据。
///
/// `points` 保留服务端的稀疏时间桶；`start_date` / `end_date` / `granularity`
/// 用来在客户端补出完整时间轴。老版本服务端缺少这些字段时保留空字符串，
/// 调用方会退化为"按原始点绘制"。
#[derive(Debug, Clone, PartialEq)]
pub struct UsageTrend {
    /// 服务端返回的稀疏用量点。
    pub points: Vec<UsagePoint>,
    /// 查询起始日期，格式 `YYYY-MM-DD`。
    pub start_date: String,
    /// 查询结束日期，格式 `YYYY-MM-DD`。
    pub end_date: String,
    /// 聚合粒度：`hour` 或 `day`。
    pub granularity: String,
}

/// 拼接用量趋势请求地址。
///
/// 把 URL 构造拆成纯函数，方便单测确认 `api_key_id` 只在选择单个 Key 时出现；
/// Key id 来自服务端数字 id，不需要再做 URL 编码。
fn usage_trend_url(
    base_url: &str,
    start: &str,
    end: &str,
    granularity: &str,
    api_key_id: Option<&str>,
) -> String {
    let mut url = format!(
        "{base_url}{USAGE_TREND_PATH}?start_date={start}&end_date={end}&granularity={granularity}"
    );
    if let Some(api_key_id) = api_key_id {
        url.push_str("&api_key_id=");
        url.push_str(api_key_id);
    }
    url
}

/// 纯函数：解析 `GET /api/v1/usage/dashboard/trend`。
pub fn parse_usage_trend(status: u16, body: &str) -> Result<UsageTrend, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let items = data
        .get("trend")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    let points = items
        .iter()
        .map(|item| UsagePoint {
            label: item
                .get("date")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_owned(),
            requests: item
                .get("requests")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            input_tokens: item
                .get("input_tokens")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            output_tokens: item
                .get("output_tokens")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            cache_creation_tokens: item
                .get("cache_creation_tokens")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            cache_read_tokens: item
                .get("cache_read_tokens")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            total_tokens: item
                .get("total_tokens")
                .and_then(|value| value.as_i64())
                .unwrap_or(0),
            cost: item
                .get("cost")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0),
            actual_cost: item
                .get("actual_cost")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0),
        })
        .collect();

    // 兼容 snake_case 与 camelCase：不同版本后端返回字段名可能不同。
    let read_text = |names: &[&str]| -> String {
        names
            .iter()
            .find_map(|name| data.get(*name).and_then(|value| value.as_str()))
            .unwrap_or_default()
            .to_owned()
    };

    Ok(UsageTrend {
        points,
        start_date: read_text(&["start_date", "startDate"]),
        end_date: read_text(&["end_date", "endDate"]),
        granularity: read_text(&["granularity"]),
    })
}

/// 纯函数：把稀疏用量点补齐成完整时间轴，缺失桶记为 0。
///
/// 服务端只返回有流量的桶。直接按数组下标等距绘制会把相隔数小时甚至数天的
/// 两个点画成相邻点，历史趋势会被压缩。这里优先使用响应里的
/// `start_date` / `end_date` / `granularity` 生成完整桶：
///
/// * `hour`：范围内每天 00:00 到 23:00，共 24 个桶；
/// * `day`：范围内每天一个桶；
/// * 重复标签会合并 requests/total_tokens/actual_cost；
/// * 日期缺失或非法、粒度未知、桶数异常时，回退为按标签排序后的原始点。
pub fn fill_usage_gaps(trend: &UsageTrend) -> Vec<UsagePoint> {
    let merged = merge_usage_points(&trend.points);
    let fallback = || merged.values().cloned().collect::<Vec<_>>();
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let (Ok(start), Ok(end)) = (
        time::Date::parse(&trend.start_date, &format),
        time::Date::parse(&trend.end_date, &format),
    ) else {
        return fallback();
    };
    if start > end {
        return fallback();
    }

    let by_hour = match trend.granularity.as_str() {
        "hour" => true,
        "day" => false,
        _ => return fallback(),
    };
    let days = (end - start).whole_days() + 1;
    let bucket_count = if by_hour { days * 24 } else { days };
    if bucket_count <= 0 || bucket_count > MAX_USAGE_BUCKETS {
        return fallback();
    }

    let mut filled = Vec::with_capacity(bucket_count as usize);
    let mut day = start;
    while day <= end {
        let date_label = day.format(&format).unwrap_or_default();
        if by_hour {
            for hour in 0..24 {
                let label = format!("{date_label} {hour:02}:00");
                filled.push(merged.get(&label).cloned().unwrap_or(UsagePoint {
                    label,
                    requests: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                    total_tokens: 0,
                    cost: 0.0,
                    actual_cost: 0.0,
                }));
            }
        } else {
            filled.push(merged.get(&date_label).cloned().unwrap_or(UsagePoint {
                label: date_label,
                requests: 0,
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                total_tokens: 0,
                cost: 0.0,
                actual_cost: 0.0,
            }));
        }
        day += time::Duration::days(1);
    }
    filled
}

/// 把同一时间标签的重复桶合并，并保持标签顺序。
fn merge_usage_points(points: &[UsagePoint]) -> std::collections::BTreeMap<String, UsagePoint> {
    let mut merged = std::collections::BTreeMap::new();
    for point in points {
        if point.label.is_empty() {
            continue;
        }
        merged
            .entry(point.label.clone())
            .and_modify(|entry: &mut UsagePoint| {
                entry.requests += point.requests;
                entry.input_tokens += point.input_tokens;
                entry.output_tokens += point.output_tokens;
                entry.cache_creation_tokens += point.cache_creation_tokens;
                entry.cache_read_tokens += point.cache_read_tokens;
                entry.total_tokens += point.total_tokens;
                entry.cost += point.cost;
                entry.actual_cost += point.actual_cost;
            })
            .or_insert_with(|| point.clone());
    }
    merged
}

/// 纯函数：解析 `GET /api/v1/keys` 的列表。
///
/// 兼容几种字段写法（`masked_key` / `maskedKey` / `key`），因为不同版本后端
/// 返回的字段名不完全一致；拿不到打码值时用 [`mask_api_key`] 现算。
pub fn parse_api_keys(status: u16, body: &str) -> Result<Vec<ApiKeyInfo>, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let items = data
        .get("items")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(items.iter().map(parse_api_key_item).collect())
}

/// 纯函数：把单个 Key 的 JSON 对象解析成展示摘要。
///
/// 列表接口与额度更新接口返回的 Key 字段基本相同，因此抽成一个函数，
/// 避免两处字段兼容逻辑逐渐分叉。拿不到完整 `key` 时会按打码规则现算；
/// 服务端已经返回打码值时则原样保留，避免二次打码。
fn parse_api_key_item(item: &serde_json::Value) -> ApiKeyInfo {
    let raw_key = first_string(item, &["masked_key", "maskedKey", "key"]).unwrap_or_default();
    let masked = if raw_key.contains('…') || raw_key.contains('*') {
        raw_key
    } else {
        mask_api_key(&raw_key)
    };

    ApiKeyInfo {
        id: first_string(item, &["id"]).unwrap_or_default(),
        name: first_string(item, &["name"]).unwrap_or_else(|| "未命名".to_owned()),
        masked,
        status: first_string(item, &["status"]).unwrap_or_else(|| "unknown".to_owned()),
        group: first_string(item, &["group_name", "groupName"]).or_else(|| {
            item.get("group")
                .and_then(|group| group.get("name"))
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        }),
        last_used_at: first_string(item, &["last_used_at", "lastUsedAt"]),
        quota: first_f64(item, &["quota"]),
        quota_used: first_f64(item, &["quota_used", "quotaUsed"]),
        usage_5h: first_f64(item, &["usage_5h", "usage5h"]),
        usage_1d: first_f64(item, &["usage_1d", "usage1d"]),
        usage_7d: first_f64(item, &["usage_7d", "usage7d"]),
        rate_limit_5h: first_f64(item, &["rate_limit_5h", "rateLimit5h"]),
        rate_limit_1d: first_f64(item, &["rate_limit_1d", "rateLimit1d"]),
        rate_limit_7d: first_f64(item, &["rate_limit_7d", "rateLimit7d"]),
    }
}

/// 纯函数：解析 `PUT /api/v1/keys/{id}` 的响应。
///
/// 用户态额度更新接口返回更新后的 Key 对象；兼容少数版本把它包在
/// `data.api_key` 里的情况。缺少 id 或 data 不是对象时按协议异常处理，
/// 避免把错误响应当成更新成功写进本地缓存。
pub fn parse_api_key_update_response(status: u16, body: &str) -> Result<ApiKeyInfo, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let item = data.get("api_key").unwrap_or(&data);
    if !item.is_object() {
        return Err(ApiError::NotSub2Api);
    }
    let key = parse_api_key_item(item);
    if key.id.is_empty() {
        return Err(ApiError::NotSub2Api);
    }
    Ok(key)
}

/// 纯函数：解析 `DELETE /api/v1/keys/{id}` 的响应。
///
/// 删除成功的响应体不承载业务数据，只要求信封 `code == 0`；少数反向代理
/// 可能把成功改写成 204 No Content，因此 204 直接算成功，不强行解析 JSON。
pub fn parse_delete_api_key_response(status: u16, body: &str) -> Result<(), ApiError> {
    if status == 204 {
        return Ok(());
    }
    read_data(status, body, RequestKind::Me).map(|_| ())
}

/// 纯函数：解析 `GET /api/v1/groups/available`。
///
/// 新版本返回 `data: [ ... ]`，部分旧版本可能包一层 `{items:[...]}`，两者都兼容。
pub fn parse_available_groups(status: u16, body: &str) -> Result<Vec<ApiKeyGroup>, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let items = data
        .as_array()
        .cloned()
        .or_else(|| {
            data.get("items")
                .and_then(|value| value.as_array())
                .cloned()
        })
        .unwrap_or_default();

    Ok(items
        .iter()
        .filter_map(|item| {
            let id = item.get("id").and_then(|value| value.as_i64())?;
            let name = first_string(item, &["name"]).unwrap_or_else(|| format!("分组 {id}"));
            Some(ApiKeyGroup {
                id,
                name,
                platform: first_string(item, &["platform"]),
            })
        })
        .collect())
}

/// 纯函数：解析创建 API Key 的响应，同时保留完整 Key。
///
/// 完整值只在创建成功后返回一次，调用方必须复制到剪贴板/展示一次后丢弃，
/// 不能写进 `accounts.json`。若服务端只返回打码值，则按协议异常处理。
pub fn parse_created_api_key_response(status: u16, body: &str) -> Result<CreatedApiKey, ApiError> {
    let data = read_data(status, body, RequestKind::Me)?;
    let item = data.get("api_key").unwrap_or(&data);
    if !item.is_object() {
        return Err(ApiError::NotSub2Api);
    }
    let info = parse_api_key_item(item);
    if info.id.is_empty() {
        return Err(ApiError::NotSub2Api);
    }
    let full_key = item
        .get("key")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned();
    if full_key.is_empty() || full_key.contains('…') || full_key.contains('*') {
        return Err(ApiError::Server(
            "创建成功但响应没有返回完整 Key，请到 Web 后台复制".to_owned(),
        ));
    }
    Ok(CreatedApiKey {
        info,
        full_key,
        warning: None,
    })
}

/// 阻塞式 HTTP 客户端。
///
/// 每个实例绑定一个实例地址与超时；超时来自 `app.json` 的 `requestTimeoutSeconds`。
/// `ureq` 默认不跟随 POST 重定向（把正文一起转发不安全），因此遇到 307/308 会报
/// `RedirectFailed`，这里统一归类为网络错误。
pub struct HttpClient {
    base_url: String,
    agent: ureq::Agent,
}

impl HttpClient {
    /// 创建客户端；地址会先走 [`crate::accounts::normalize_base_url`] 归一化。
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, ApiError> {
        let normalized = crate::accounts::normalize_base_url(base_url)
            .map_err(|err| ApiError::InvalidInput(format!("{err}")))?;

        let config = ureq::Agent::config_builder()
            // 统一超时：连接、读、写都受它约束，避免登录卡住不返回
            .timeout_global(Some(timeout))
            // 自己看状态码：ureq 默认把 4xx/5xx 变成 Err，那样就拿不到响应体里的原因了
            .http_status_as_error(false)
            .build();

        Ok(Self {
            base_url: normalized,
            agent: config.new_agent(),
        })
    }

    /// 实际使用的实例地址（已归一化）。
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// 账号密码登录。
    pub fn login(&self, email: &str, password: &str) -> Result<LoginOutcome, ApiError> {
        let body = serde_json::json!({ "email": email, "password": password });
        let (status, text) = self.post_json(LOGIN_PATH, &body)?;
        parse_login_response(status, &text, RequestKind::Login)
    }

    /// 用 `temp_token` + 6 位验证码完成登录。
    pub fn login_totp(&self, temp_token: &str, totp_code: &str) -> Result<LoginOutcome, ApiError> {
        let body = serde_json::json!({ "temp_token": temp_token, "totp_code": totp_code });
        let (status, text) = self.post_json(LOGIN_2FA_PATH, &body)?;
        parse_login_response(status, &text, RequestKind::LoginTotp)
    }

    /// 读取当前用户信息（用于确定角色）。
    pub fn me(&self, access_token: &str) -> Result<MeInfo, ApiError> {
        let url = format!("{}{ME_PATH}", self.base_url);
        let mut response = self
            .agent
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(map_transport_error)?;
        let status = response.status().as_u16();
        // 原地读 body：ureq 3 的响应类型没有从 crate 根重导出，
        // 拆成独立函数就得额外依赖 `http` 包，不值得。
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        parse_me_response(status, &text)
    }

    /// 发一个 JSON POST，返回 `(状态码, 响应体文本)`。
    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<(u16, String), ApiError> {
        let url = format!("{}{path}", self.base_url);
        let payload = serde_json::to_string(body)
            .map_err(|err| ApiError::InvalidInput(format!("序列化请求失败：{err}")))?;

        let mut response = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .send(payload.as_str())
            .map_err(map_transport_error)?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            // 读 body 失败在 ureq 里也归到 `ureq::Error`，与连接错误同一类处理
            .map_err(map_transport_error)?;
        Ok((status, text))
    }

    /// 用 refresh token 续期，拿到新的 token 对。
    ///
    /// 响应结构与登录成功一致，因此复用 [`parse_login_response`]；
    /// 后端对 `/auth/refresh` 有 30 次/分钟限流，调用方失败后必须退避。
    pub fn refresh(&self, refresh_token: &str) -> Result<LoginTokens, ApiError> {
        let body = serde_json::json!({ "refresh_token": refresh_token });
        let (status, text) = self.post_json(REFRESH_PATH, &body)?;
        match parse_login_response(status, &text, RequestKind::Login)? {
            LoginOutcome::Tokens(tokens) => Ok(tokens),
            // refresh 接口不该返回"需要 2FA"，出现了就按地址/协议异常处理
            LoginOutcome::NeedsTotp { .. } => Err(ApiError::NotSub2Api),
        }
    }

    /// 读取订阅摘要（面板展示"余额 / 订阅"用）。
    pub fn subscription_summary(
        &self,
        access_token: &str,
    ) -> Result<SubscriptionSummary, ApiError> {
        let url = format!("{}{SUBSCRIPTION_SUMMARY_PATH}", self.base_url);
        let mut response = self
            .agent
            .get(&url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(map_transport_error)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        parse_subscription_summary(status, &text)
    }

    /// 列出当前账号的 API Key（打码摘要）。
    pub fn list_api_keys(&self, access_token: &str) -> Result<Vec<ApiKeyInfo>, ApiError> {
        let url = format!("{}{API_KEYS_PATH}", self.base_url);
        let (status, text) = self.get_json(&url, access_token)?;
        parse_api_keys(status, &text)
    }

    /// 拉取用量曲线（用户态接口，范围为当前账号自己）。
    ///
    /// `granularity` 取 `hour` 或 `day`；日期都是 `YYYY-MM-DD`。
    pub fn usage_trend(
        &self,
        access_token: &str,
        start: &str,
        end: &str,
        granularity: &str,
        api_key_id: Option<&str>,
    ) -> Result<UsageTrend, ApiError> {
        // 用户态接口会校验 Key 是否属于当前登录账号，因此只能在已登录账号的
        // access token 下附带该过滤条件，不能跨账号查询。
        let url = usage_trend_url(
            &self.base_url,
            start,
            end,
            granularity,
            api_key_id.filter(|value| !value.is_empty()),
        );
        let (status, text) = self.get_json(&url, access_token)?;
        parse_usage_trend(status, &text)
    }

    /// 列出当前用户可绑定的 API Key 分组。
    pub fn list_available_groups(&self, access_token: &str) -> Result<Vec<ApiKeyGroup>, ApiError> {
        let url = format!("{}/api/v1/groups/available", self.base_url);
        let (status, text) = self.get_json(&url, access_token)?;
        parse_available_groups(status, &text)
    }

    /// 创建 API Key。
    ///
    /// `quota = 0` 表示不限额；`group_id` 必须是当前用户可绑定的分组。
    /// 成功后返回完整 Key，只用于一次性展示/复制。
    pub fn create_api_key(
        &self,
        access_token: &str,
        name: &str,
        group_id: i64,
        quota: f64,
        expires_in_days: Option<u32>,
        expires_at: Option<&str>,
    ) -> Result<CreatedApiKey, ApiError> {
        if name.trim().is_empty() {
            return Err(ApiError::InvalidInput("API Key 名称不能为空".to_owned()));
        }
        if group_id <= 0 {
            return Err(ApiError::InvalidInput("请选择有效的分组".to_owned()));
        }
        if !quota.is_finite() || quota < 0.0 {
            return Err(ApiError::InvalidInput(
                "额度必须是大于或等于 0 的数字".to_owned(),
            ));
        }
        if matches!(expires_in_days, Some(0)) {
            return Err(ApiError::InvalidInput("有效期天数必须大于 0".to_owned()));
        }

        let mut body = serde_json::json!({
            "name": name.trim(),
            "group_id": group_id,
            "quota": quota,
        });
        if let Some(days) = expires_in_days {
            body["expires_in_days"] = serde_json::json!(days);
        }
        let (status, text) = self.post_json_auth(API_KEYS_PATH, access_token, &body)?;
        let mut created = parse_created_api_key_response(status, &text)?;

        // 创建接口只接受天数；需要精确到时间时，创建成功后再用 PUT 写入 expires_at。
        // 创建时已经带了按天向上取整的兜底有效期，因此即使第二次请求失败，Key 也不会变成永久。
        if let Some(expires_at) = expires_at {
            match self.update_api_key_expiry(access_token, &created.info.id, expires_at) {
                Ok(updated) => created.info = updated,
                Err(err) => {
                    created.warning = Some(format!(
                        "Key 已创建，但精确结束时间设置失败：{}",
                        err.user_message()
                    ));
                }
            }
        }
        Ok(created)
    }

    /// 更新单个 API Key 的精确过期时间。
    pub fn update_api_key_expiry(
        &self,
        access_token: &str,
        id: &str,
        expires_at: &str,
    ) -> Result<ApiKeyInfo, ApiError> {
        if id.is_empty() {
            return Err(ApiError::InvalidInput("API Key id 不能为空".to_owned()));
        }
        if expires_at.is_empty() {
            return Err(ApiError::InvalidInput("结束时间不能为空".to_owned()));
        }
        let path = format!("{API_KEYS_PATH}/{id}");
        let body = serde_json::json!({ "expires_at": expires_at });
        let (status, text) = self.put_json(&path, access_token, &body)?;
        parse_api_key_update_response(status, &text)
    }

    /// 删除一个 API Key。
    ///
    /// 这是不可恢复操作，调用方必须先做二次确认。接口只允许删除当前登录
    /// 账号自己的 Key；成功响应通常没有业务数据，失败仍按统一信封分类。
    pub fn delete_api_key(&self, access_token: &str, id: &str) -> Result<(), ApiError> {
        if id.is_empty() {
            return Err(ApiError::InvalidInput("API Key id 不能为空".to_owned()));
        }
        let path = format!("{API_KEYS_PATH}/{id}");
        let (status, text) = self.delete_json(&path, access_token)?;
        parse_delete_api_key_response(status, &text)
    }

    /// 调整一个 API Key 的额度，并可选地把已用额度清零。
    ///
    /// 这是用户态接口：只能修改当前 access token 所属账号自己的 Key。
    /// 传 `quota = 0` 表示不限额；`reset_quota = true` 会同时把 `quota_used`
    /// 归零，用于额度已耗尽但还想继续使用这个 Key 的场景。
    pub fn update_api_key_quota(
        &self,
        access_token: &str,
        id: &str,
        quota: f64,
        reset_quota: bool,
    ) -> Result<ApiKeyInfo, ApiError> {
        if id.is_empty() {
            return Err(ApiError::InvalidInput("API Key id 不能为空".to_owned()));
        }
        if !quota.is_finite() || quota < 0.0 {
            return Err(ApiError::InvalidInput(
                "额度必须是大于或等于 0 的数字".to_owned(),
            ));
        }

        let path = format!("{API_KEYS_PATH}/{id}");
        let body = serde_json::json!({
            "quota": quota,
            "reset_quota": reset_quota,
        });
        let (status, text) = self.put_json(&path, access_token, &body)?;
        parse_api_key_update_response(status, &text)
    }

    /// 取某个 Key 的完整值（只用于复制到剪贴板，绝不落盘）。
    ///
    /// 服务端若只返回打码值，这里按"不支持查看完整值"处理，不猜测。
    pub fn api_key_value(&self, access_token: &str, id: &str) -> Result<String, ApiError> {
        let url = format!("{}{API_KEYS_PATH}/{id}", self.base_url);
        let (status, text) = self.get_json(&url, access_token)?;
        let data = read_data(status, &text, RequestKind::Me)?;
        let key = data
            .get("key")
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        // 打码值（含 … 或 *）说明服务端不给完整值
        if key.is_empty() || key.contains('…') || key.contains('*') {
            return Err(ApiError::Server(
                "该 Key 不支持查看完整值，请到 Web 后台复制".to_owned(),
            ));
        }
        Ok(key.to_owned())
    }

    /// 发一个带 Bearer 的 DELETE，返回 `(状态码, 响应体文本)`。
    ///
    /// DELETE 没有请求正文，但仍要带 Authorization；保留独立函数让删除
    /// 路径的错误处理和认证头一眼可见，便于后续安全审查。
    fn delete_json(&self, path: &str, access_token: &str) -> Result<(u16, String), ApiError> {
        let url = format!("{}{path}", self.base_url);
        let mut response = self
            .agent
            .delete(&url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(map_transport_error)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        Ok((status, text))
    }

    /// 发一个带 Bearer 的 JSON POST，返回 `(状态码, 响应体文本)`。
    fn post_json_auth(
        &self,
        path: &str,
        access_token: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, String), ApiError> {
        let url = format!("{}{path}", self.base_url);
        let payload = serde_json::to_string(body)
            .map_err(|err| ApiError::InvalidInput(format!("序列化请求失败：{err}")))?;

        let mut response = self
            .agent
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .send(payload.as_str())
            .map_err(map_transport_error)?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        Ok((status, text))
    }

    /// 发一个带 Bearer 的 JSON PUT，返回 `(状态码, 响应体文本)`。
    ///
    /// 单独保留 PUT 而不是把 GET/POST 揉成通用请求构造器，是因为这个项目目前
    /// 只有额度更新需要带 JSON 正文的 PUT，简单直接的实现更容易审查。
    fn put_json(
        &self,
        path: &str,
        access_token: &str,
        body: &serde_json::Value,
    ) -> Result<(u16, String), ApiError> {
        let url = format!("{}{path}", self.base_url);
        let payload = serde_json::to_string(body)
            .map_err(|err| ApiError::InvalidInput(format!("序列化请求失败：{err}")))?;

        let mut response = self
            .agent
            .put(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .send(payload.as_str())
            .map_err(map_transport_error)?;

        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        Ok((status, text))
    }

    /// 发一个带 Bearer 的 GET，返回 `(状态码, 响应体文本)`。
    fn get_json(&self, url: &str, access_token: &str) -> Result<(u16, String), ApiError> {
        let mut response = self
            .agent
            .get(url)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(map_transport_error)?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(map_transport_error)?;
        Ok((status, text))
    }
}

/// 把 `ureq` 的传输层错误映射成 [`ApiError::Network`]。
///
/// 这里保留原始错误文本（写日志用），但界面统一显示"无法连接到该地址"，
/// 因为对普通用户来说，"DNS 解析失败"和"连接被拒绝"的下一步操作是一样的。
fn map_transport_error(err: ureq::Error) -> ApiError {
    ApiError::Network(format!("{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对"环境导致的临时网络失败"做有限重试（原因见 main.rs 的 test_support 说明）。
    ///
    /// 只重试 [`ApiError::Network`]；其它错误（解析、鉴权、断言）直接返回，
    /// 避免把真实缺陷掩盖成"偶发"。
    fn retry_network<T>(mut op: impl FnMut() -> Result<T, ApiError>) -> Result<T, ApiError> {
        let attempts = crate::test_support::RETRY_ATTEMPTS;
        let mut last_error = None;

        for attempt in 1..=attempts {
            match op() {
                Ok(value) => return Ok(value),
                Err(ApiError::Network(detail)) if attempt < attempts => {
                    eprintln!("临时网络错误（第 {attempt} 次尝试）：{detail}，稍后重试");
                    last_error = Some(ApiError::Network(detail));
                    std::thread::sleep(crate::test_support::RETRY_DELAY);
                }
                Err(err) => return Err(err),
            }
        }

        Err(last_error.unwrap_or(ApiError::Network("重试次数耗尽".to_owned())))
    }

    #[test]
    fn 登录成功解析出token与过期时间() {
        let body = r#"{"code":0,"message":"success","data":{
            "access_token":"a-token","refresh_token":"r-token","expires_in":86400,"token_type":"Bearer"
        }}"#;

        let outcome = parse_login_response(200, body, RequestKind::Login).unwrap();

        match outcome {
            LoginOutcome::Tokens(tokens) => {
                assert_eq!(tokens.access_token, "a-token");
                assert_eq!(tokens.refresh_token, "r-token");
                let expires = tokens.expires_at.expect("应当算出过期时间");
                assert!(
                    expires.ends_with('Z'),
                    "过期时间应为 RFC3339 UTC：{expires}"
                );
            }
            other => panic!("应当直接拿到 token，实际：{other:?}"),
        }
    }

    #[test]
    fn 需要两步验证时返回临时令牌() {
        let body = r#"{"code":0,"message":"success","data":{
            "requires_2fa":true,"temp_token":"temp-123","user_email_masked":"u***@e***.com"
        }}"#;

        let outcome = parse_login_response(200, body, RequestKind::Login).unwrap();

        assert_eq!(
            outcome,
            LoginOutcome::NeedsTotp {
                temp_token: "temp-123".to_owned(),
                masked_email: Some("u***@e***.com".to_owned()),
            }
        );
    }

    #[test]
    fn 密码错误归类为凭据错误() {
        let body = r#"{"code":401,"message":"invalid email or password"}"#;
        let err = parse_login_response(401, body, RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::Unauthorized);
        assert_eq!(err.user_message(), "账号或密码可能有误");
    }

    #[test]
    fn 频率限制单独提示() {
        let body = r#"{"code":429,"message":"too many requests"}"#;
        let err = parse_login_response(429, body, RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::RateLimited);
    }

    #[test]
    fn 非sub2api地址被识别() {
        let err =
            parse_login_response(404, "<html>not found</html>", RequestKind::Login).unwrap_err();
        // 404 优先按"地址不对"处理，比"响应格式不对"更好操作
        assert_eq!(err, ApiError::NotSub2Api);

        let err = parse_login_response(200, "<html>hello</html>", RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::NotSub2Api);
    }

    #[test]
    fn 人机验证有专门文案() {
        let body = r#"{"code":400,"message":"turnstile verification failed"}"#;
        let err = parse_login_response(400, body, RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::CaptchaRequired);
        assert!(err.user_message().contains("人机验证"));
    }

    #[test]
    fn 验证码错误与密码错误区分开() {
        let body = r#"{"code":401,"message":"invalid totp code"}"#;
        let err = parse_login_response(401, body, RequestKind::LoginTotp).unwrap_err();
        assert_eq!(err, ApiError::TotpInvalid);

        let err = parse_login_response(401, body, RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::Unauthorized, "密码请求仍应提示凭据错误");
    }

    #[test]
    fn 字符串错误码按_http_语义分类() {
        let err = read_data(
            401,
            r#"{"code":"UNAUTHORIZED","message":"Authorization header is required"}"#,
            RequestKind::Me,
        )
        .unwrap_err();
        assert_eq!(err, ApiError::Unauthorized);

        let expired = read_data(
            401,
            r#"{"code":"TOKEN_EXPIRED","message":"Token has expired"}"#,
            RequestKind::Me,
        )
        .unwrap_err();
        assert_eq!(expired, ApiError::Unauthorized);

        let data = read_data(
            200,
            r#"{"code":"OK","message":"success","data":{"email":"demo@example.com"}}"#,
            RequestKind::Me,
        )
        .unwrap();
        assert_eq!(data["email"], "demo@example.com");
    }

    #[test]
    fn 服务端异常带上状态码() {
        let body = r#"{"code":502,"message":"bad gateway"}"#;
        let err = parse_login_response(502, body, RequestKind::Login).unwrap_err();
        assert!(err.user_message().contains("HTTP 502"));
    }

    #[test]
    fn 缺少token视为地址不对() {
        let body = r#"{"code":0,"message":"success","data":{"requires_2fa":false}}"#;
        let err = parse_login_response(200, body, RequestKind::Login).unwrap_err();
        assert_eq!(err, ApiError::NotSub2Api);
    }

    /// 端到端验证：用本地 `TcpListener` 假扮 Sub2API，检查真实的 ureq 请求路径。
    ///
    /// 覆盖点：URL 拼接、`content-type` 头、JSON 信封解析、Bearer 头。
    /// 之所以不连真实例：单元测试不该依赖 PostgreSQL/Redis 与网络。
    #[test]
    #[ignore = "loopback 端到端用例：
需在验收时显式运行 cargo test -- --ignored（本机安全软件会偶发中止 loopback 连接）"]
    fn 本地假实例完成登录并读取角色() {
        // 与其它 loopback 用例串行，避免并行建连被本机安全软件打断（见 main.rs 说明）
        let _network_guard = crate::test_support::lock_network();
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取得端口");
        // 非阻塞 accept + 停止标志：客户端可能因连接池重试而多连一次，
        // "固定 accept 两次"会让服务端线程卡住、测试挂死。
        listener.set_nonblocking(true).expect("切换为非阻塞监听");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_in_server = std::sync::Arc::clone(&stop);
        // 记录收到的请求，测试末尾统一断言。
        //
        // 之前是"在服务端线程里 assert Bearer 头"：一旦断言失败，服务端线程直接 panic，
        // 客户端拿不到响应，失败信息变成"网络错误"，既难定位又偶发。
        // 现在服务端只负责回包，断言留给测试线程。
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests_in_server = std::sync::Arc::clone(&requests);

        let server = std::thread::spawn(move || {
            while !stop_in_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = crate::test_support::read_request(&mut stream);
                        if let Ok(mut log) = requests_in_server.lock() {
                            log.push(request.clone());
                        }
                        // 按路径分发，避免依赖请求到达顺序
                        let body = if request.starts_with("POST /api/v1/auth/login ") {
                            r#"{"code":0,"message":"success","data":{"access_token":"tok-1","refresh_token":"ref-1","expires_in":3600}}"#
                        } else {
                            r#"{"code":0,"message":"success","data":{"email":"user@example.com","role":"admin"}}"#
                        };

                        crate::test_support::write_json(&mut stream, "200 OK", body);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let base_url = format!("http://{addr}");
        // 每次调用用一个独立客户端：服务端响应后立即关闭连接，
        // 复用连接池会让测试依赖 ureq 的重试行为，徒增不确定性。
        let login_client =
            HttpClient::new(&base_url, Duration::from_secs(5)).expect("构造登录客户端");
        let outcome =
            retry_network(|| login_client.login("user@example.com", "password")).expect("登录成功");
        let tokens = match outcome {
            LoginOutcome::Tokens(tokens) => tokens,
            other => panic!("应当直接拿到 token，实际：{other:?}"),
        };
        assert_eq!(tokens.access_token, "tok-1");
        assert_eq!(tokens.refresh_token, "ref-1");

        let me_client =
            HttpClient::new(&base_url, Duration::from_secs(5)).expect("构造取用户客户端");
        let me = retry_network(|| me_client.me(&tokens.access_token)).expect("读取当前用户");
        assert_eq!(me.role, AccountRole::Admin);

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("假实例线程正常结束");

        // 断言：登录请求带了 content-type，取用户请求带了 Bearer
        let log = requests.lock().expect("请求日志锁");
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .contains("content-type: application/json")),
            "登录请求应当携带 content-type，实际：{log:?}"
        );
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-1")),
            "取用户请求应当携带 Bearer，实际：{log:?}"
        );
    }

    #[test]
    fn 解析额度更新响应() {
        let body = r#"{"code":0,"message":"success","data":{
            "id":12,"name":"默认 Key","key":"sk-1234567890abcdef","status":"active",
            "group":{"name":"默认分组"},"quota":20.5,"quota_used":3.25,
            "usage_5h":1.5,"usage_1d":2.0
        }}"#;
        let key = parse_api_key_update_response(200, body).expect("解析成功");
        assert_eq!(key.id, "12");
        assert_eq!(key.masked, "sk-1…cdef");
        assert_eq!(key.group.as_deref(), Some("默认分组"));
        assert_eq!(key.quota, Some(20.5));
        assert_eq!(key.quota_used, Some(3.25));
    }

    #[test]
    fn 额度更新响应兼容_api_key_包裹() {
        let body = r#"{"code":0,"message":"success","data":{
            "api_key":{"id":"9","name":"备用","masked_key":"sk-a…9f2c","quota":0}
        }}"#;
        let key = parse_api_key_update_response(200, body).expect("解析成功");
        assert_eq!(key.id, "9");
        assert_eq!(key.quota, Some(0.0));
    }

    #[test]
    fn 额度更新响应缺_id_视为协议异常() {
        let body = r#"{"code":0,"message":"success","data":{"name":"没有 id"}}"#;
        let err = parse_api_key_update_response(200, body).unwrap_err();
        assert_eq!(err, ApiError::NotSub2Api);
    }

    #[test]
    fn 额度更新接口错误沿用信封分类() {
        let body = r#"{"code":400,"message":"invalid quota"}"#;
        let err = parse_api_key_update_response(400, body).unwrap_err();
        assert!(matches!(err, ApiError::Server(detail) if detail.contains("invalid quota")));
    }

    /// 端到端验证额度更新的真实请求方法与 JSON 正文。
    ///
    /// 该用例依赖 loopback，按项目约定标记为 ignored，验收时显式运行。
    #[test]
    #[ignore = "loopback 端到端用例：
需在验收时显式运行 cargo test -- --ignored（本机安全软件会偶发中止 loopback 连接）"]
    fn 本地假实例收到额度更新请求() {
        let _network_guard = crate::test_support::lock_network();
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取得端口");
        listener.set_nonblocking(true).expect("切换为非阻塞监听");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_in_server = std::sync::Arc::clone(&stop);
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests_in_server = std::sync::Arc::clone(&requests);

        let server = std::thread::spawn(move || {
            while !stop_in_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = crate::test_support::read_request(&mut stream);
                        if let Ok(mut log) = requests_in_server.lock() {
                            log.push(request);
                        }
                        let body = r#"{"code":0,"message":"success","data":{
                            "id":12,"name":"默认 Key","masked_key":"sk-a…9f2c",
                            "status":"active","quota":12.5,"quota_used":0
                        }}"#;
                        crate::test_support::write_json(&mut stream, "200 OK", body);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let base_url = format!("http://{addr}");
        let client = HttpClient::new(&base_url, Duration::from_secs(5)).expect("构造客户端");
        let updated = retry_network(|| client.update_api_key_quota("tok-1", "12", 12.5, true))
            .expect("更新额度成功");
        assert_eq!(updated.id, "12");
        assert_eq!(updated.quota, Some(12.5));
        assert_eq!(updated.quota_used, Some(0.0));

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("假实例线程正常结束");

        let log = requests.lock().expect("请求日志锁");
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .starts_with("put /api/v1/keys/12 ")),
            "应当使用 PUT 并带上 Key id，实际：{log:?}"
        );
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-1")),
            "额度更新应当携带 Bearer，实际：{log:?}"
        );
        assert!(
            log.iter().any(|request| request.contains(r#""quota":12.5"#)
                && request.contains(r#""reset_quota":true"#)),
            "请求正文应当包含额度与清零标记，实际：{log:?}"
        );
    }

    #[test]
    fn 删除额度接口成功返回空数据() {
        let body =
            r#"{"code":0,"message":"success","data":{"message":"API key deleted successfully"}}"#;
        assert!(parse_delete_api_key_response(200, body).is_ok());
        assert!(parse_delete_api_key_response(204, "").is_ok());
    }

    #[test]
    fn 删除额度接口错误沿用信封分类() {
        let body = r#"{"code":404,"message":"api key not found"}"#;
        let err = parse_delete_api_key_response(404, body).unwrap_err();
        assert_eq!(err, ApiError::NotSub2Api);
    }

    /// 端到端验证删除 Key 的真实 HTTP 方法与认证头。
    ///
    /// 该用例只连本地假实例，不会删除真实 Key；依赖 loopback，按约定标记 ignored。
    #[test]
    #[ignore = "loopback 端到端用例：
需在验收时显式运行 cargo test -- --ignored（本机安全软件会偶发中止 loopback 连接）"]
    fn 本地假实例收到删除key请求() {
        let _network_guard = crate::test_support::lock_network();
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取得端口");
        listener.set_nonblocking(true).expect("切换为非阻塞监听");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_in_server = std::sync::Arc::clone(&stop);
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let requests_in_server = std::sync::Arc::clone(&requests);

        let server = std::thread::spawn(move || {
            while !stop_in_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = crate::test_support::read_request(&mut stream);
                        if let Ok(mut log) = requests_in_server.lock() {
                            log.push(request);
                        }
                        let body = r#"{"code":0,"message":"success","data":{"message":"deleted"}}"#;
                        crate::test_support::write_json(&mut stream, "200 OK", body);
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let base_url = format!("http://{addr}");
        let client = HttpClient::new(&base_url, Duration::from_secs(5)).expect("构造客户端");
        retry_network(|| client.delete_api_key("tok-1", "77")).expect("删除成功");

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("假实例线程正常结束");

        let log = requests.lock().expect("请求日志锁");
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .starts_with("delete /api/v1/keys/77 ")),
            "应当使用 DELETE 并带上 Key id，实际：{log:?}"
        );
        assert!(
            log.iter().any(|request| request
                .to_ascii_lowercase()
                .contains("authorization: bearer tok-1")),
            "删除请求应当携带 Bearer，实际：{log:?}"
        );
    }

    #[test]
    fn 解析可用分组() {
        let body = r#"{"code":0,"message":"success","data":[
            {"id":1,"name":"默认分组","platform":"openai"},
            {"id":"2","name":"备用分组","platform":"anthropic"}
        ]}"#;
        let groups = parse_available_groups(200, body).expect("解析成功");
        assert_eq!(groups.len(), 1, "缺少数值 id 的异常项应被忽略");
        assert_eq!(groups[0].id, 1);
        assert_eq!(groups[0].name, "默认分组");
        assert_eq!(groups[0].platform.as_deref(), Some("openai"));
    }

    #[test]
    fn 解析创建key响应并保留完整值() {
        let body = r#"{"code":0,"message":"success","data":{
            "id":100,"name":"新 Key","key":"sk-full-secret-value","status":"active",
            "group":{"name":"默认分组"},"quota":20,"quota_used":0
        }}"#;
        let created = parse_created_api_key_response(200, body).expect("解析成功");
        assert_eq!(created.info.id, "100");
        assert_eq!(created.info.masked, "sk-f…alue");
        assert_eq!(created.full_key, "sk-full-secret-value");
        assert!(created.warning.is_none());
    }

    #[test]
    fn 创建key响应缺少完整值视为异常() {
        let body = r#"{"code":0,"message":"success","data":{
            "id":100,"name":"新 Key","masked_key":"sk-f…alue","status":"active"
        }}"#;
        let err = parse_created_api_key_response(200, body).unwrap_err();
        assert!(matches!(err, ApiError::Server(message) if message.contains("Web 后台")));
    }

    #[test]
    fn 角色映射() {
        assert_eq!(role_from_str("admin"), AccountRole::Admin);
        assert_eq!(role_from_str("USER"), AccountRole::User);
        assert_eq!(role_from_str("superuser"), AccountRole::Unknown);
    }

    #[test]
    fn 当前用户解析角色邮箱与余额() {
        let body = r#"{"code":0,"message":"success","data":{
            "id":1,"email":"User@Example.com","role":"admin","balance":12.5
        }}"#;
        let me = parse_me_response(200, body).unwrap();
        assert_eq!(me.role, AccountRole::Admin);
        assert_eq!(me.email.as_deref(), Some("User@Example.com"));
        assert_eq!(me.balance, Some(12.5));
    }

    #[test]
    fn 订阅摘要解析出数量金额与分组名() {
        let body = r#"{"code":0,"message":"success","data":{
            "active_count":2,"total_used_usd":3.25,
            "subscriptions":[
                {"id":1,"group_name":"默认分组","status":"active"},
                {"id":2,"group_name":"默认分组","status":"active"},
                {"id":3,"group_name":"高速分组","status":"active"}
            ]
        }}"#;
        let summary = parse_subscription_summary(200, body).unwrap();
        assert_eq!(summary.active_count, 2);
        assert_eq!(summary.total_used_usd, 3.25);
        // 分组名去重且保持出现顺序
        assert_eq!(summary.group_names, vec!["默认分组", "高速分组"]);
    }

    #[test]
    fn 没有订阅时摘要为零值() {
        let body = r#"{"code":0,"message":"success","data":{
            "active_count":0,"total_used_usd":0,"subscriptions":[]
        }}"#;
        let summary = parse_subscription_summary(200, body).unwrap();
        assert_eq!(summary.active_count, 0);
        assert_eq!(summary.total_used_usd, 0.0);
        assert!(summary.group_names.is_empty());
    }

    #[test]
    fn 订阅接口返回错误时按错误分类() {
        let body = r#"{"code":401,"message":"unauthorized"}"#;
        let err = parse_subscription_summary(401, body).unwrap_err();
        assert_eq!(err, ApiError::Unauthorized);
    }

    #[test]
    fn 打码规则() {
        assert_eq!(mask_api_key("sk-abcdefghijklmnop"), "sk-a…mnop");
        assert_eq!(mask_api_key("short"), "sh…rt");
        assert_eq!(mask_api_key(""), "***");
        // 已是打码值时不再二次打码（由 parse 负责判断）
        assert!(mask_api_key("sk-1234567890abcdef").contains('…'));
    }

    #[test]
    fn 解析key列表并兼容字段差异() {
        let body = r#"{"code":0,"message":"success","data":{"items":[
            {"id":12,"name":"默认 Key","key":"sk-1234567890abcdef","status":"active","group_name":"默认分组","last_used_at":"2026-10-18T09:00:00Z"},
            {"id":"13","name":"备用","masked_key":"sk-a…9f2c","status":"disabled"}
        ],"total":2}}"#;
        let keys = parse_api_keys(200, body).expect("解析成功");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].id, "12", "数字 id 应转成字符串");
        assert_eq!(keys[0].masked, "sk-1…cdef");
        assert_eq!(keys[0].group.as_deref(), Some("默认分组"));
        assert_eq!(keys[1].masked, "sk-a…9f2c", "已打码的值原样保留");
        assert_eq!(keys[1].status, "disabled");
    }

    #[test]
    fn key摘要缓存不含完整值() {
        let info = ApiKeyInfo {
            id: "1".to_owned(),
            name: "默认".to_owned(),
            masked: "sk-a…9f2c".to_owned(),
            status: "active".to_owned(),
            group: Some("默认分组".to_owned()),
            last_used_at: None,
            quota: Some(10.0),
            quota_used: Some(2.5),
            usage_5h: Some(0.4),
            usage_1d: Some(1.2),
            usage_7d: Some(2.3),
            rate_limit_5h: Some(5.0),
            rate_limit_1d: None,
            rate_limit_7d: None,
        };
        let value = info.to_cache_value();
        assert_eq!(value["maskedKey"], "sk-a…9f2c");
        assert!(value.get("key").is_none(), "缓存里不能有完整 Key 字段");
        assert!(value["fetchedAt"].as_str().is_some());
        assert_eq!(value["quotaUsed"], 2.5);
        assert_eq!(value["usage5h"], 0.4);
    }

    #[test]
    fn 过期时间非法时不返回时间() {
        assert!(expires_at_from_now(0).is_none());
        assert!(expires_at_from_now(-5).is_none());
    }

    /// 手工验收：对**真实** Sub2API 实例做一次登录并读取角色。
    ///
    /// 默认忽略（`cargo test` 不会跑它），需要时显式执行：
    ///
    /// ```powershell
    /// $env:SUB2API_ACCEPTANCE_URL = "http://your-host:8080"
    /// $env:SUB2API_ACCEPTANCE_EMAIL = "you@example.com"
    /// $env:SUB2API_ACCEPTANCE_PASSWORD = "******"
    /// cargo test 真实实例 -- --ignored --nocapture
    /// ```
    ///
    /// 凭据只从环境变量读取，**不要**把真实账号写进代码或提交进仓库。
    #[test]
    #[ignore = "需要真实实例与凭据，见函数文档"]
    fn 真实实例登录并读取角色() {
        let (base_url, email, password) = acceptance_env();

        let client = HttpClient::new(&base_url, Duration::from_secs(15)).expect("构造客户端");
        let outcome = client.login(&email, &password).expect("真实实例登录失败");

        let tokens = match outcome {
            LoginOutcome::Tokens(tokens) => tokens,
            other => panic!("该账号需要额外验证步骤，当前用例不支持：{other:?}"),
        };
        assert!(!tokens.access_token.is_empty());

        let me = client
            .me(&tokens.access_token)
            .expect("读取真实实例的当前用户");
        assert_ne!(me.role, AccountRole::Unknown, "角色不应为未知");
        eprintln!(
            "真实实例验收通过：role={:?} email={:?} token_len={}",
            me.role,
            me.email,
            tokens.access_token.len()
        );
    }

    /// 从环境变量读取手工验收所需的地址与凭据。
    fn acceptance_env() -> (String, String, String) {
        let read = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("缺少环境变量 {name}（见用例文档）"))
        };
        (
            read("SUB2API_ACCEPTANCE_URL"),
            read("SUB2API_ACCEPTANCE_EMAIL"),
            read("SUB2API_ACCEPTANCE_PASSWORD"),
        )
    }

    /// 构造测试用量点，total_tokens 自动按后端口径求和。
    fn usage_point(
        label: &str,
        requests: i64,
        input_tokens: i64,
        output_tokens: i64,
        cache_creation_tokens: i64,
        cache_read_tokens: i64,
        actual_cost: f64,
    ) -> UsagePoint {
        UsagePoint {
            label: label.to_owned(),
            requests,
            input_tokens,
            output_tokens,
            cache_creation_tokens,
            cache_read_tokens,
            total_tokens: input_tokens + output_tokens + cache_creation_tokens + cache_read_tokens,
            cost: actual_cost,
            actual_cost,
        }
    }

    #[test]
    fn 解析用量曲线并保留完整指标() {
        let body = r#"{"code":0,"message":"success","data":{"trend":[
            {"date":"2026-10-18 02:00","requests":1,"input_tokens":500,"output_tokens":100,
             "cache_creation_tokens":200,"cache_read_tokens":1200,"total_tokens":2000,
             "cost":0.03,"actual_cost":0.0268},
            {"date":"2026-10-18 05:00","requests":3,"input_tokens":300,"output_tokens":100,
             "cache_creation_tokens":0,"cache_read_tokens":500,"total_tokens":900,
             "cost":0.02,"actual_cost":0.01}
        ],"start_date":"2026-10-17","end_date":"2026-10-18","granularity":"hour"}}"#;
        let trend = parse_usage_trend(200, body).expect("解析成功");
        assert_eq!(trend.points.len(), 2, "稀疏点应原样保留");
        assert_eq!(trend.points[0].label, "2026-10-18 02:00");
        assert_eq!(trend.points[0].input_tokens, 500);
        assert_eq!(trend.points[0].cache_read_tokens, 1200);
        assert_eq!(trend.points[0].total_tokens, 2000);
        assert_eq!(trend.points[1].requests, 3);
        assert_eq!(trend.start_date, "2026-10-17");
        assert_eq!(trend.end_date, "2026-10-18");
        assert_eq!(trend.granularity, "hour");
        assert!((trend.points[0].cache_hit_rate_percent() - 63.157_894).abs() < 0.001);
    }

    #[test]
    fn 分key查询只附带选中的api_key_id() {
        let all = usage_trend_url(
            "https://sub2api.example.com",
            "2026-10-18",
            "2026-10-19",
            "hour",
            None,
        );
        assert!(
            !all.contains("api_key_id"),
            "全部 Key 不应带过滤参数：{all}"
        );

        let one = usage_trend_url(
            "https://sub2api.example.com",
            "2026-10-18",
            "2026-10-19",
            "hour",
            Some("42"),
        );
        assert!(
            one.contains("api_key_id=42"),
            "单 Key 请求应带过滤参数：{one}"
        );
    }

    #[test]
    fn 按小时补齐缺失时间桶() {
        let trend = UsageTrend {
            points: vec![
                usage_point("2026-10-18 02:00", 1, 500, 100, 200, 1200, 0.0268),
                usage_point("2026-10-18 05:00", 3, 300, 100, 0, 500, 0.01),
            ],
            start_date: "2026-10-18".to_owned(),
            end_date: "2026-10-18".to_owned(),
            granularity: "hour".to_owned(),
        };

        let filled = fill_usage_gaps(&trend);
        assert_eq!(filled.len(), 24, "一天应有 24 个小时桶");
        assert_eq!(filled[2].total_tokens, 2000);
        assert_eq!(filled[5].requests, 3);
        assert_eq!(filled[3].total_tokens, 0, "缺失小时必须补零");
        assert_eq!(filled[4].input_tokens, 0);
    }

    #[test]
    fn 按天补齐并合并重复与乱序时间桶() {
        let trend = UsageTrend {
            points: vec![
                usage_point("2026-10-19", 2, 100, 50, 0, 50, 0.02),
                usage_point("2026-10-17", 1, 50, 20, 10, 20, 0.01),
                usage_point("2026-10-19", 3, 150, 70, 30, 50, 0.03),
            ],
            start_date: "2026-10-17".to_owned(),
            end_date: "2026-10-19".to_owned(),
            granularity: "day".to_owned(),
        };

        let filled = fill_usage_gaps(&trend);
        assert_eq!(filled.len(), 3);
        assert_eq!(filled[0].label, "2026-10-17");
        assert_eq!(filled[1].label, "2026-10-18");
        assert_eq!(filled[1].total_tokens, 0, "中间缺失日期必须补零");
        assert_eq!(filled[2].label, "2026-10-19");
        assert_eq!(filled[2].requests, 5, "重复桶应合并");
        assert_eq!(filled[2].input_tokens, 250);
        assert_eq!(filled[2].output_tokens, 120);
        assert_eq!(filled[2].cache_creation_tokens, 30);
        assert_eq!(filled[2].cache_read_tokens, 100);
        assert_eq!(filled[2].total_tokens, 500);
    }

    #[test]
    fn 空趋势在已知范围时补成零值时间轴() {
        let trend = UsageTrend {
            points: Vec::new(),
            start_date: "2026-10-18".to_owned(),
            end_date: "2026-10-18".to_owned(),
            granularity: "day".to_owned(),
        };
        let filled = fill_usage_gaps(&trend);
        assert_eq!(filled.len(), 1);
        assert_eq!(filled[0].label, "2026-10-18");
        assert_eq!(filled[0].total_tokens, 0);
        assert_eq!(filled[0].cache_hit_rate_percent(), 0.0);
    }

    #[test]
    fn 日期非法时回退为排序后的原始点() {
        let trend = UsageTrend {
            points: vec![
                usage_point("b", 2, 2, 0, 0, 0, 0.0),
                usage_point("a", 1, 1, 0, 0, 0, 0.0),
            ],
            start_date: "not-a-date".to_owned(),
            end_date: String::new(),
            granularity: "day".to_owned(),
        };
        let filled = fill_usage_gaps(&trend);
        assert_eq!(filled.len(), 2);
        assert_eq!(filled[0].label, "a");
        assert_eq!(filled[1].label, "b");
    }

    #[test]
    fn 命中率按网页端口径计算() {
        let point = usage_point("2026-10-18", 1, 500, 100, 0, 1500, 0.01);
        assert!((point.cache_hit_rate_percent() - 75.0).abs() < f64::EPSILON);

        let anthropic = usage_point("2026-10-18", 1, 200, 50, 300, 500, 0.01);
        assert!((anthropic.cache_hit_rate_percent() - 50.0).abs() < f64::EPSILON);

        let empty = usage_point("2026-10-18", 0, 0, 0, 0, 0, 0.0);
        assert_eq!(empty.cache_hit_rate_percent(), 0.0);
    }
}

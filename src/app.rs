//! 托盘面板窗口：显示 / 隐藏 / 定位 / 绘制。
//!
//! 面板是一个**无边框、置顶、不在任务栏显示**的小窗口，行为对标常见的菜单栏工具：
//!
//! * 点击托盘图标 → 在图标旁边弹出面板；
//! * 点击面板外部（窗口失焦）→ 自动收起；
//! * 按 `Esc` → 收起；
//! * 右键托盘菜单里的“退出” → 结束进程。
//!
//! 本文件同时承担“状态机”和“界面绘制”两件事，因为面板目前非常简单；
//! 等账户列表、设置页加进来之后，绘制部分可以整体挪到 `ui` 模块，届时只需搬代码。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use crate::accounts::{self, Account, AccountProvider};
use crate::data::DataLayer;
use crate::fonts;
use crate::login::{LoginDialog, LoginDraft};
use crate::refresh::{self, RefreshOutcome, RoundResult};
use crate::state::{AccountStatus, RuntimeTable};
use crate::tray::{TrayBridge, TrayCommand, TrayHandle};

/// 面板逻辑宽度（单位：egui 点；100% 缩放下等于像素）。
pub const PANEL_WIDTH: f32 = 360.0;

/// 面板逻辑高度（单位：egui 点）。
pub const PANEL_HEIGHT: f32 = 460.0;

/// 面板可缩放时的最小尺寸（逻辑点）：再小就放不下标题条与一张卡片。
pub const MIN_PANEL_WIDTH: f32 = 320.0;

/// 见 [`MIN_PANEL_WIDTH`]。
pub const MIN_PANEL_HEIGHT: f32 = 240.0;

/// 面板与托盘图标之间留出的空隙，避免气泡紧贴图标。
const TRAY_GAP: f32 = 8.0;

/// 面板与屏幕边缘之间保留的最小距离。
const SCREEN_MARGIN: f32 = 8.0;

/// 面板刚弹出后的“宽限期”。
///
/// 窗口从“显示”到“真正拿到焦点”之间有一两帧延迟，这段时间里
/// `viewport().focused` 可能仍然是 `false`。如果立刻按失焦处理，面板会刚弹出就消失，
/// 所以在这段时间内不响应失焦事件。
const FOCUS_GRACE: Duration = Duration::from_millis(400);

/// 收起面板后的去抖窗口。
///
/// 用户点击托盘图标关闭面板时，同一动作会先让面板失焦（触发自动收起），
/// 再派发一次托盘单击事件（触发“切换显示”）。若不去抖，面板会关掉又立刻弹回来。
const TOGGLE_DEBOUNCE: Duration = Duration::from_millis(250);

use crate::ui_style::{self, ButtonSize, ButtonTone, font, usage_font};
pub(crate) use crate::ui_style::{apply_egui_visuals, lerp_color, set_palette, theme};

/// 复制结果：(账号 id, Key id, 完整值或错误文案)。
type CopyReply = (String, String, Result<String, String>);

/// 用量曲线的区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UsageRange {
    /// 最近 24 小时（按小时聚合）。
    Day,
    /// 最近 7 天（按天聚合）。
    Week,
    /// 最近 30 天（按天聚合）。
    Month,
}

impl UsageRange {
    /// 依次排列的区间，用于按钮组。
    const ALL: [UsageRange; 3] = [UsageRange::Day, UsageRange::Week, UsageRange::Month];

    /// 按钮上的中文标签。
    fn label(self) -> &'static str {
        match self {
            UsageRange::Day => "24 小时",
            UsageRange::Week => "7 天",
            UsageRange::Month => "30 天",
        }
    }

    /// 服务端的聚合粒度。
    fn granularity(self) -> &'static str {
        match self {
            UsageRange::Day => "hour",
            UsageRange::Week | UsageRange::Month => "day",
        }
    }

    /// 回溯天数（24 小时算 1 天）。
    fn back_days(self) -> i64 {
        match self {
            UsageRange::Day => 1,
            UsageRange::Week => 7,
            UsageRange::Month => 30,
        }
    }

    /// 给接口用的 `(start_date, end_date)`，格式 `YYYY-MM-DD`。
    ///
    /// 用 UTC 日期：服务端按自己的时区切桶，跨时区时边界可能差一天，
    /// 但曲线形状不受影响，不值得为此引入时区库。
    fn dates(self) -> (String, String) {
        let format = time::macros::format_description!("[year]-[month]-[day]");
        let today = time::OffsetDateTime::now_utc().date();
        let start = today - time::Duration::days(self.back_days());
        (
            start.format(&format).unwrap_or_default(),
            today.format(&format).unwrap_or_default(),
        )
    }
}

/// 用量曲线的纵轴指标。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum UsageMetric {
    /// 输入 + 输出 + Cache 写读的总 token。
    Total,
    /// 输入 token。
    Input,
    /// 输出 token。
    Output,
    /// Cache 命中率（0 到 100）。
    CacheHitRate,
}

impl UsageMetric {
    /// 指标切换按钮的排列顺序。
    const ALL: [UsageMetric; 4] = [
        UsageMetric::Total,
        UsageMetric::Input,
        UsageMetric::Output,
        UsageMetric::CacheHitRate,
    ];

    /// 按钮标签。
    fn label(self) -> &'static str {
        match self {
            UsageMetric::Total => "总 Token",
            UsageMetric::Input => "输入",
            UsageMetric::Output => "输出",
            UsageMetric::CacheHitRate => "命中率",
        }
    }

    /// 某个时间桶在该指标上的纵轴值。
    fn value(self, point: &crate::api::UsagePoint) -> f64 {
        match self {
            UsageMetric::Total => point.total_tokens as f64,
            UsageMetric::Input => point.input_tokens as f64,
            UsageMetric::Output => point.output_tokens as f64,
            UsageMetric::CacheHitRate => point.cache_hit_rate_percent(),
        }
    }

    /// 把指标值格式化成适合悬浮提示/摘要的文字。
    fn format(self, value: f64) -> String {
        match self {
            UsageMetric::CacheHitRate => format!("{value:.1}%"),
            UsageMetric::Total | UsageMetric::Input | UsageMetric::Output => {
                fmt_tokens(value.round() as i64)
            }
        }
    }

    /// 该指标的纵轴上限。命中率固定 0 到 100，其余按数据最大值。
    fn axis_max(self, points: &[crate::api::UsagePoint]) -> f32 {
        match self {
            UsageMetric::CacheHitRate => 100.0,
            _ => points
                .iter()
                .map(|point| self.value(point))
                .fold(0.0_f64, f64::max)
                .max(1.0) as f32,
        }
    }
}

/// 用量缓存键：账号 id + 区间 + 选中的 API Key（`None` 表示全部 Key）。
type UsageCacheKey = (String, UsageRange, Option<String>);

/// 绘制阶段收集的用量区交互，交给 `logic()` 统一处理。
#[derive(Debug, Default)]
struct UsageUiActions {
    /// 切换时间区间。
    range: Option<(String, UsageRange)>,
    /// 切换 API Key；第二个元素为 `None` 表示全部 Key。
    api_key: Option<(String, Option<String>)>,
    /// 切换图表指标。
    metric: Option<(String, UsageMetric)>,
    /// 展开/收起整个用量区。
    toggle_collapse: Option<String>,
}

/// 用量区汇总指标。
#[derive(Debug, Clone, Copy, Default)]
struct UsageTotals {
    /// 请求总数。
    requests: i64,
    /// 输入 token 总数。
    input_tokens: i64,
    /// 输出 token 总数。
    output_tokens: i64,
    /// Cache 写入 token 总数。
    cache_creation_tokens: i64,
    /// Cache 读取 token 总数。
    cache_read_tokens: i64,
    /// 总 token 数。
    total_tokens: i64,
    /// 标准计费。
    cost: f64,
    /// 实际花费。
    actual_cost: f64,
}

impl UsageTotals {
    /// 汇总一组时间桶。
    fn from_points(points: &[crate::api::UsagePoint]) -> Self {
        let mut totals = Self::default();
        for point in points {
            totals.requests += point.requests;
            totals.input_tokens += point.input_tokens;
            totals.output_tokens += point.output_tokens;
            totals.cache_creation_tokens += point.cache_creation_tokens;
            totals.cache_read_tokens += point.cache_read_tokens;
            totals.total_tokens += point.total_tokens;
            totals.cost += point.cost;
            totals.actual_cost += point.actual_cost;
        }
        totals
    }

    /// 与网页端一致的 Cache 命中率百分比。
    fn cache_hit_rate_percent(&self) -> f64 {
        let denominator = self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens;
        if denominator <= 0 {
            0.0
        } else {
            self.cache_read_tokens as f64 / denominator as f64 * 100.0
        }
    }
}

/// 用量拉取结果：账号 id、区间、API Key、结果通道。
type UsageReply = (
    String,
    UsageRange,
    Option<String>,
    Receiver<Result<crate::api::UsageTrend, String>>,
);

/// 某个账号在某个区间下的用量曲线缓存。
#[derive(Debug, Clone, Default)]
struct UsageSeries {
    /// 已按真实时间轴补齐的时间桶（缺失桶为 0）。
    points: Vec<crate::api::UsagePoint>,
    /// 是否正在拉取。
    loading: bool,
    /// 错误文案（可直接展示）。
    error: Option<String>,
    /// 上次成功拉取的时刻，用于 TTL 判断。
    fetched_at: Option<Instant>,
}

/// 设置页的编辑草稿：先在本地改，点"保存"才写盘并生效。
#[derive(Debug, Clone)]
struct SettingsDraft {
    /// 账号名是否打码。
    mask_accounts: bool,
    /// 是否开机自启。
    autostart: bool,
    /// 状态刷新间隔（秒）。
    refresh_interval_seconds: u64,
    /// 单次请求超时（秒）。
    request_timeout_seconds: u64,
    dark_mode: bool,
}

/// 后台保存设置的结果：配置写盘成功后由 UI 线程应用并关闭弹窗。
#[derive(Debug)]
struct SettingsSaveResult {
    /// 已保存的配置。
    config: crate::config::AppConfig,
    /// 保存后的开机自启状态。
    autostart: bool,
}

/// 悬浮球主目标：账号余额或某个 API Key 的剩余额度。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FloatingBallTarget {
    /// 账号 id。
    account_id: String,
    /// Key id；None 表示展示账号余额。
    key_id: Option<String>,
}

/// 悬浮球当前要展示的内容。
#[derive(Debug, Clone)]
struct FloatingBallView {
    /// 正中心大字。
    text: String,
    /// 账号状态色。
    status: AccountStatus,
}

/// 悬浮球主 Key 的后台刷新周期。
const FLOATING_BALL_KEY_REFRESH_TTL: Duration = Duration::from_secs(60);

/// 悬浮动画状态：按 key 记录"当前淡入进度"与"上一帧的命中矩形"。
///
/// 为什么不用 egui 自带的 `animate_bool`：它要求稳定的 `Id`，而这里的行是按
/// 账号/Key 动态生成的；自己存一张表更直观，也方便统一调时间常数。
/// 效果是约 90ms 的指数式淡入/淡出（接近系统菜单的高亮过渡），不会"闪一下"。
#[derive(Default)]
struct HoverState {
    anim: HashMap<String, f32>,
    rects: HashMap<String, egui::Rect>,
}

impl HoverState {
    /// 取某个元素的当前悬浮进度（0.0～1.0），并根据上一帧矩形判断是否悬浮。
    fn t(&mut self, ctx: &egui::Context, key: &str) -> f32 {
        let pointer = ctx.pointer_hover_pos();
        let hovered = match (pointer, self.rects.get(key)) {
            (Some(position), Some(rect)) => rect.contains(position),
            _ => false,
        };
        let target = if hovered { 1.0 } else { 0.0 };

        // stable_dt 在 60fps 下约 0.016；夹住上限避免掉帧时突跳
        let dt = ctx.input(|input| input.stable_dt).clamp(0.001, 0.05);
        let current = self.anim.entry(key.to_owned()).or_insert(target);
        let step = (dt / 0.09).min(1.0);
        *current += (target - *current) * step;
        if (*current - target).abs() < 0.01 {
            *current = target;
        } else {
            // 动画未结束：主动排下一帧，否则按需重绘会让过渡卡住
            ctx.request_repaint();
        }
        *current
    }

    /// 记录元素本帧的矩形，供下一帧判断悬浮。
    fn store(&mut self, key: &str, rect: egui::Rect) {
        self.rects.insert(key.to_owned(), rect);
    }
}

/// 操作成功提示在面板底部的停留时长。
const TOAST_DURATION: Duration = Duration::from_secs(4);

/// 启动后延迟多久做第一轮状态刷新。
///
/// 不立刻刷新有两个原因：托盘图标与窗口还没完全就绪；用户刚启动就看到一堆请求也没必要。
const FIRST_REFRESH_DELAY: Duration = Duration::from_secs(5);

/// 面板打开时，数据超过这个时长就补刷一次。
const STALE_AFTER: Duration = Duration::from_secs(60);

/// 把用户输入的额度字符串解析成美元金额。
///
/// 单独抽成纯函数，方便在无界面环境下覆盖空值、负数与 NaN 等输入。
/// 后端把 0 解释为不限额，因此这里允许 0，但拒绝负数和无穷大。
fn parse_quota_amount(input: &str) -> Result<f64, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("请输入额度金额，0 表示不限额".to_owned());
    }
    let value = trimmed
        .parse::<f64>()
        .map_err(|_| "额度必须是有效数字".to_owned())?;
    if !value.is_finite() || value < 0.0 {
        return Err("额度必须是大于或等于 0 的有效数字".to_owned());
    }
    Ok(value)
}

/// 解析创建 Key 时输入的有效期天数。
///
/// 空字符串或 0 表示永久有效；其余必须是大于 0 的整数。
fn parse_expires_days(input: &str) -> Result<Option<u32>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == "0" {
        return Ok(None);
    }
    let days = trimmed
        .parse::<u32>()
        .map_err(|_| "有效期必须是大于 0 的整数天".to_owned())?;
    if days == 0 {
        return Ok(None);
    }
    Ok(Some(days))
}

/// 把日期选择器与小时/分钟组合成本地时区的精确结束时间。
fn selected_expiry_zoned(state: &CreateKeyState) -> Result<jiff::Zoned, String> {
    let date = state.expires_date;
    let datetime = jiff::civil::DateTime::new(
        date.year(),
        date.month(),
        date.day(),
        state.expires_hour,
        state.expires_minute,
        0,
        0,
    )
    .map_err(|_| "结束日期或时间无效".to_owned())?;
    jiff::tz::TimeZone::system()
        .to_zoned(datetime)
        .map_err(|err| format!("无法解析本地时区：{err}"))
}

/// 把选择的时间格式化成界面展示文案。
fn selected_expiry_label(state: &CreateKeyState) -> String {
    format!(
        "{} {:02}:{:02}",
        state.expires_date, state.expires_hour, state.expires_minute
    )
}

/// 把当前额度格式化成输入框初值。
/// 把当前额度格式化成输入框初值。
///
/// 保留最多 4 位小数并裁掉末尾的 0，避免用户看到 `10.0000` 这种噪声；
/// 0 会格式化成 `0`，与服务端表示不限额的约定一致。
fn format_quota_input(quota: f64) -> String {
    if !quota.is_finite() || quota <= 0.0 {
        return "0".to_owned();
    }
    let mut text = format!("{quota:.4}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// 新额度是否会立即耗尽：只对有限额度且未勾选清零时提示。
fn quota_will_exceed(quota: f64, quota_used: f64, reset_quota: bool) -> bool {
    !reset_quota && quota > 0.0 && quota_used >= quota
}

/// 同时允许登录的账号上限（需求方要求：同时最多 10 个）。
const MAX_ACCOUNTS: usize = 10;

/// 同时拉取 API Key 列表的账号数上限。
///
/// Key 列表请求比状态刷新更轻，但多个账号同时展开时仍不应无限并发；
/// 4 个与账号状态刷新保持一致，既明显缩短等待时间，也避免瞬间打满实例。
const MAX_CONCURRENT_KEY_FETCHES: usize = 4;

/// 用量曲线的缓存有效期：到点后重新拉，避免频繁打 Heavy 限流接口。
const USAGE_TTL: Duration = Duration::from_secs(60);

/// 面板里一张账号卡片需要的展示数据。
///
/// 在绘制前一次性算好（见 `collect_cards`），这样绘制闭包只依赖局部变量，
/// 不会和 `&mut self` 打架；也避免在闭包里反复查运行态。
struct AccountCard {
    /// 账号 id（点击展开要用）。
    id: String,
    /// 账号来源。
    provider: AccountProvider,
    /// 展示用邮箱（已按开关打码）。
    display: String,
    /// 实例地址。
    base_url: String,
    /// 登录态。
    status: AccountStatus,
    /// 余额（美元）。
    balance: Option<f64>,
    /// 订阅摘要的一行文字。
    summary: Option<String>,
    /// "x 秒前刷新"。
    refreshed: String,
    /// 最近一次失败原因。
    error: Option<String>,
    /// 是否展开该账号的 API Key 明细。
    expanded: bool,
    /// 展开时的 Key 明细（`None` 表示还没拉取）。
    keys: Option<AccountKeysState>,
    /// 订阅名（来自 `/subscriptions/summary` 的分组名）。
    subscription: Option<String>,
    /// 刚刚复制成功的 Key id（用于浮窗提示，过期后为 `None`）。
    copied_key: Option<String>,
    /// 当前选中的用量区间。
    usage_range: UsageRange,
    /// 当前选中的图表指标。
    usage_metric: UsageMetric,
    /// 当前选中的 API Key；`None` 表示全部 Key。
    usage_api_key_id: Option<String>,
    /// 用量区是否被用户单独收起。
    usage_collapsed: bool,
    /// 当前选择下补齐后的用量点。
    usage_points: Vec<crate::api::UsagePoint>,
    /// 用量是否正在拉取。
    usage_loading: bool,
    /// 用量拉取失败的原因。
    usage_error: Option<String>,
}

/// 单个账号的 API Key 明细状态（面板内展开时用）。
#[derive(Debug, Clone, Default)]
struct AccountKeysState {
    /// 是否正在拉取。
    loading: bool,
    /// 是否是"离线回退"的缓存数据。
    cached: bool,
    /// 错误提示。
    error: Option<String>,
    /// 已拉取到的 Key 列表。
    keys: Vec<crate::api::ApiKeyInfo>,
}

/// API Key 弹窗的后台结果。
enum ApiKeysReply {
    /// 列表拉取结果（错误已是可展示文案）。
    List(Result<Vec<crate::api::ApiKeyInfo>, String>),
}

/// 用户点某个 Key 的调整额度按钮后，需要带到编辑弹窗里的纯数据。
///
/// 绘制函数拿不到 `&mut TrayApp`，所以先把 Key 名称、额度与用量摘出来，
/// 再由调用方打开弹窗；这样也能避免弹窗打开后还持有账号列表的借用。
struct QuotaEditTarget {
    /// 所属账号 id，用于定位凭据与缓存。
    account_id: String,
    /// Key id，用于拼 `PUT /api/v1/keys/{id}`。
    key_id: String,
    /// Key 名称，用于弹窗标题辅助说明。
    key_name: String,
    /// 打码后的 Key 值，用于确认编辑对象。
    key_masked: String,
    /// 当前额度（0 表示不限额）。
    current_quota: f64,
    /// 已用额度（美元）。
    quota_used: f64,
}

/// 调额度弹窗的编辑状态。
struct QuotaEditState {
    /// 所属账号 id。
    account_id: String,
    /// Key id。
    key_id: String,
    /// Key 名称。
    key_name: String,
    /// 打码后的 Key 值。
    key_masked: String,
    /// 用户输入的新额度，保留字符串以便校验空值与非法字符。
    quota_input: String,
    /// 打开弹窗时的额度，用于展示当前值。
    current_quota: f64,
    /// 当前已用额度，用于展示与额度低于已用的警告。
    quota_used: f64,
    /// 是否同时把已用额度清零。
    reset_quota: bool,
    /// 是否正在等待后台请求。
    submitting: bool,
    /// 最近一次保存失败原因。
    error: Option<String>,
}

/// 额度更新后台结果：(账号 id, Key id, 更新后的摘要或错误文案)。
type QuotaUpdateReply = (String, String, Result<crate::api::ApiKeyInfo, String>);

/// 用户点某个 Key 的删除按钮后，用于打开确认弹窗的纯数据。
struct DeleteKeyTarget {
    /// 所属账号 id。
    account_id: String,
    /// Key id。
    key_id: String,
    /// Key 名称。
    key_name: String,
    /// 打码后的 Key 值。
    key_masked: String,
}

/// 删除 Key 确认弹窗的状态。
struct DeleteKeyState {
    /// 所属账号 id。
    account_id: String,
    /// Key id。
    key_id: String,
    /// Key 名称。
    key_name: String,
    /// 打码后的 Key 值。
    key_masked: String,
    /// 是否正在等待后台请求。
    submitting: bool,
    /// 最近一次删除失败原因。
    error: Option<String>,
}

/// 删除 Key 后台结果：(账号 id, Key id, 结果)。
type DeleteKeyReply = (String, String, Result<(), String>);

/// 创建 Key 时的有效期模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryMode {
    /// 按天数，从创建时间起计算。
    Days,
    /// 指定精确结束时间（RFC3339，含时区）。
    DateTime,
}

/// 创建 API Key 弹窗状态。
struct CreateKeyState {
    /// 所属账号 id。
    account_id: String,
    /// 新 Key 名称。
    name: String,
    /// 额度输入（美元，0 表示不限额）。
    quota_input: String,
    /// 有效期模式。
    expiry_mode: ExpiryMode,
    /// 有效期天数输入；空或 0 表示永久有效。
    expires_in_days_input: String,
    /// 精确结束日期（本地时区）。
    expires_date: jiff::civil::Date,
    /// 精确结束小时（本地时区，0-23）。
    expires_hour: i8,
    /// 精确结束分钟（本地时区，0-59）。
    expires_minute: i8,
    /// 当前账号可绑定的分组。
    groups: Vec<crate::api::ApiKeyGroup>,
    /// 已选择的分组 id。
    selected_group_id: Option<i64>,
    /// 是否正在加载分组。
    loading_groups: bool,
    /// 是否正在提交创建请求。
    submitting: bool,
    /// 最近一次错误文案。
    error: Option<String>,
    /// 创建成功后的完整 Key；关闭弹窗即丢弃，不写入缓存。
    created: Option<crate::api::CreatedApiKey>,
}

/// 创建 API Key 的后台结果。
enum CreateKeyReply {
    /// 可用分组加载结果（附带账号 id，防止弹窗切换后串结果）。
    Groups(String, Result<Vec<crate::api::ApiKeyGroup>, String>),
    /// 创建结果（附带账号 id；大对象装箱，避免枚举整体过大）。
    Created(String, Box<Result<crate::api::CreatedApiKey, String>>),
}

/// 在 Key 列表中收集本帧发生的操作。
///
/// 绘制函数拿不到 `&mut TrayApp`，所以把复制与调额度这两个动作装进一个结构，
/// 绘制结束后由 `draw_panel` 统一消费。这样也避免继续给绘制函数增加参数。
#[derive(Default)]
struct KeyUiActions {
    /// 请求复制完整值的 (账号 id, Key id)。
    copy_request: Option<(String, String)>,
    /// 请求打开额度编辑弹窗的目标 Key。
    quota_request: Option<QuotaEditTarget>,
    /// 请求打开删除确认弹窗的目标 Key。
    delete_request: Option<DeleteKeyTarget>,
    /// 请求在某个账号下创建 API Key。
    create_key_request: Option<String>,
    /// 请求为凭据损坏的账号原地重新登录。
    relogin_request: Option<String>,
}

/// 应用主体。
pub struct TrayApp {
    /// 托盘图标句柄。**必须持有**：drop 之后托盘图标会消失。
    tray: TrayHandle,

    /// 与托盘回调通信的桥，每帧从中取指令。
    bridge: Arc<TrayBridge>,

    /// 面板当前是否应当可见（这是应用侧的期望值，不是操作系统的实际值）。
    panel_visible: bool,

    /// 面板本次被弹出的时刻，用于计算 [`FOCUS_GRACE`] 宽限期。
    shown_at: Option<Instant>,

    /// 面板上一次被收起的时刻，用于 [`TOGGLE_DEBOUNCE`] 去抖。
    last_hidden_at: Option<Instant>,

    /// 是否还在第一帧。
    ///
    /// eframe 会先显示窗口再跑第一帧（历史上是为了避免白屏闪烁），
    /// 托盘程序启动时不应该出现面板，所以第一帧里立刻把它收起来。
    first_frame: bool,

    /// 调试开关：进程启动时若存在环境变量 `SUB2API_TRAY_DEBUG_VISIBLE`，
    /// 就直接弹出面板，并且**不因失焦自动收起**。
    ///
    /// 之所以需要它：托盘程序平时启动后处于“隐身”状态，自动化冒烟测试和
    /// 截图验收都没办法去点托盘图标；有了这个开关就能稳定复现“面板已打开”的画面。
    /// 正常使用时不设置该变量，行为与正式版本完全一致。
    debug_show_on_start: bool,

    /// 调试开关：存在环境变量 `SUB2API_TRAY_DEBUG_LOGIN` 时，启动后自动打开登录弹窗。
    ///
    /// 仅用于截图验收弹窗布局：自动化无法真的去点“登陆新用户”按钮。
    /// 打开时会连带显示面板，否则弹窗没有可绘制的窗口。
    debug_open_login: bool,

    /// 数据层。
    ///
    /// `Unavailable` 表示 `app.json` / `accounts.json` 读不了或写不了
    /// （最常见的原因：程序被放在 `Program Files` 这类只读目录）。
    /// 此时面板顶部会显示红色提示，并且不提供登录入口。
    data: DataLayer,

    /// 登录弹窗（含后台线程与表单状态）。
    login: LoginDialog,

    /// 操作成功提示：内容 + 出现时刻。
    ///
    /// 用一个临时的 `(String, Instant)` 而不是完整的通知系统，是因为目前只有
    /// "账号已保存 / 已删除 / 删除失败" 这几种提示，等通知需求变多再抽象。
    toast: Option<(String, Instant)>,

    /// 账号运行态（在线/离线/余额/订阅摘要）。
    ///
    /// 只存内存，不落盘：重启后由第一轮刷新重新判定。
    runtime: RuntimeTable,

    /// 进行中的刷新轮次；`None` 表示当前没有刷新在跑。
    refresh_rx: Option<Receiver<Vec<RoundResult>>>,

    /// 上一轮刷新的开始时刻，用于判断"到点该刷新了"。
    last_round: Option<Instant>,

    /// 进程启动时刻，用于延迟首刷。
    started_at: Instant,

    /// 等待用户确认删除的账号 id。
    ///
    /// 点托盘子菜单里的"退出账号并删除记录"只设置它，真正的删除在面板的确认框里完成。
    pending_delete: Option<String>,

    /// 当前已经应用到托盘图标上的徽标颜色。
    ///
    /// 用来做"只在变化时更新"：`set_icon` 会触发任务栏重绘，每轮刷新都换会闪。
    badge_applied: Option<[u8; 3]>,

    /// 是否已经应用过一次徽标（区分"还没设过"和"设成无徽标"）。
    badge_initialized: bool,

    /// 面板里已展开 API Key 明细的账号 id。
    expanded_accounts: HashSet<String>,

    /// 每个账号的 API Key 明细缓存（展开时拉取）。
    account_keys: HashMap<String, AccountKeysState>,

    /// 每个账号正在进行的 Key 列表拉取；多个账号可同时进行。
    keys_rx: HashMap<String, Receiver<ApiKeysReply>>,

    /// 面板是否被钉住（置顶显示，点外部不自动收起）。
    pinned: bool,

    /// 是否显示桌面悬浮球。
    floating_ball_visible: bool,
    /// 悬浮球是否启用贴边自动收起。
    floating_ball_edge_hide: bool,
    /// 悬浮球主目标。
    floating_ball_target: Option<FloatingBallTarget>,
    /// 上一帧鼠标是否在悬浮球上。
    floating_ball_hovered: bool,
    /// 主 Key 最近一次刷新时间。
    floating_ball_key_refresh_at: Option<Instant>,

    dark_mode: bool,

    /// 每个账号选中的用量区间（默认 24 小时）。
    usage_range: HashMap<String, UsageRange>,

    /// 每个账号选中的用量图表指标（默认总 Token）。
    usage_metric: HashMap<String, UsageMetric>,

    /// 每个账号选中的用量 API Key（没有记录表示全部 Key）。
    usage_api_key: HashMap<String, String>,

    /// 用户单独收起用量区的账号 id（只影响用量区，不影响账号展开）。
    usage_collapsed: HashSet<String>,

    /// 用量曲线缓存：账号 + 区间 + API Key → 曲线。
    usage: HashMap<UsageCacheKey, UsageSeries>,

    /// 进行中的用量拉取（同一时间只允许一个）。
    usage_rx: Option<UsageReply>,

    /// 悬浮高亮动画状态（账号卡片、Key 行共用）。
    hover: HoverState,

    /// 调试开关：启动后自动展开第一个账号的 Key 明细（仅截图验收用）。
    debug_expand: bool,

    /// 是否已经从 `accounts.json` 恢复过展开状态（只在启动后做一次）。
    restored_expansion: bool,

    /// 设置页草稿（`None` 表示未打开）。
    settings: Option<SettingsDraft>,

    /// 打开设置页前的主题，用于取消/关闭时回滚实时预览。
    settings_original_dark_mode: Option<bool>,

    /// 设置保存后台通道；Some 时界面显示「正在保存设置」遮罩。
    settings_save_rx: Option<Receiver<Result<SettingsSaveResult, String>>>,

    /// 最近一次读取到的开机自启状态；`None` 表示后台读取尚未完成。
    ///
    /// 打开设置页时不能再同步执行 `reg query`，否则会被注册表/杀软拖慢整个 UI 线程。
    autostart_enabled: Option<bool>,

    /// 开机自启状态的异步读取通道。
    autostart_rx: Option<Receiver<bool>>,

    /// 正在复制的 Key id（用于按钮态/日志）。
    copying_key: Option<String>,

    /// 复制结果通道：`(key_id, 完整值或错误文案)`。
    copy_rx: Option<Receiver<CopyReply>>,

    /// 复制成功的浮窗：`(账号 id, Key id, 出现时刻)`。
    copy_notice: Option<(String, String, Instant)>,

    /// 调额度弹窗的编辑状态；`None` 表示弹窗未打开。
    quota_edit: Option<QuotaEditState>,

    /// 额度更新请求通道；同一时间只允许一个在途请求。
    quota_update_rx: Option<Receiver<QuotaUpdateReply>>,

    /// 删除 Key 确认弹窗状态；`None` 表示未打开。
    delete_key: Option<DeleteKeyState>,

    /// 删除 Key 请求通道；同一时间只允许一个在途请求。
    delete_key_rx: Option<Receiver<DeleteKeyReply>>,

    /// 创建 API Key 弹窗状态。
    create_key: Option<CreateKeyState>,

    /// 创建 API Key 的后台请求通道。
    create_key_rx: Option<Receiver<CreateKeyReply>>,

    /// 是否正在检查更新。
    update_checking: bool,

    /// 更新检查结果文案。
    update_result: Option<String>,

    /// 新版本下载页。
    update_url: Option<String>,

    /// 更新检查通道。
    update_rx: Option<Receiver<Result<Option<crate::update::UpdateInfo>, String>>>,
}

impl TrayApp {
    /// 创建应用：装字体、设配色、建托盘。
    ///
    /// 这个函数在 winit 事件循环内部被调用，此时主线程与事件循环都已就绪，
    /// 满足 `tray-icon` 对“必须先有事件循环”的要求。
    ///
    /// `data` 由 `main` 在启动时初始化好并移交给这里：数据层失败不应该阻止托盘与面板出现，
    /// 否则用户会以为程序根本没启动。
    pub fn new(cc: &eframe::CreationContext<'_>, data: DataLayer) -> anyhow::Result<Self> {
        // 中文字体必须在任何界面绘制之前装好，否则第一帧会是方块
        fonts::install(&cc.egui_ctx);

        // 高 DPI 说明：egui 里所有尺寸都是"逻辑点"，真实像素 = 点 × pixels_per_point，
        // 由 winit 按系统缩放（100%/125%/150%…）设置。面板尺寸、字号、间距都按点写，
        // 因此换成高分辨率屏幕时会整体等比放大，不会出现"字小得像蚂蚁"。
        log::info!(
            "界面缩放 {}x（高 DPI 下尺寸与字号按此比例放大）",
            cc.egui_ctx.pixels_per_point()
        );

        let dark_mode = match &data {
            DataLayer::Ready(store) => store.config().theme.as_str() != "light",
            DataLayer::Unavailable(_) => true,
        };
        set_palette(dark_mode);
        apply_egui_visuals(&cc.egui_ctx, dark_mode);

        // 钉住状态是持久化的 UI 偏好，启动时从 app.json 读回
        let pinned = match &data {
            DataLayer::Ready(store) => store.config().panel_pinned,
            DataLayer::Unavailable(_) => false,
        };

        let (floating_ball_visible, floating_ball_edge_hide, floating_ball_target) =
            match &data {
                DataLayer::Ready(store) => {
                    let config = store.config();
                    let target = config.floating_ball_account_id.as_ref().map(|account_id| {
                        FloatingBallTarget {
                            account_id: account_id.clone(),
                            key_id: config.floating_ball_api_key_id.clone(),
                        }
                    });
                    (
                        config.floating_ball_visible,
                        config.floating_ball_edge_hide,
                        target,
                    )
                }
                DataLayer::Unavailable(_) => (false, true, None),
            };

        let bridge = TrayBridge::new(cc.egui_ctx.clone());
        let tray = crate::tray::install(Arc::clone(&bridge))?;

        let debug_show_on_start = std::env::var_os("SUB2API_TRAY_DEBUG_VISIBLE").is_some();
        if debug_show_on_start {
            log::warn!("检测到 SUB2API_TRAY_DEBUG_VISIBLE：启动后直接显示面板（仅用于调试）");
        }
        let debug_open_login = std::env::var_os("SUB2API_TRAY_DEBUG_LOGIN").is_some();
        if debug_open_login {
            log::warn!("检测到 SUB2API_TRAY_DEBUG_LOGIN：启动后直接打开登录弹窗（仅用于调试）");
        }

        // 弹窗持有 egui 上下文：后台登录线程靠它唤醒界面
        let login = LoginDialog::new(cc.egui_ctx.clone());

        let mut app = Self {
            tray,
            bridge,
            panel_visible: false,
            shown_at: None,
            last_hidden_at: None,
            first_frame: true,
            debug_show_on_start,
            debug_open_login,
            data,
            login,
            toast: None,
            runtime: RuntimeTable::default(),
            refresh_rx: None,
            last_round: None,
            started_at: Instant::now(),
            pending_delete: None,
            badge_applied: None,
            badge_initialized: false,
            expanded_accounts: HashSet::new(),
            account_keys: HashMap::new(),
            keys_rx: HashMap::new(),
            pinned,
            floating_ball_visible,
            floating_ball_edge_hide,
            floating_ball_target,
            floating_ball_hovered: false,
            floating_ball_key_refresh_at: None,
            dark_mode,
            usage_range: HashMap::new(),
            usage_metric: HashMap::new(),
            usage_api_key: HashMap::new(),
            usage_collapsed: HashSet::new(),
            usage: HashMap::new(),
            usage_rx: None,
            hover: HoverState::default(),
            debug_expand: std::env::var_os("SUB2API_TRAY_DEBUG_EXPAND").is_some(),
            restored_expansion: false,
            settings: None,
            settings_original_dark_mode: None,
            settings_save_rx: None,
            autostart_enabled: None,
            autostart_rx: None,
            copying_key: None,
            copy_rx: None,
            copy_notice: None,
            quota_edit: None,
            quota_update_rx: None,
            delete_key: None,
            delete_key_rx: None,
            create_key: None,
            create_key_rx: None,
            update_checking: false,
            update_result: None,
            update_url: None,
            update_rx: None,
        };

        // 开机自启状态在后台读取，避免首次打开设置页时同步等待 reg query。
        app.start_autostart_check(&cc.egui_ctx);

        // 启动时先按已加载的账号建一次菜单（此时状态都是"未知"）
        app.rebuild_menu();
        app.update_badge();

        Ok(app)
    }

    /// 按当前账号与运行态重建托盘菜单与提示文字。
    ///
    /// `muda` 菜单不能增删项，所以这里是"整份重建"。调用时机：
    /// 启动、登录成功、删除账号、每轮刷新结束、状态标记为"刷新中"。
    fn rebuild_menu(&self) {
        // 账号不再进原生菜单（Windows/macOS 的系统菜单既不能滚动也不能展开），
        // 菜单只保留固定入口；账号的一切操作都在面板里完成。
        match crate::tray::build_menu(&self.tray_menu_model()) {
            Ok(menu) => self.tray.set_menu(menu),
            Err(err) => log::warn!("重建托盘菜单失败：{err:#}"),
        }

        if let Err(err) = self.tray.set_tooltip(&self.tooltip_text()) {
            log::debug!("更新托盘提示失败：{err:#}");
        }
    }

    /// 托盘悬停提示：账号总数与在线数。
    fn tooltip_text(&self) -> String {
        match &self.data {
            DataLayer::Ready(store) => {
                let total = store.accounts().len();
                let online = self.runtime.online_count();
                format!("用量统计托盘 · {total} 个账号（{online} 在线）")
            }
            DataLayer::Unavailable(_) => "用量统计托盘 · 数据目录不可用".to_owned(),
        }
    }

    /// 已加载的账号数量（日志与提示用）。
    fn data_account_count(&self) -> usize {
        match &self.data {
            DataLayer::Ready(store) => store.accounts().len(),
            DataLayer::Unavailable(_) => 0,
        }
    }

    /// 当前应该显示的徽标颜色（由运行态聚合而来）。
    fn badge_color(&self) -> Option<[u8; 3]> {
        let DataLayer::Ready(store) = &self.data else {
            return None;
        };

        let mut online = 0_usize;
        let mut offline = 0_usize;
        let mut invalid = 0_usize;
        for account in store.accounts() {
            match self
                .runtime
                .get(&account.id)
                .map(|runtime| runtime.status)
                .unwrap_or_default()
            {
                AccountStatus::Online => online += 1,
                AccountStatus::Offline => offline += 1,
                AccountStatus::CredentialsInvalid => invalid += 1,
                _ => {}
            }
        }

        aggregate_badge(store.accounts().len(), online, offline, invalid)
    }

    /// 把聚合出的徽标应用到托盘图标（仅在变化时真正更新）。
    fn update_badge(&mut self) {
        let next = self.badge_color();
        if self.badge_initialized && self.badge_applied == next {
            return;
        }

        self.badge_applied = next;
        self.badge_initialized = true;

        match self.tray.set_badge(next) {
            Ok(()) => log::info!(
                "托盘图标状态徽标：{}",
                next.map(badge_label).unwrap_or("无（状态未知或没有账号）")
            ),
            Err(err) => log::warn!("更新托盘徽标失败：{err:#}"),
        }
    }

    /// 把"账号 + 运行态"整理成面板要画的卡片数据。
    fn collect_cards(&self) -> Vec<AccountCard> {
        let DataLayer::Ready(store) = &self.data else {
            return Vec::new();
        };

        store
            .accounts()
            .iter()
            .map(|account| {
                let runtime = self.runtime.get(&account.id);
                let status = runtime.map(|item| item.status).unwrap_or_default();
                let summary = runtime.and_then(|item| item.summary.as_ref()).map(|item| {
                    if item.active_count == 0 {
                        "无生效订阅".to_owned()
                    } else if item.group_names.is_empty() {
                        format!(
                            "{} 个订阅 · 已用 ${:.2}",
                            item.active_count, item.total_used_usd
                        )
                    } else {
                        format!(
                            "{} 个订阅 · 已用 ${:.2} · {}",
                            item.active_count,
                            item.total_used_usd,
                            item.group_names.join("、")
                        )
                    }
                });

                let refreshed = runtime
                    .and_then(|item| item.last_refreshed)
                    .map(|at| format!("{} 秒前刷新", at.elapsed().as_secs()))
                    .unwrap_or_else(|| "尚未刷新".to_owned());

                let usage_range = self.usage_range_of(&account.id);
                let usage_metric = self.usage_metric_of(&account.id);
                let usage_api_key_id = self.usage_api_key_of(&account.id);
                let usage_series =
                    self.usage
                        .get(&(account.id.clone(), usage_range, usage_api_key_id.clone()));

                AccountCard {
                    id: account.id.clone(),
                    provider: account.provider,
                    display: if account.provider == AccountProvider::DeepSeekPlatform {
                        account.email.clone()
                    } else {
                        store.display_email(&account.email)
                    },
                    base_url: if account.provider == AccountProvider::DeepSeekPlatform {
                        "DeepSeek 官方 API Key".to_owned()
                    } else {
                        account.base_url.clone()
                    },
                    status,
                    balance: runtime.and_then(|item| item.balance),
                    summary,
                    refreshed,
                    error: runtime.and_then(|item| item.last_error.clone()),
                    expanded: self.expanded_accounts.contains(&account.id),
                    keys: self.account_keys.get(&account.id).cloned(),
                    subscription: runtime
                        .and_then(|item| item.summary.as_ref())
                        .map(|item| item.group_names.join("、"))
                        .filter(|names| !names.is_empty()),
                    usage_range,
                    usage_metric,
                    usage_api_key_id,
                    usage_collapsed: self.usage_collapsed.contains(&account.id),
                    usage_points: usage_series
                        .map(|series| series.points.clone())
                        .unwrap_or_default(),
                    usage_loading: usage_series.map(|series| series.loading).unwrap_or(false),
                    usage_error: usage_series.and_then(|series| series.error.clone()),
                    copied_key: self
                        .copy_notice
                        .as_ref()
                        .filter(|(account_id, _, at)| {
                            account_id == &account.id && at.elapsed().as_secs_f32() < 1.6
                        })
                        .map(|(_, key_id, _)| key_id.clone()),
                }
            })
            .collect()
    }

    /// 处理本帧从托盘收到的指令。
    /// 构建托盘菜单里的悬浮球目标列表。
    fn tray_menu_model(&self) -> crate::tray::TrayMenuModel {
        let mut targets = Vec::new();
        let DataLayer::Ready(store) = &self.data else {
            return crate::tray::TrayMenuModel {
                ball_visible: self.floating_ball_visible,
                ball_edge_hide: self.floating_ball_edge_hide,
                ball_targets: targets,
            };
        };

        for account in store.accounts() {
            let account_label = if account.provider == AccountProvider::DeepSeekPlatform {
                format!("{} · 余额", account.email)
            } else {
                store.display_email(&account.email)
            };
            let account_selected = self
                .floating_ball_target
                .as_ref()
                .is_some_and(|target| target.account_id == account.id && target.key_id.is_none());
            targets.push(crate::tray::BallTargetMenuItem {
                account_id: account.id.clone(),
                key_id: None,
                label: account_label,
                selected: account_selected,
            });

            let keys = self
                .account_keys
                .get(&account.id)
                .map(|state| state.keys.clone())
                .unwrap_or_else(|| self.cached_api_keys(&account.id));
            for key in keys {
                let selected = self.floating_ball_target.as_ref().is_some_and(|target| {
                    target.account_id == account.id
                        && target.key_id.as_deref() == Some(key.id.as_str())
                });
                targets.push(crate::tray::BallTargetMenuItem {
                    account_id: account.id.clone(),
                    key_id: Some(key.id.clone()),
                    label: format!("{}  {}", key.name, key.masked),
                    selected,
                });
            }
        }

        crate::tray::TrayMenuModel {
            ball_visible: self.floating_ball_visible,
            ball_edge_hide: self.floating_ball_edge_hide,
            ball_targets: targets,
        }
    }

    /// 把悬浮球配置写回 app.json。
    fn persist_floating_ball_settings(&mut self) {
        let DataLayer::Ready(store) = &mut self.data else {
            return;
        };
        let mut config = store.config().clone();
        config.floating_ball_visible = self.floating_ball_visible;
        config.floating_ball_edge_hide = self.floating_ball_edge_hide;
        config.floating_ball_account_id = self
            .floating_ball_target
            .as_ref()
            .map(|target| target.account_id.clone());
        config.floating_ball_api_key_id = self
            .floating_ball_target
            .as_ref()
            .and_then(|target| target.key_id.clone());
        if let Err(err) = crate::config::save(store.dir(), &config) {
            log::warn!("保存悬浮球配置失败：{err:#}");
        } else {
            *store.config_mut() = config;
        }
    }

    /// 选一个默认悬浮球目标（第一个账号）。
    fn choose_default_floating_ball_target(&mut self) {
        if self.floating_ball_target.is_some() {
            return;
        }
        let account_id = match &self.data {
            DataLayer::Ready(store) => store.accounts().first().map(|item| item.id.clone()),
            DataLayer::Unavailable(_) => None,
        };
        if let Some(account_id) = account_id {
            self.floating_ball_target = Some(FloatingBallTarget {
                account_id,
                key_id: None,
            });
        }
    }

    /// 设置悬浮球主目标，并立即拉一轮数据。
    fn set_floating_ball_target(
        &mut self,
        ctx: &egui::Context,
        account_id: String,
        key_id: Option<String>,
    ) {
        self.floating_ball_target = Some(FloatingBallTarget {
            account_id: account_id.clone(),
            key_id: key_id.clone(),
        });
        self.floating_ball_visible = true;
        self.floating_ball_key_refresh_at = key_id.as_ref().map(|_| Instant::now());
        self.persist_floating_ball_settings();
        self.rebuild_menu();
        self.try_start_refresh(ctx, true);
        if key_id.is_some() {
            self.request_account_keys(ctx, &account_id);
        }
        ctx.request_repaint();
    }

    /// 悬浮球主 Key 的后台刷新；账号余额由现有状态刷新链更新。
    fn maybe_refresh_floating_ball_data(&mut self, ctx: &egui::Context) {
        if !self.floating_ball_visible {
            return;
        }
        let Some(target) = self.floating_ball_target.clone() else {
            return;
        };
        if target.key_id.is_none() {
            return;
        }
        let due = self
            .floating_ball_key_refresh_at
            .map(|at| at.elapsed() >= FLOATING_BALL_KEY_REFRESH_TTL)
            .unwrap_or(true);
        if due && !self.keys_rx.contains_key(&target.account_id) {
            self.floating_ball_key_refresh_at = Some(Instant::now());
            self.request_account_keys(ctx, &target.account_id);
        }
    }

    /// 计算悬浮球当前展示内容。
    fn floating_ball_view(&self) -> FloatingBallView {
        let status = self
            .floating_ball_target
            .as_ref()
            .and_then(|target| self.runtime.get(&target.account_id))
            .map(|runtime| runtime.status)
            .unwrap_or(AccountStatus::Unknown);
        let Some(target) = self.floating_ball_target.as_ref() else {
            return FloatingBallView {
                text: "设置".to_owned(),
                status,
            };
        };
        let DataLayer::Ready(store) = &self.data else {
            return FloatingBallView {
                text: "--".to_owned(),
                status,
            };
        };
        let Some(account) = store
            .accounts()
            .iter()
            .find(|item| item.id == target.account_id)
        else {
            return FloatingBallView {
                text: "失效".to_owned(),
                status,
            };
        };

        if let Some(key_id) = target.key_id.as_ref() {
            let key = self
                .account_keys
                .get(&account.id)
                .and_then(|state| state.keys.iter().find(|key| key.id == *key_id))
                .cloned()
                .or_else(|| {
                    self.cached_api_keys(&account.id)
                        .into_iter()
                        .find(|key| key.id == *key_id)
                });
            if let Some(key) = key {
                let text = match key.quota {
                    Some(quota) if quota > 0.0 => {
                        format!("${:.2}", (quota - key.quota_used.unwrap_or(0.0)).max(0.0))
                    }
                    _ => "不限".to_owned(),
                };
                return FloatingBallView { text, status };
            }
            return FloatingBallView {
                text: "--".to_owned(),
                status,
            };
        }

        let balance = self
            .runtime
            .get(&account.id)
            .and_then(|runtime| runtime.balance);
        let text = balance
            .map(|value| {
                if account.provider == AccountProvider::DeepSeekPlatform {
                    format!("¥{value:.2}")
                } else {
                    format!("${value:.2}")
                }
            })
            .unwrap_or_else(|| "--".to_owned());
        FloatingBallView { text, status }
    }

    /// 绘制并处理桌面悬浮球窗口。
    fn render_floating_ball(&mut self, ctx: &egui::Context) {
        if !self.floating_ball_visible {
            return;
        }
        let view = self.floating_ball_view();
        let expanded = !self.floating_ball_edge_hide || self.floating_ball_hovered;
        let width = ui_style::size::FLOATING_BALL_SIZE;
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or(egui::vec2(1920.0, 1080.0));
        let position = egui::pos2(
            (monitor.x - width - ui_style::size::FLOATING_BALL_MARGIN).max(0.0),
            ((monitor.y - ui_style::size::FLOATING_BALL_SIZE) * 0.5).max(0.0),
        );
        let viewport_id = egui::ViewportId::from_hash_of("floating-ball");
        let builder = egui::ViewportBuilder::default()
            .with_title("用量统计悬浮球")
            .with_inner_size([width, ui_style::size::FLOATING_BALL_SIZE])
            .with_min_inner_size([width, ui_style::size::FLOATING_BALL_SIZE])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_position(position);

        let mut clicked = false;
        let mut hovered = false;
        let mut close_requested = false;
        ctx.show_viewport_immediate(viewport_id, builder, |ui, _class| {
            let child_ctx = ui.ctx().clone();
            close_requested = child_ctx.input(|i| i.viewport().close_requested());
            // 不套 CentralPanel/Frame：它们会绘制面板底色，透明 viewport 上会留下
            // 一层半透明方形底层。直接在最外层 Ui 上分配矩形并只画圆/窄条。
            ui.set_min_size(egui::vec2(width, ui_style::size::FLOATING_BALL_SIZE));
            let (rect, response) = ui.allocate_exact_size(
                egui::vec2(width, ui_style::size::FLOATING_BALL_SIZE),
                egui::Sense::click(),
            );
            let painter = ui.painter();
            if expanded {
                let center = rect.center();
                painter.circle_filled(
                    center,
                    ui_style::size::FLOATING_BALL_SIZE * 0.5 - 2.0,
                    theme().accent,
                );
                painter.circle_stroke(
                    center,
                    ui_style::size::FLOATING_BALL_SIZE * 0.5 - 2.0,
                    egui::Stroke::new(1.0, theme().on_accent),
                );
                painter.text(
                    center,
                    egui::Align2::CENTER_CENTER,
                    &view.text,
                    egui::FontId::proportional(font::BODY_SMALL_PLUS),
                    theme().on_accent,
                );
                painter.circle_filled(
                    rect.right_bottom() - egui::vec2(12.0, 12.0),
                    4.0,
                    status_color(view.status),
                );
            } else {
                let tab = egui::Rect::from_min_max(
                    egui::pos2(
                        rect.right() - ui_style::size::FLOATING_BALL_COLLAPSED_WIDTH,
                        rect.top() + 4.0,
                    ),
                    rect.right_bottom() - egui::vec2(0.0, 4.0),
                );
                painter.rect_filled(
                    tab,
                    egui::CornerRadius::same(ui_style::size::RADIUS_LG),
                    theme().accent,
                );
                painter.circle_filled(tab.center(), 3.0, status_color(view.status));
            }
            let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
            hovered = response.hovered();
            if response.clicked() {
                clicked = true;
            }
        });

        if close_requested {
            self.floating_ball_visible = false;
            self.persist_floating_ball_settings();
            self.rebuild_menu();
        }
        if hovered != self.floating_ball_hovered {
            self.floating_ball_hovered = hovered;
            ctx.request_repaint();
        }
        if clicked {
            self.show_panel(ctx);
        }
    }

    fn handle_tray_commands(&mut self, ctx: &egui::Context) {
        for command in self.bridge.drain() {
            match command {
                TrayCommand::TogglePanel => {
                    if self.panel_visible {
                        self.hide_panel(ctx);
                    } else if !self.recently_hidden() {
                        self.show_panel(ctx);
                    } else {
                        // 刚因失焦而收起：这次点击的真实语义是“关闭”，不再重新弹出
                        log::debug!("忽略去抖窗口内的托盘点击");
                    }
                }
                TrayCommand::RefreshNow => {
                    log::info!("收到手动刷新请求");
                    self.try_start_refresh(ctx, true);
                }
                TrayCommand::Quit => {
                    log::info!("收到退出指令，关闭应用");
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayCommand::ToggleFloatingBall => {
                    self.floating_ball_visible = !self.floating_ball_visible;
                    if self.floating_ball_visible {
                        self.choose_default_floating_ball_target();
                        self.try_start_refresh(ctx, true);
                    }
                    self.persist_floating_ball_settings();
                    self.rebuild_menu();
                    ctx.request_repaint();
                }
                TrayCommand::ToggleFloatingBallEdgeHide => {
                    self.floating_ball_edge_hide = !self.floating_ball_edge_hide;
                    self.persist_floating_ball_settings();
                    self.rebuild_menu();
                    ctx.request_repaint();
                }
                TrayCommand::SetFloatingBallTarget { account_id, key_id } => {
                    self.set_floating_ball_target(ctx, account_id, key_id);
                }
                TrayCommand::RefreshFloatingBallTargets => {
                    let ids = match &self.data {
                        DataLayer::Ready(store) => store
                            .accounts()
                            .iter()
                            .filter(|account| account.provider == AccountProvider::Sub2Api)
                            .map(|account| account.id.clone())
                            .collect::<Vec<_>>(),
                        DataLayer::Unavailable(_) => Vec::new(),
                    };
                    for id in ids {
                        if !self.keys_rx.contains_key(&id) {
                            self.request_account_keys(ctx, &id);
                        }
                    }
                    self.try_start_refresh(ctx, true);
                    self.toast = Some(("正在刷新 Key 列表".to_owned(), Instant::now()));
                    ctx.request_repaint();
                }
            }
        }
    }

    /// 把 Key 摘要写进 `accounts.json` 的 `apiKeys`（不含完整 Key）。
    fn cache_api_keys(&mut self, account_id: &str, keys: &[crate::api::ApiKeyInfo]) {
        let DataLayer::Ready(store) = &mut self.data else {
            return;
        };
        let values = keys.iter().map(|key| key.to_cache_value()).collect();
        match store.accounts_mut().set_api_keys(account_id, values) {
            Ok(true) => log::debug!("已缓存 {} 个 API Key 摘要", keys.len()),
            Ok(false) => log::debug!("账号已不存在，跳过 API Key 缓存"),
            Err(err) => log::warn!("API Key 缓存写盘失败：{err:#}"),
        }
    }

    /// 从 `accounts.json` 读取某账号的 Key 缓存（断网回退用）。
    fn cached_api_keys(&self, account_id: &str) -> Vec<crate::api::ApiKeyInfo> {
        let DataLayer::Ready(store) = &self.data else {
            return Vec::new();
        };
        store
            .accounts()
            .iter()
            .find(|account| account.id == account_id)
            .map(|account| {
                account
                    .api_keys
                    .iter()
                    .filter_map(crate::api::ApiKeyInfo::from_cache_value)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 后台拉取某个账号的 API Key 列表（面板展开时用）。
    ///
    /// 与弹窗那条路径的区别：结果写进 `account_keys`，供面板卡片内联展示。
    /// 不同账号可以并行拉取，但同一账号同一时间只允许一个在途请求。
    fn request_account_keys(&mut self, ctx: &egui::Context, account_id: &str) {
        if self.keys_rx.contains_key(account_id) {
            log::debug!("账号 {account_id} 已有展开拉取在进行中，忽略本次请求");
            return;
        }

        let DataLayer::Ready(store) = &self.data else {
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            return;
        };
        if account.provider != AccountProvider::Sub2Api {
            return;
        }
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();
        let id = account_id.to_owned();

        self.account_keys.insert(
            id.clone(),
            AccountKeysState {
                loading: true,
                cached: false,
                ..AccountKeysState::default()
            },
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        self.keys_rx.insert(id.clone(), receiver);
        let repaint = ctx.clone();

        std::thread::spawn(move || {
            let reply =
                match crate::secrets::open_credentials(secret_store.as_ref(), &id, &secrets_blob) {
                    Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                        Ok(client) => client
                            .list_api_keys(&credentials.access_token)
                            .map_err(|err| err.user_message()),
                        Err(err) => Err(err.user_message()),
                    },
                    Err(err) => Err(format!("凭据无法解密：{err:#}")),
                };
            let _ = sender.send(ApiKeysReply::List(reply));
            repaint.request_repaint();
        });
    }

    /// 账号当前选中的用量区间（默认 24 小时）。
    fn usage_range_of(&self, account_id: &str) -> UsageRange {
        self.usage_range
            .get(account_id)
            .copied()
            .unwrap_or(UsageRange::Day)
    }

    /// 账号当前选中的用量图表指标（默认总 Token）。
    fn usage_metric_of(&self, account_id: &str) -> UsageMetric {
        self.usage_metric
            .get(account_id)
            .copied()
            .unwrap_or(UsageMetric::Total)
    }

    /// 账号当前选中的 API Key；`None` 表示全部 Key 汇总。
    fn usage_api_key_of(&self, account_id: &str) -> Option<String> {
        self.usage_api_key.get(account_id).cloned()
    }

    /// 切换某个账号的用量区间，并立刻确保有数据。
    fn set_usage_range(&mut self, ctx: &egui::Context, account_id: &str, range: UsageRange) {
        self.usage_range.insert(account_id.to_owned(), range);
        self.maybe_request_usage(ctx);
        ctx.request_repaint();
    }

    /// 切换某个账号的用量图表指标；不重新请求接口。
    fn set_usage_metric(&mut self, ctx: &egui::Context, account_id: &str, metric: UsageMetric) {
        self.usage_metric.insert(account_id.to_owned(), metric);
        ctx.request_repaint();
    }

    /// 切换某个账号的用量 API Key；`None` 表示回到全部 Key。
    fn set_usage_api_key(
        &mut self,
        ctx: &egui::Context,
        account_id: &str,
        api_key_id: Option<String>,
    ) {
        match api_key_id {
            Some(id) => {
                self.usage_api_key.insert(account_id.to_owned(), id);
            }
            None => {
                self.usage_api_key.remove(account_id);
            }
        }
        self.maybe_request_usage(ctx);
        ctx.request_repaint();
    }

    /// 后台拉取某账号某区间、可选用量 API Key 的曲线。
    ///
    /// 这里不判断账号角色：接口是用户态 `/usage/dashboard/trend`，
    /// 任何已登录用户都只能拿到自己的数据；管理员也使用同一路径。
    fn request_usage(
        &mut self,
        ctx: &egui::Context,
        account_id: &str,
        range: UsageRange,
        api_key_id: Option<String>,
    ) {
        if self.usage_rx.is_some() {
            return;
        }
        let DataLayer::Ready(store) = &self.data else {
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();
        let id = account_id.to_owned();
        let (start, end) = range.dates();
        let granularity = range.granularity();

        self.usage
            .entry((id.clone(), range, api_key_id.clone()))
            .or_default()
            .loading = true;
        let (sender, receiver) = std::sync::mpsc::channel();
        self.usage_rx = Some((id.clone(), range, api_key_id.clone(), receiver));
        let repaint = ctx.clone();

        std::thread::spawn(move || {
            let result =
                match crate::secrets::open_credentials(secret_store.as_ref(), &id, &secrets_blob) {
                    Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                        Ok(client) => client
                            .usage_trend(
                                &credentials.access_token,
                                &start,
                                &end,
                                granularity,
                                api_key_id.as_deref(),
                            )
                            .map_err(|err| err.user_message()),
                        Err(err) => Err(err.user_message()),
                    },
                    Err(err) => Err(format!("凭据无法解密：{err:#}")),
                };
            let _ = sender.send(result);
            repaint.request_repaint();
        });
    }

    /// 取一次用量拉取结果。
    fn poll_usage(&mut self, ctx: &egui::Context) {
        let Some((account_id, range, api_key_id, receiver)) = self.usage_rx.as_ref() else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        let key = (account_id.clone(), *range, api_key_id.clone());
        self.usage_rx = None;

        let entry = self.usage.entry(key).or_default();
        entry.loading = false;
        match result {
            Ok(trend) => {
                let raw_count = trend.points.len();
                entry.points = crate::api::fill_usage_gaps(&trend);
                log::debug!(
                    "用量曲线已补齐：原始 {raw_count} 个时间桶，绘制 {} 个",
                    entry.points.len()
                );
                entry.error = None;
                entry.fetched_at = Some(Instant::now());
            }
            Err(message) => {
                entry.error = Some(message);
                // 失败也算"刚试过"：否则每帧都会重试，把 Heavy 限流接口打爆
                entry.fetched_at = Some(Instant::now());
            }
        }
        ctx.request_repaint();
    }

    /// 为展开且未折叠的账号补齐用量曲线（过期或缺失才拉；同一时间只拉一个）。
    fn maybe_request_usage(&mut self, ctx: &egui::Context) {
        if self.usage_rx.is_some() {
            return;
        }
        let ids: Vec<String> = match &self.data {
            DataLayer::Ready(store) => store
                .accounts()
                .iter()
                .filter(|item| item.provider == AccountProvider::Sub2Api)
                .filter(|item| self.expanded_accounts.contains(&item.id))
                .filter(|item| !self.usage_collapsed.contains(&item.id))
                .map(|item| item.id.clone())
                .collect(),
            DataLayer::Unavailable(_) => Vec::new(),
        };

        for id in ids {
            let range = self.usage_range_of(&id);
            let api_key_id = self.usage_api_key_of(&id);
            let needs_fetch = match self.usage.get(&(id.clone(), range, api_key_id.clone())) {
                Some(series) if series.loading => false,
                Some(series) => series
                    .fetched_at
                    .map(|at| at.elapsed() > USAGE_TTL)
                    .unwrap_or(true),
                None => true,
            };
            if needs_fetch {
                self.request_usage(ctx, &id, range, api_key_id);
                return;
            }
        }
    }

    /// 为已展开但还没拉到 Key 的账号发起拉取。
    ///
    /// 重启后可能同时有多个账号处于展开状态；这里按并发上限一次启动多个请求，
    /// 每个请求结束后都会再次调用本函数，把剩余账号补上。
    fn request_next_keys(&mut self, ctx: &egui::Context) {
        let slots = MAX_CONCURRENT_KEY_FETCHES.saturating_sub(self.keys_rx.len());
        if slots == 0 {
            return;
        }
        let pending: Vec<String> = match &self.data {
            DataLayer::Ready(store) => store
                .accounts()
                .iter()
                .filter(|item| item.provider == AccountProvider::Sub2Api)
                .filter(|item| self.expanded_accounts.contains(&item.id))
                .map(|item| item.id.clone())
                .filter(|id| !self.account_keys.contains_key(id) && !self.keys_rx.contains_key(id))
                .collect(),
            DataLayer::Unavailable(_) => Vec::new(),
        };
        for id in pending.into_iter().take(slots) {
            self.request_account_keys(ctx, &id);
        }
    }

    /// 收取所有已完成的"面板展开"Key 拉取结果。
    ///
    /// 多个账号并行时，完成顺序不固定；逐条取出已完成的请求，未完成的留到下一帧。
    fn poll_account_keys(&mut self, ctx: &egui::Context) {
        if self.keys_rx.is_empty() {
            return;
        }

        let account_ids: Vec<String> = self.keys_rx.keys().cloned().collect();
        let mut changed = false;
        for account_id in account_ids {
            let received = self
                .keys_rx
                .get(&account_id)
                .map(std::sync::mpsc::Receiver::try_recv);
            match received {
                Some(Ok(reply)) => {
                    self.keys_rx.remove(&account_id);
                    self.apply_account_keys_reply(&account_id, reply);
                    changed = true;
                }
                Some(Err(TryRecvError::Empty)) => {}
                Some(Err(TryRecvError::Disconnected)) => {
                    self.keys_rx.remove(&account_id);
                    self.account_keys.insert(
                        account_id,
                        AccountKeysState {
                            loading: false,
                            cached: false,
                            error: Some("拉取 Key 的线程意外退出".to_owned()),
                            keys: Vec::new(),
                        },
                    );
                    changed = true;
                }
                None => {}
            }
        }

        if changed {
            // 有位置空出来后补上剩余展开账号，保持最多 4 个请求并行。
            self.request_next_keys(ctx);
            // Key 列表变化会影响托盘的「主账号 / 主 Key」菜单。
            self.rebuild_menu();
            ctx.request_repaint();
        }
    }

    /// 把单个账号的 Key 列表结果写进面板缓存。
    fn apply_account_keys_reply(&mut self, account_id: &str, reply: ApiKeysReply) {
        let ApiKeysReply::List(result) = reply;
        let state = match result {
            Ok(keys) => {
                // Key 可能已在网页端被删除：选中的旧 Key 不存在时回退到全部 Key。
                if let Some(selected) = self.usage_api_key.get(account_id)
                    && !keys.iter().any(|key| &key.id == selected)
                {
                    self.usage_api_key.remove(account_id);
                }
                self.cache_api_keys(account_id, &keys);
                AccountKeysState {
                    loading: false,
                    cached: false,
                    error: None,
                    keys,
                }
            }
            Err(message) => {
                // 拉取失败时回退到 accounts.json 里的缓存，至少让用户看到上次结果
                let cached_keys = self.cached_api_keys(account_id);
                if cached_keys.is_empty() {
                    AccountKeysState {
                        loading: false,
                        cached: false,
                        error: Some(message),
                        keys: Vec::new(),
                    }
                } else {
                    AccountKeysState {
                        loading: false,
                        cached: true,
                        error: Some(format!("{message}（下面是上次的缓存）")),
                        keys: cached_keys,
                    }
                }
            }
        };
        self.account_keys.insert(account_id.to_owned(), state);
    }

    /// 后台检查 GitHub 上是否有新版本。
    fn start_update_check(&mut self, ctx: &egui::Context) {
        if self.update_rx.is_some() {
            return;
        }
        self.update_checking = true;
        self.update_result = Some("正在检查…".to_owned());

        let (sender, receiver) = std::sync::mpsc::channel();
        self.update_rx = Some(receiver);
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = crate::update::check_latest(std::time::Duration::from_secs(10));
            let _ = sender.send(result);
            repaint.request_repaint();
        });
    }

    /// 取一次更新检查结果。
    fn poll_update(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.update_rx.as_ref() else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        self.update_rx = None;
        self.update_checking = false;

        match result {
            Ok(Some(info)) => {
                if crate::update::is_newer(crate::APP_VERSION, &info.version) {
                    self.update_result = Some(format!(
                        "发现新版本 v{}（当前 v{}）",
                        info.version,
                        crate::APP_VERSION
                    ));
                    self.update_url = Some(info.url);
                } else {
                    self.update_result = Some(format!("已是最新版本 v{}", crate::APP_VERSION));
                    self.update_url = None;
                }
            }
            Ok(None) => self.update_result = Some("远端还没有发布记录".to_owned()),
            Err(message) => self.update_result = Some(format!("检查失败：{message}")),
        }
        ctx.request_repaint();
    }

    /// 点击 Key 方块：后台取完整值，成功后写剪贴板并弹出"已复制"浮窗。
    fn request_copy(&mut self, ctx: &egui::Context, account_id: &str, key_id: &str) {
        if self.copy_rx.is_some() {
            return;
        }

        let DataLayer::Ready(store) = &self.data else {
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();
        let account_for_thread = account_id.to_owned();
        let key_for_thread = key_id.to_owned();

        self.copying_key = Some(key_id.to_owned());
        let (sender, receiver) = std::sync::mpsc::channel();
        self.copy_rx = Some(receiver);
        let repaint = ctx.clone();

        std::thread::spawn(move || {
            let result = match crate::secrets::open_credentials(
                secret_store.as_ref(),
                &account_for_thread,
                &secrets_blob,
            ) {
                Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                    Ok(client) => client
                        .api_key_value(&credentials.access_token, &key_for_thread)
                        .map_err(|err| err.user_message()),
                    Err(err) => Err(err.user_message()),
                },
                Err(err) => Err(format!("凭据无法解密：{err:#}")),
            };
            let _ = sender.send((account_for_thread, key_for_thread, result));
            repaint.request_repaint();
        });
    }

    /// 取一次复制结果：成功则写剪贴板 + 记录浮窗。
    fn poll_copy(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.copy_rx.as_ref() else {
            return;
        };
        let received = receiver.try_recv();
        let (account_id, key_id, result) = match received {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.copy_rx = None;
                self.copying_key = None;
                self.toast = Some(("复制线程意外退出".to_owned(), Instant::now()));
                return;
            }
        };

        self.copy_rx = None;
        self.copying_key = None;

        match result {
            Ok(value) => {
                // 完整 Key 只进剪贴板，不写日志、不落盘
                ctx.copy_text(value);
                self.copy_notice = Some((account_id, key_id, Instant::now()));
                // 浮窗显示期间要有重绘，否则到期不会消失
                ctx.request_repaint_after(Duration::from_millis(1700));
            }
            Err(message) => {
                self.toast = Some((format!("复制失败：{message}"), Instant::now()));
            }
        }
        ctx.request_repaint();
    }

    /// 打开某个 Key 的调额度弹窗。
    ///
    /// 只准备编辑状态，不发请求；真正保存由用户点击按钮后调用
    /// [`Self::start_quota_update`] 发起，避免打开弹窗就产生副作用。
    fn open_quota_editor(&mut self, target: QuotaEditTarget) {
        self.quota_edit = Some(QuotaEditState {
            account_id: target.account_id,
            key_id: target.key_id,
            key_name: target.key_name,
            key_masked: target.key_masked,
            quota_input: format_quota_input(target.current_quota),
            current_quota: target.current_quota,
            quota_used: target.quota_used,
            reset_quota: false,
            submitting: false,
            error: None,
        });
    }

    /// 后台提交某个 Key 的额度更新。
    ///
    /// 网络请求必须离开 egui 线程，否则面板会卡住。凭据只在后台线程内解密，
    /// 不写日志、不进入弹窗状态。调用前 [`QuotaEditState::submitting`] 会被置真，
    /// 结果由 [`Self::poll_quota_update`] 回收。
    fn start_quota_update(&mut self, ctx: &egui::Context, quota: f64) {
        if self.quota_update_rx.is_some() {
            return;
        }
        let Some(edit) = self.quota_edit.as_ref() else {
            return;
        };
        let account_id = edit.account_id.clone();
        let key_id = edit.key_id.clone();
        let reset_quota = edit.reset_quota;

        let DataLayer::Ready(store) = &self.data else {
            if let Some(edit) = self.quota_edit.as_mut() {
                edit.error = Some("数据目录不可用，无法读取登录凭据".to_owned());
            }
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            if let Some(edit) = self.quota_edit.as_mut() {
                edit.error = Some("账号已不存在，请关闭弹窗后刷新".to_owned());
            }
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();

        if let Some(edit) = self.quota_edit.as_mut() {
            edit.submitting = true;
            edit.error = None;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        self.quota_update_rx = Some(receiver);
        let repaint = ctx.clone();
        let account_for_thread = account_id.clone();
        let key_for_thread = key_id.clone();

        std::thread::spawn(move || {
            let result = match crate::secrets::open_credentials(
                secret_store.as_ref(),
                &account_for_thread,
                &secrets_blob,
            ) {
                Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                    Ok(client) => client
                        .update_api_key_quota(
                            &credentials.access_token,
                            &key_for_thread,
                            quota,
                            reset_quota,
                        )
                        .map_err(|err| err.user_message()),
                    Err(err) => Err(err.user_message()),
                },
                Err(err) => Err(format!("凭据无法解密：{err:#}")),
            };
            let _ = sender.send((account_for_thread, key_for_thread, result));
            repaint.request_repaint();
        });
    }

    /// 回收额度更新结果，写回面板缓存与 `accounts.json`。
    ///
    /// 成功时关闭弹窗并给出底部提示；失败时保留弹窗与用户输入，
    /// 让用户可以修正金额后直接重试，而不是重新找 Key。
    fn poll_quota_update(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.quota_update_rx.as_ref() else {
            return;
        };
        let received = receiver.try_recv();
        let (account_id, key_id, result) = match received {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.quota_update_rx = None;
                if let Some(edit) = self.quota_edit.as_mut() {
                    edit.submitting = false;
                    edit.error = Some("额度更新线程意外退出".to_owned());
                }
                ctx.request_repaint();
                return;
            }
        };
        self.quota_update_rx = None;

        match result {
            Ok(updated) => {
                self.apply_api_key_update(&account_id, &updated);
                if self
                    .quota_edit
                    .as_ref()
                    .is_some_and(|edit| edit.account_id == account_id && edit.key_id == key_id)
                {
                    self.quota_edit = None;
                }
                self.toast = Some((format!("已更新「{}」的额度", updated.name), Instant::now()));
            }
            Err(message) => {
                if let Some(edit) = self.quota_edit.as_mut()
                    && edit.account_id == account_id
                    && edit.key_id == key_id
                {
                    edit.submitting = false;
                    edit.error = Some(message);
                } else {
                    self.toast = Some((format!("额度更新失败：{message}"), Instant::now()));
                }
            }
        }
        ctx.request_repaint();
    }

    /// 把更新后的 Key 摘要写回展开态缓存，并同步 `accounts.json`。
    ///
    /// 这里只替换单个 Key，不重新拉整张列表：额度接口已经返回更新后的对象，
    /// 重拉会让用户刚保存就短暂看到加载态，没有必要。若 Key 已从网页端删除，
    /// 则把它补进列表，保证界面与服务端响应一致。
    fn apply_api_key_update(&mut self, account_id: &str, updated: &crate::api::ApiKeyInfo) {
        let keys = self.account_keys.get_mut(account_id).map(|state| {
            if let Some(existing) = state.keys.iter_mut().find(|key| key.id == updated.id) {
                *existing = updated.clone();
            } else {
                state.keys.push(updated.clone());
            }
            // 服务端刚刚确认过 Key 存在，之前列表失败留下的错误/缓存标记失效。
            state.cached = false;
            state.error = None;
            state.keys.clone()
        });
        if let Some(keys) = keys {
            self.cache_api_keys(account_id, &keys);
        }
    }

    /// 打开删除 Key 的二次确认弹窗。
    ///
    /// 删除 Key 不可恢复，且会让正在使用该 Key 的客户端立即失效，因此不能
    /// 点击后直接发请求；这里只准备确认弹窗所需信息。
    fn open_delete_key_confirm(&mut self, target: DeleteKeyTarget) {
        self.delete_key = Some(DeleteKeyState {
            account_id: target.account_id,
            key_id: target.key_id,
            key_name: target.key_name,
            key_masked: target.key_masked,
            submitting: false,
            error: None,
        });
    }

    /// 后台执行删除 Key。
    ///
    /// 删除请求同样必须离开 egui 线程；凭据只在后台线程解密，不写入日志。
    /// 成功后会同时清理面板缓存、`accounts.json` 缓存与用量筛选状态。
    fn start_delete_key(&mut self, ctx: &egui::Context) {
        if self.delete_key_rx.is_some() {
            return;
        }
        let Some(state) = self.delete_key.as_ref() else {
            return;
        };
        let account_id = state.account_id.clone();
        let key_id = state.key_id.clone();

        let DataLayer::Ready(store) = &self.data else {
            if let Some(state) = self.delete_key.as_mut() {
                state.error = Some("数据目录不可用，无法读取登录凭据".to_owned());
            }
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            if let Some(state) = self.delete_key.as_mut() {
                state.error = Some("账号已不存在，请关闭弹窗后刷新".to_owned());
            }
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();

        if let Some(state) = self.delete_key.as_mut() {
            state.submitting = true;
            state.error = None;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        self.delete_key_rx = Some(receiver);
        let repaint = ctx.clone();
        let account_for_thread = account_id.clone();
        let key_for_thread = key_id.clone();

        std::thread::spawn(move || {
            let result = match crate::secrets::open_credentials(
                secret_store.as_ref(),
                &account_for_thread,
                &secrets_blob,
            ) {
                Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                    Ok(client) => client
                        .delete_api_key(&credentials.access_token, &key_for_thread)
                        .map_err(|err| err.user_message()),
                    Err(err) => Err(err.user_message()),
                },
                Err(err) => Err(format!("凭据无法解密：{err:#}")),
            };
            let _ = sender.send((account_for_thread, key_for_thread, result));
            repaint.request_repaint();
        });
    }

    /// 回收删除 Key 的结果。
    fn poll_delete_key(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.delete_key_rx.as_ref() else {
            return;
        };
        let received = receiver.try_recv();
        let (account_id, key_id, result) = match received {
            Ok(message) => message,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.delete_key_rx = None;
                if let Some(state) = self.delete_key.as_mut() {
                    state.submitting = false;
                    state.error = Some("删除线程意外退出".to_owned());
                }
                ctx.request_repaint();
                return;
            }
        };
        self.delete_key_rx = None;

        match result {
            Ok(()) => {
                let key_name = self
                    .delete_key
                    .as_ref()
                    .filter(|state| state.account_id == account_id && state.key_id == key_id)
                    .map(|state| state.key_name.clone())
                    .unwrap_or_else(|| "API Key".to_owned());
                self.apply_api_key_delete(&account_id, &key_id);
                self.delete_key = None;
                self.toast = Some((format!("已删除「{key_name}」"), Instant::now()));
                self.maybe_request_usage(ctx);
            }
            Err(message) => {
                if let Some(state) = self.delete_key.as_mut()
                    && state.account_id == account_id
                    && state.key_id == key_id
                {
                    state.submitting = false;
                    state.error = Some(message);
                } else {
                    self.toast = Some((format!("删除失败：{message}"), Instant::now()));
                }
            }
        }
        ctx.request_repaint();
    }

    /// 从面板缓存与 `accounts.json` 中移除已删除的 Key。
    fn apply_api_key_delete(&mut self, account_id: &str, key_id: &str) {
        let keys = self.account_keys.get_mut(account_id).map(|state| {
            state.keys.retain(|key| key.id != key_id);
            state.keys.clone()
        });
        if let Some(keys) = keys {
            self.cache_api_keys(account_id, &keys);
        }

        // 已删除的 Key 不能再作为用量筛选条件，否则曲线会一直查旧 id。
        if self.usage_api_key.get(account_id).map(String::as_str) == Some(key_id) {
            self.usage_api_key.remove(account_id);
        }
        self.usage.retain(|(cached_account, _, api_key), _| {
            !(cached_account == account_id && api_key.as_deref() == Some(key_id))
        });
    }

    /// 打开凭据损坏账号的原地重新登录弹窗。
    ///
    /// 账号 id 与 Key 缓存保持不变，只替换加密凭据；成功后立即触发一轮刷新。
    fn open_relogin(&mut self, ctx: &egui::Context, account_id: &str) {
        let DataLayer::Ready(store) = &self.data else {
            self.toast = Some(("数据目录不可用，无法重新登录".to_owned(), Instant::now()));
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            self.toast = Some(("账号已不存在，请刷新后重试".to_owned(), Instant::now()));
            return;
        };
        self.login.open_for_relogin(
            account.provider,
            account.id.clone(),
            account.base_url.clone(),
            account.email.clone(),
        );
        self.show_panel(ctx);
        ctx.request_repaint();
    }

    /// 打开某个账号的「添加 API Key」弹窗，并异步加载可用分组。
    fn open_create_key_modal(&mut self, ctx: &egui::Context, account_id: &str) {
        let DataLayer::Ready(store) = &self.data else {
            self.toast = Some(("数据目录不可用，无法创建 Key".to_owned(), Instant::now()));
            return;
        };
        if !store.accounts().iter().any(|item| item.id == account_id) {
            self.toast = Some(("账号已不存在，请刷新后重试".to_owned(), Instant::now()));
            return;
        }

        self.create_key = Some(CreateKeyState {
            account_id: account_id.to_owned(),
            name: "新 API Key".to_owned(),
            quota_input: "0".to_owned(),
            expiry_mode: ExpiryMode::Days,
            expires_in_days_input: "0".to_owned(),
            expires_date: jiff::Zoned::now().date(),
            expires_hour: 23,
            expires_minute: 59,
            groups: Vec::new(),
            selected_group_id: None,
            loading_groups: true,
            submitting: false,
            error: None,
            created: None,
        });
        self.start_load_create_groups(ctx);
    }

    /// 后台加载当前账号可用于创建 Key 的分组。
    fn start_load_create_groups(&mut self, ctx: &egui::Context) {
        if self.create_key_rx.is_some() {
            return;
        }
        let Some(state) = self.create_key.as_ref() else {
            return;
        };
        let account_id = state.account_id.clone();
        let DataLayer::Ready(store) = &self.data else {
            if let Some(state) = self.create_key.as_mut() {
                state.loading_groups = false;
                state.error = Some("数据目录不可用，无法读取登录凭据".to_owned());
            }
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            if let Some(state) = self.create_key.as_mut() {
                state.loading_groups = false;
                state.error = Some("账号已不存在".to_owned());
            }
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();

        if let Some(state) = self.create_key.as_mut() {
            state.loading_groups = true;
            state.error = None;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        self.create_key_rx = Some(receiver);
        let repaint = ctx.clone();
        let account_for_thread = account_id.clone();

        std::thread::spawn(move || {
            let result = match crate::secrets::open_credentials(
                secret_store.as_ref(),
                &account_for_thread,
                &secrets_blob,
            ) {
                Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                    Ok(client) => client
                        .list_available_groups(&credentials.access_token)
                        .map_err(|err| err.user_message()),
                    Err(err) => Err(err.user_message()),
                },
                Err(err) => Err(format!("凭据无法解密：{err:#}")),
            };
            let _ = sender.send(CreateKeyReply::Groups(account_for_thread, result));
            repaint.request_repaint();
        });
    }

    /// 提交创建 API Key 请求。
    fn start_create_key(&mut self, ctx: &egui::Context) {
        if self.create_key_rx.is_some() {
            return;
        }
        let Some(state) = self.create_key.as_ref() else {
            return;
        };
        let Some(group_id) = state.selected_group_id else {
            if let Some(state) = self.create_key.as_mut() {
                state.error = Some("请选择分组".to_owned());
            }
            return;
        };
        let name = state.name.trim().to_owned();
        if name.is_empty() {
            if let Some(state) = self.create_key.as_mut() {
                state.error = Some("API Key 名称不能为空".to_owned());
            }
            return;
        }
        let Ok(quota) = parse_quota_amount(&state.quota_input) else {
            if let Some(state) = self.create_key.as_mut() {
                state.error = Some("额度必须是大于或等于 0 的有效数字".to_owned());
            }
            return;
        };
        let (expires_in_days, expires_at) = match state.expiry_mode {
            ExpiryMode::Days => {
                let days = match parse_expires_days(&state.expires_in_days_input) {
                    Ok(value) => value,
                    Err(message) => {
                        if let Some(state) = self.create_key.as_mut() {
                            state.error = Some(message);
                        }
                        return;
                    }
                };
                (days, None)
            }
            ExpiryMode::DateTime => {
                let target = match selected_expiry_zoned(state) {
                    Ok(value) => value,
                    Err(message) => {
                        if let Some(state) = self.create_key.as_mut() {
                            state.error = Some(message);
                        }
                        return;
                    }
                };
                let now = jiff::Zoned::now();
                if target <= now {
                    if let Some(state) = self.create_key.as_mut() {
                        state.error = Some("结束时间必须晚于当前时间".to_owned());
                    }
                    return;
                }
                let seconds = (target.timestamp().as_second() - now.timestamp().as_second()).max(1);
                let days = u32::try_from((seconds + 86_399) / 86_400).unwrap_or(u32::MAX);
                (Some(days.max(1)), Some(target.timestamp().to_string()))
            }
        };
        let account_id = state.account_id.clone();

        let DataLayer::Ready(store) = &self.data else {
            if let Some(state) = self.create_key.as_mut() {
                state.error = Some("数据目录不可用，无法读取登录凭据".to_owned());
            }
            return;
        };
        let Some(account) = store.accounts().iter().find(|item| item.id == account_id) else {
            if let Some(state) = self.create_key.as_mut() {
                state.error = Some("账号已不存在".to_owned());
            }
            return;
        };
        let base_url = account.base_url.clone();
        let secrets_blob = account.secrets.clone();
        let secret_store = store.secret_store();
        let timeout = self.request_timeout();

        if let Some(state) = self.create_key.as_mut() {
            state.submitting = true;
            state.error = None;
        }

        let (sender, receiver) = std::sync::mpsc::channel();
        self.create_key_rx = Some(receiver);
        let repaint = ctx.clone();
        let account_for_thread = account_id.clone();

        std::thread::spawn(move || {
            let result = match crate::secrets::open_credentials(
                secret_store.as_ref(),
                &account_for_thread,
                &secrets_blob,
            ) {
                Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                    Ok(client) => client
                        .create_api_key(
                            &credentials.access_token,
                            &name,
                            group_id,
                            quota,
                            expires_in_days,
                            expires_at.as_deref(),
                        )
                        .map_err(|err| err.user_message()),
                    Err(err) => Err(err.user_message()),
                },
                Err(err) => Err(format!("凭据无法解密：{err:#}")),
            };
            let _ = sender.send(CreateKeyReply::Created(
                account_for_thread,
                Box::new(result),
            ));
            repaint.request_repaint();
        });
    }

    /// 回收「添加 API Key」的分组加载与创建结果。
    fn poll_create_key(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.create_key_rx.as_ref() else {
            return;
        };
        let received = receiver.try_recv();
        let reply = match received {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                self.create_key_rx = None;
                if let Some(state) = self.create_key.as_mut() {
                    state.loading_groups = false;
                    state.submitting = false;
                    state.error = Some("创建 Key 的后台线程意外退出".to_owned());
                }
                ctx.request_repaint();
                return;
            }
        };
        self.create_key_rx = None;

        match reply {
            CreateKeyReply::Groups(account_id, result) => {
                if let Some(state) = self.create_key.as_mut()
                    && state.account_id == account_id
                {
                    state.loading_groups = false;
                    match result {
                        Ok(groups) => {
                            state.selected_group_id = groups.first().map(|group| group.id);
                            state.groups = groups;
                            if state.groups.is_empty() {
                                state.error = Some(
                                    "当前账号没有可绑定分组，请先在 Web 后台检查分组权限"
                                        .to_owned(),
                                );
                            }
                        }
                        Err(message) => state.error = Some(message),
                    }
                }
            }
            CreateKeyReply::Created(account_id, result) => {
                let applies = self
                    .create_key
                    .as_ref()
                    .is_some_and(|state| state.account_id == account_id);
                if applies {
                    match *result {
                        Ok(created) => {
                            let key_name = created.info.name.clone();
                            self.apply_api_key_update(&account_id, &created.info);
                            ctx.copy_text(created.full_key.clone());
                            self.toast = Some((
                                format!("已创建「{key_name}」并复制完整 Key"),
                                Instant::now(),
                            ));
                            if let Some(state) = self.create_key.as_mut() {
                                state.submitting = false;
                                state.created = Some(created);
                                state.error = None;
                            }
                        }
                        Err(message) => {
                            if let Some(state) = self.create_key.as_mut() {
                                state.submitting = false;
                                state.error = Some(message);
                            }
                        }
                    }
                }
            }
        }
        ctx.request_repaint();
    }

    /// 切换"钉住"状态并写回 `app.json`（失败只记日志，不影响本次会话）。
    fn set_pinned(&mut self, ctx: &egui::Context, pinned: bool) {
        self.pinned = pinned;
        log::info!("面板置顶：{}", if pinned { "已钉住" } else { "已取消" });

        if let DataLayer::Ready(store) = &mut self.data {
            let mut config = store.config().clone();
            config.panel_pinned = pinned;
            if let Err(err) = crate::config::save(store.dir(), &config) {
                log::warn!("保存置顶状态失败：{err:#}");
            } else {
                *store.config_mut() = config;
            }
        }
        ctx.request_repaint();
    }

    /// 打开设置页：把当前配置拷进草稿，改动在点"保存"前不生效。
    fn apply_theme(&self, ctx: &egui::Context) {
        set_palette(self.dark_mode);
        apply_egui_visuals(ctx, self.dark_mode);
    }

    fn set_dark_mode(&mut self, ctx: &egui::Context, dark_mode: bool) {
        self.dark_mode = dark_mode;
        let result = if let DataLayer::Ready(store) = &mut self.data {
            let mut config = store.config().clone();
            config.theme = if dark_mode { "dark" } else { "light" }.to_owned();
            match crate::config::save(store.dir(), &config) {
                Ok(()) => {
                    *store.config_mut() = config;
                    Ok(())
                }
                Err(err) => Err(err),
            }
        } else {
            Ok(())
        };
        if let Err(err) = result {
            let message = format!("保存主题失败：{err:#}");
            log::warn!("{message}");
            self.toast = Some((message, Instant::now()));
        }
        self.apply_theme(ctx);
        ctx.request_repaint();
    }

    /// 设置页内实时预览主题。
    ///
    /// 只改当前进程的调色板与 egui 控件样式，不写 `app.json`；用户点保存后
    /// 才持久化，点取消或关闭设置页则回滚到打开前的主题。
    fn preview_dark_mode(&mut self, ctx: &egui::Context, dark_mode: bool) {
        if self.dark_mode == dark_mode {
            return;
        }
        self.dark_mode = dark_mode;
        self.apply_theme(ctx);
        ctx.request_repaint();
    }

    /// 关闭设置页时回滚未保存的主题预览。
    fn restore_settings_theme(&mut self, ctx: &egui::Context) {
        let Some(original) = self.settings_original_dark_mode.take() else {
            return;
        };
        if self.dark_mode != original {
            self.dark_mode = original;
            self.apply_theme(ctx);
            ctx.request_repaint();
        }
    }

    /// 后台读取开机自启状态。
    ///
    /// Windows 下 `reg query` 是阻塞子进程，某些机器上会被杀软/注册表代理拖到明显卡顿。
    /// 这里把它彻底移出 UI 线程，结果通过通道回传，面板先正常打开。
    fn start_autostart_check(&mut self, ctx: &egui::Context) {
        if self.autostart_rx.is_some() {
            return;
        }
        let (sender, receiver) = std::sync::mpsc::channel();
        self.autostart_rx = Some(receiver);
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let enabled = crate::autostart::is_enabled();
            let _ = sender.send(enabled);
            repaint.request_repaint();
        });
    }

    /// 收取后台读取的开机自启状态。
    fn poll_autostart(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.autostart_rx.as_ref() else {
            return;
        };
        match receiver.try_recv() {
            Ok(enabled) => {
                self.autostart_rx = None;
                self.autostart_enabled = Some(enabled);
                // 如果设置页正好开着，且读取期间用户不能编辑该开关，直接同步显示。
                if let Some(draft) = self.settings.as_mut() {
                    draft.autostart = enabled;
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.autostart_rx = None;
                log::warn!("读取开机自启状态的后台线程意外退出");
                ctx.request_repaint();
            }
        }
    }

    /// 打开设置页。
    ///
    /// 只读取内存里的自启缓存，不再在 UI 线程同步执行 `reg query`。
    fn open_settings(&mut self, ctx: &egui::Context) {
        let DataLayer::Ready(store) = &self.data else {
            self.toast = Some(("数据目录不可用，无法打开设置".to_owned(), Instant::now()));
            return;
        };
        let config = store.config();
        self.settings_original_dark_mode = Some(self.dark_mode);
        self.settings = Some(SettingsDraft {
            mask_accounts: config.mask_accounts,
            autostart: self.autostart_enabled.unwrap_or(false),
            refresh_interval_seconds: config.refresh_interval_seconds,
            request_timeout_seconds: config.request_timeout_seconds,
            dark_mode: self.dark_mode,
        });
        // 如果启动时的后台读取还没完成（或失败），打开设置页时再补一次。
        if self.autostart_enabled.is_none() {
            self.start_autostart_check(ctx);
        }
    }

    /// 在后台保存设置：写 app.json 与开机自启，避免阻塞 UI 线程。
    fn start_save_settings(&mut self, ctx: &egui::Context) {
        if self.settings_save_rx.is_some() {
            return;
        }
        let Some(draft) = self.settings.clone() else {
            return;
        };
        let Some(autostart_enabled) = self.autostart_enabled else {
            self.toast = Some((
                "正在读取开机自启状态，请稍后再保存".to_owned(),
                Instant::now(),
            ));
            return;
        };
        let DataLayer::Ready(store) = &self.data else {
            self.toast = Some(("数据目录不可用".to_owned(), Instant::now()));
            return;
        };

        let mut config = store.config().clone();
        config.mask_accounts = draft.mask_accounts;
        config.refresh_interval_seconds = draft.refresh_interval_seconds.clamp(30, 86400);
        config.request_timeout_seconds = draft.request_timeout_seconds.clamp(1, 120);
        config.theme = if draft.dark_mode { "dark" } else { "light" }.to_owned();
        let dir = store.dir().to_path_buf();

        let needs_autostart_write = draft.autostart != autostart_enabled;
        let executable = if needs_autostart_write {
            match std::env::current_exe() {
                Ok(path) => Some(path),
                Err(err) => {
                    self.toast = Some((format!("无法定位当前程序路径：{err}"), Instant::now()));
                    return;
                }
            }
        } else {
            None
        };

        let desired_autostart = draft.autostart;
        let (sender, receiver) = std::sync::mpsc::channel();
        self.settings_save_rx = Some(receiver);

        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<SettingsSaveResult, String> {
                crate::config::save(&dir, &config)
                    .map_err(|err| format!("保存 app.json 失败：{err:#}"))?;
                if let Some(executable) = executable {
                    crate::autostart::set_enabled(desired_autostart, &executable)
                        .map_err(|err| format!("设置开机自启失败：{err:#}"))?;
                }
                Ok(SettingsSaveResult {
                    config,
                    autostart: desired_autostart,
                })
            })();
            let _ = sender.send(result);
            ctx.request_repaint();
        });
    }

    /// 收取后台保存结果；成功后才更新内存配置并关闭设置页。
    fn poll_settings_save(&mut self, ctx: &egui::Context) {
        let reply = match self.settings_save_rx.as_ref() {
            Some(receiver) => receiver.try_recv(),
            None => return,
        };
        match reply {
            Ok(Ok(result)) => {
                self.settings_save_rx = None;
                if let DataLayer::Ready(store) = &mut self.data {
                    *store.config_mut() = result.config;
                }
                self.autostart_enabled = Some(result.autostart);
                if let Some(draft) = self.settings.as_ref() {
                    self.dark_mode = draft.dark_mode;
                }
                self.settings_original_dark_mode = None;
                self.settings = None;
                self.rebuild_menu();
                self.update_badge();
                self.toast = Some(("设置已保存".to_owned(), Instant::now()));
                ctx.request_repaint();
            }
            Ok(Err(message)) => {
                self.settings_save_rx = None;
                self.toast = Some((message, Instant::now()));
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.settings_save_rx = None;
                self.toast = Some(("保存设置的线程意外退出，请重试".to_owned(), Instant::now()));
                ctx.request_repaint();
            }
        }
    }

    /// 保存期间显示的阻塞式进度弹窗。
    fn draw_settings_saving(&mut self, ctx: &egui::Context) {
        egui::Modal::new(egui::Id::unique("settings-saving"))
            .backdrop_color(ui_style::modal_backdrop())
            .frame(ui_style::modal_frame())
            .show(ctx, |ui| {
                ui.set_width(ui_style::size::SETTINGS_WIDTH);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("正在保存设置")
                                .size(font::HEADING)
                                .strong()
                                .color(theme().text_primary),
                        );
                        ui_style::hint_label(ui, "正在写入 app.json 并更新系统设置，请稍候…");
                    });
                });
            });
    }

    /// 绘制设置页（打码开关、刷新间隔、请求超时）。
    fn draw_settings(&mut self, ctx: &egui::Context) {
        if self.settings_save_rx.is_some() {
            self.draw_settings_saving(ctx);
            return;
        }

        // 先拷贝出与更新检查相关的只读状态，避免与 settings 的可变借用冲突
        let update_checking = self.update_checking;
        let update_result = self.update_result.clone();
        let update_url = self.update_url.clone();
        let autostart_known = self.autostart_enabled.is_some();

        let Some(draft) = self.settings.as_mut() else {
            return;
        };

        let mut close = false;
        let mut save = false;
        let mut check_update = false;
        let mut copy_update_url: Option<String> = None;
        let mut preview_dark_mode: Option<bool> = None;

        let modal = egui::Modal::new(egui::Id::unique("settings-dialog"))
            .backdrop_color(ui_style::modal_backdrop())
            .frame(ui_style::modal_frame())
            .show(ctx, |ui| {
                ui.set_width(ui_style::size::MODAL_MAX_WIDTH);

                // 标题区与新增 Key、登录弹窗保持同一层级：品牌竖条 + 标题 + 说明。
                ui.horizontal(|ui| {
                    let (bar_rect, _) = ui.allocate_exact_size(
                        egui::vec2(
                            ui_style::size::ACCENT_BAR_WIDTH,
                            ui_style::size::ACCENT_BAR_HEIGHT,
                        ),
                        egui::Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        bar_rect,
                        egui::CornerRadius::same(ui_style::size::RADIUS_HAIRLINE),
                        theme().accent,
                    );
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("设置")
                                .size(font::HEADING)
                                .strong()
                                .color(theme().text_primary),
                        );
                        ui.label(
                            egui::RichText::new("保存后立即生效并写入 app.json")
                                .size(font::SMALL)
                                .color(theme().text_dim),
                        );
                    });
                });
                ui.add_space(ui_style::space::LG);

                let scroll_max_height = (ctx.content_rect().height()
                    - ui_style::size::SETTINGS_SCROLL_RESERVED)
                    .max(ui_style::size::SETTINGS_SCROLL_MIN);
                egui::ScrollArea::vertical()
                    .id_salt("settings-scroll")
                    .max_height(scroll_max_height)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());

                        ui_style::section_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui_style::section_title(ui, "外观");
                            ui.add_space(ui_style::space::XS);

                            ui.horizontal(|ui| {
                                ui_style::field_label(ui, "主题");
                                let light_text = egui::RichText::new("日间")
                                    .size(font::BODY)
                                    .color(if draft.dark_mode {
                                        theme().text_secondary
                                    } else {
                                        theme().on_accent
                                    });
                                if ui
                                    .add(ui_style::rich_button(
                                        light_text,
                                        if draft.dark_mode {
                                            ButtonTone::Ghost
                                        } else {
                                            ButtonTone::Primary
                                        },
                                        ButtonSize::Auto,
                                    ))
                                    .clicked()
                                {
                                    draft.dark_mode = false;
                                    preview_dark_mode = Some(false);
                                }
                                let dark_text = egui::RichText::new("夜间").size(font::BODY).color(
                                    if draft.dark_mode {
                                        theme().on_accent
                                    } else {
                                        theme().text_secondary
                                    },
                                );
                                if ui
                                    .add(ui_style::rich_button(
                                        dark_text,
                                        if draft.dark_mode {
                                            ButtonTone::Primary
                                        } else {
                                            ButtonTone::Ghost
                                        },
                                        ButtonSize::Auto,
                                    ))
                                    .clicked()
                                {
                                    draft.dark_mode = true;
                                    preview_dark_mode = Some(true);
                                }
                            });
                        });
                        ui.add_space(ui_style::space::MD);

                        ui_style::section_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui_style::section_title(ui, "账号与启动");
                            ui.add_space(ui_style::space::XS);
                            ui.checkbox(
                                &mut draft.mask_accounts,
                                egui::RichText::new("账号名打码显示（只影响界面）")
                                    .size(font::BODY)
                                    .color(theme().text_primary),
                            );
                            ui.add_space(ui_style::space::XS);
                            if autostart_known {
                                ui.checkbox(
                                    &mut draft.autostart,
                                    egui::RichText::new("开机自动启动（登录时运行）")
                                        .size(font::BODY)
                                        .color(theme().text_primary),
                                );
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui_style::hint_label(ui, "正在读取开机自启状态…");
                                });
                            }
                        });
                        ui.add_space(ui_style::space::MD);

                        ui_style::section_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui_style::section_title(ui, "刷新与网络");
                            ui.add_space(ui_style::space::XS);
                            ui.horizontal(|ui| {
                                ui_style::field_label(ui, "状态刷新间隔");
                                ui_style::drag_number(
                                    ui,
                                    &mut draft.refresh_interval_seconds,
                                    30..=86400,
                                    " 秒",
                                    10.0,
                                );
                            });
                            ui.add_space(ui_style::space::XS);
                            ui.horizontal(|ui| {
                                ui_style::field_label(ui, "请求超时");
                                ui_style::drag_number(
                                    ui,
                                    &mut draft.request_timeout_seconds,
                                    1..=120,
                                    " 秒",
                                    1.0,
                                );
                            });
                        });
                        ui.add_space(ui_style::space::MD);

                        ui_style::section_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui_style::section_title(ui, "版本更新");
                            ui.add_space(ui_style::space::XS);
                            ui.horizontal(|ui| {
                                if ui_style::add_button(
                                    ui,
                                    "检查更新",
                                    ButtonTone::Neutral,
                                    ButtonSize::Header,
                                )
                                .clicked()
                                {
                                    check_update = true;
                                }
                                if update_checking {
                                    ui.spinner();
                                    ui_style::hint_label(ui, "检查中…");
                                }
                            });
                            if let Some(message) = update_result.as_deref() {
                                ui_style::hint_label(ui, message);
                            }
                            if let Some(url) = update_url.as_deref() {
                                ui.add(
                                    egui::Hyperlink::from_label_and_url(
                                        egui::RichText::new("点击进入下载最新版本")
                                            .size(font::BODY)
                                            .color(theme().accent),
                                        url,
                                    )
                                    .open_in_new_tab(true),
                                );
                                ui_style::hint_label(ui, url);
                                if ui_style::add_button(
                                    ui,
                                    "复制下载链接",
                                    ButtonTone::Neutral,
                                    ButtonSize::Wide,
                                )
                                .clicked()
                                {
                                    copy_update_url = Some(url.to_owned());
                                }
                            }
                        });
                    });

                ui.add_space(ui_style::space::LG);
                ui.horizontal(|ui| {
                    if ui_style::add_enabled_button(
                        ui,
                        autostart_known,
                        "保存",
                        ButtonTone::Primary,
                        ButtonSize::Form,
                    )
                    .clicked()
                    {
                        save = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui_style::add_button(ui, "取消", ButtonTone::Neutral, ButtonSize::Form)
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });
            });

        if modal.backdrop_response.clicked() {
            close = true;
        }

        if let Some(dark_mode) = preview_dark_mode {
            self.preview_dark_mode(ctx, dark_mode);
        }
        if let Some(url) = copy_update_url {
            ctx.copy_text(url);
            self.toast = Some(("已复制下载链接".to_owned(), Instant::now()));
        }
        if check_update {
            self.start_update_check(ctx);
        }

        if save {
            self.start_save_settings(ctx);
        }
        if close {
            self.settings = None;
            self.restore_settings_theme(ctx);
        }
    }

    /// 删除账号（确认后调用）：删记录、清凭据、清运行态、重建菜单。
    fn delete_account(&mut self, id: &str) {
        let DataLayer::Ready(store) = &mut self.data else {
            return;
        };

        let display = store
            .accounts()
            .iter()
            .find(|account| account.id == id)
            .map(|account| store.display_email(&account.email))
            .unwrap_or_else(|| "该账号".to_owned());

        match store.accounts_mut().remove(id) {
            Ok(Some(removed)) => {
                // macOS 需要额外清钥匙串条目；DPAPI 下是空操作
                if let Err(err) = store.secrets().remove(&removed.id) {
                    log::warn!("清理凭据失败：{err:#}");
                }
                self.runtime.remove(id);
                self.toast = Some((format!("已删除 {display}"), Instant::now()));
                log::info!("账号已删除：{display}");
            }
            Ok(None) => log::debug!("账号 {id} 不存在，忽略删除"),
            Err(err) => {
                log::warn!("删除账号失败：{err:#}");
                self.toast = Some((format!("删除失败：{err:#}"), Instant::now()));
            }
        }

        // 账号没了，展开状态、Key 缓存与用量派生状态也要一起清掉。
        self.expanded_accounts.remove(id);
        self.account_keys.remove(id);
        self.usage_range.remove(id);
        self.usage_metric.remove(id);
        self.usage_api_key.remove(id);
        self.usage_collapsed.remove(id);
        self.usage.retain(|key, _| key.0 != id);

        self.rebuild_menu();
        self.update_badge();
    }

    /// 取一轮刷新结果并写入运行态。
    ///
    /// 刷新线程按"一轮全部跑完再回传"设计，因此这里拿到的是整轮结果；
    /// 若 token 被续期/重登，`rotated` 会带上新凭据，这里负责落库。
    fn poll_refresh(&mut self, ctx: &egui::Context) {
        let Some(receiver) = self.refresh_rx.as_ref() else {
            return;
        };

        match receiver.try_recv() {
            Ok(results) => {
                self.refresh_rx = None;
                self.last_round = Some(Instant::now());
                let now = Instant::now();
                let mut rotated_count = 0_usize;

                for result in results {
                    match result.outcome {
                        RefreshOutcome::Online {
                            status,
                            summary,
                            rotated,
                        } => {
                            self.runtime.apply_online(
                                &result.id,
                                status.role,
                                status.balance,
                                summary,
                                now,
                            );
                            if let Some(credentials) = rotated {
                                self.persist_rotated(&result.id, &credentials);
                                rotated_count += 1;
                            }
                        }
                        RefreshOutcome::Offline(message) => {
                            log::debug!("账号 {} 刷新失败（离线）：{message}", result.id);
                            self.runtime
                                .apply_error(&result.id, AccountStatus::Offline, message);
                        }
                        RefreshOutcome::CredentialsInvalid(message) => {
                            log::warn!("账号 {} 凭据损坏：{message}", result.id);
                            self.runtime.apply_error(
                                &result.id,
                                AccountStatus::CredentialsInvalid,
                                message,
                            );
                        }
                    }
                }

                if rotated_count > 0 {
                    log::info!("本轮有 {rotated_count} 个账号的凭据已更新并落库");
                }

                // 每轮记一行汇总，便于排障时确认"刷新确实跑过、结果是什么"
                log::info!(
                    "本轮状态刷新完成：{} 个账号，其中 {} 在线",
                    self.data_account_count(),
                    self.runtime.online_count()
                );

                self.rebuild_menu();
                self.update_badge();
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => {
                // 还在跑：下一帧继续看
            }
            Err(TryRecvError::Disconnected) => {
                // 线程异常退出：不要永远停在"刷新中"
                self.refresh_rx = None;
                self.last_round = Some(Instant::now());
                log::warn!("刷新线程意外退出，本轮跳过");
            }
        }
    }

    /// 把续期/重登得到的新凭据加密写回 `accounts.json`。
    ///
    /// 失败只记录日志：账号本身还能用，只是下次启动会重新走恢复链。
    fn persist_rotated(&mut self, id: &str, credentials: &crate::secrets::Credentials) {
        let DataLayer::Ready(store) = &mut self.data else {
            return;
        };

        let sealed = match crate::secrets::seal_credentials(store.secrets(), id, credentials) {
            Ok(sealed) => sealed,
            Err(err) => {
                log::warn!("账号 {id} 新凭据加密失败：{err:#}");
                return;
            }
        };

        match store.accounts_mut().update_credentials(
            id,
            sealed,
            None,
            Some(accounts::now_rfc3339()),
        ) {
            Ok(true) => log::info!("账号 {id} 的凭据已更新"),
            Ok(false) => log::debug!("账号 {id} 已被删除，跳过凭据落库"),
            Err(err) => log::warn!("账号 {id} 凭据落库失败：{err:#}"),
        }
    }

    /// 是否有账号"从未刷新过或数据已过期"。
    fn accounts_need_refresh(&self, now: Instant) -> bool {
        let DataLayer::Ready(store) = &self.data else {
            return false;
        };
        store.accounts().iter().any(|account| {
            self.runtime
                .get(&account.id)
                .map(|runtime| runtime.is_stale(now, STALE_AFTER))
                .unwrap_or(true)
        })
    }

    /// 定时路径：到点就起一轮刷新（同一时间只允许一轮）。
    fn maybe_start_refresh(&mut self, ctx: &egui::Context) {
        self.try_start_refresh(ctx, false);
    }

    /// 起一轮刷新；`force = true` 表示手动触发（跳过启动延迟与间隔判断）。
    ///
    /// 无论是否 `force`，都受"同一时间只跑一轮"约束：连点"立即刷新"不会并发发请求，
    /// 否则小实例很容易被自己的手动刷新打成 429。
    ///
    /// 自动触发的条件（满足任一）：
    ///
    /// * 距上一轮已超过 `refreshIntervalSeconds`；
    /// * 面板正打开，且有账号数据过期（超过 [`STALE_AFTER`]）。
    fn try_start_refresh(&mut self, ctx: &egui::Context, force: bool) {
        if self.refresh_rx.is_some() {
            if force {
                log::debug!("已有刷新在进行中，忽略本次手动刷新");
            }
            return;
        }

        let DataLayer::Ready(store) = &self.data else {
            return;
        };
        if store.accounts().is_empty() {
            return;
        }

        let now = Instant::now();
        if !force {
            // 首轮延迟：等托盘图标与窗口就绪，避免启动瞬间就发请求
            if now.saturating_duration_since(self.started_at) < FIRST_REFRESH_DELAY {
                return;
            }

            let interval = Duration::from_secs(store.config().refresh_interval_seconds.max(1));
            let due = refresh::should_start_round(now, self.last_round, interval);
            let stale_while_open = self.panel_visible && self.accounts_need_refresh(now);
            if !due && !stale_while_open {
                return;
            }
        }

        let targets = store
            .accounts()
            .iter()
            .map(refresh::RefreshTarget::from_account)
            .collect::<Vec<_>>();
        let secret_store = store.secret_store();
        let timeout = Duration::from_secs(store.config().request_timeout_seconds.max(1));

        // 先把状态切成"刷新中"，菜单立刻能反映出来
        for account in store.accounts() {
            self.runtime.mark(&account.id, AccountStatus::Refreshing);
        }
        self.refresh_rx = Some(refresh::spawn_round(targets, secret_store, timeout));
        log::info!(
            "开始新一轮状态刷新：{} 个账号（{}）",
            store.accounts().len(),
            if force { "手动" } else { "定时" }
        );

        self.rebuild_menu();
        self.update_badge();
        ctx.request_repaint();
    }

    /// 处理“点击面板外部自动收起”。
    ///
    /// `viewport().focused` 在部分平台可能拿不到（返回 `None`），此时按“仍然聚焦”处理，
    /// 宁可面板不自动收起，也不要让它无法使用。
    fn handle_focus(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if !self.panel_visible || focused {
            return;
        }

        // 钉住（置顶）时不因失焦收起：这是用户明确要求的"别关"
        if self.pinned && !self.debug_show_on_start {
            return;
        }

        // 登录弹窗打开时不收起面板：用户正在输密码，
        // 因为"去看了一眼别的窗口"就把面板连带表单一起关掉，等于让输入白费。
        if self.login.is_open() {
            return;
        }

        // 调试模式下不自动收起：自动化截图时窗口通常拿不到焦点，
        // 一旦按失焦处理，面板会在截图前就消失，验收就无从谈起。
        // 正常运行时这个分支不生效（未设置 SUB2API_TRAY_DEBUG_VISIBLE）。
        if self.debug_show_on_start {
            return;
        }

        // 宽限期内不响应：窗口刚弹出时 focused 可能还没更新
        let in_grace = self
            .shown_at
            .map(|at| at.elapsed() < FOCUS_GRACE)
            .unwrap_or(false);
        if !in_grace {
            log::debug!("面板失焦，自动收起");
            self.hide_panel(ctx);
        }
    }

    /// 处理 `Esc`：删除确认 > 登录弹窗 > 面板，按"最上层先关"的顺序。
    ///
    /// 放在 `logic()` 里而不是绘制里，是因为"关窗"是状态变更，
    /// 与这一帧是否需要绘制无关（面板隐藏时也要能响应）。
    fn handle_escape(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            return;
        }
        if self.settings.is_some() {
            if self.settings_save_rx.is_some() {
                return;
            }
            log::debug!("Esc：关闭设置页");
            self.settings = None;
            self.restore_settings_theme(ctx);
        } else if let Some(edit) = self.quota_edit.as_ref() {
            // 保存请求在途时 Esc 不关闭弹窗，避免用户以为已经取消但服务端仍在改额度。
            if !edit.submitting {
                log::debug!("Esc：关闭额度编辑弹窗");
                self.quota_edit = None;
            }
        } else if let Some(state) = self.create_key.as_ref() {
            // 创建请求在途时不关闭，避免用户错过一次性展示的完整 Key。
            if !state.submitting {
                log::debug!("Esc：关闭创建 API Key 弹窗");
                self.create_key = None;
            }
        } else if let Some(state) = self.delete_key.as_ref() {
            // 删除请求在途时同样不响应 Esc，避免误以为取消成功。
            if !state.submitting {
                log::debug!("Esc：关闭删除 Key 确认弹窗");
                self.delete_key = None;
            }
        } else if self.pending_delete.is_some() {
            log::debug!("Esc：取消删除确认");
            self.pending_delete = None;
        } else if self.login.is_open() {
            log::debug!("Esc：关闭登录弹窗");
            self.login.close();
        } else if self.panel_visible {
            log::debug!("Esc：收起面板");
            self.hide_panel(ctx);
        }
    }

    /// 取一次登录结果并落库。
    ///
    /// 落库失败（重复账号、磁盘满、加密失败）不会丢掉已输入的凭据：
    /// 弹窗保持打开并显示具体原因，用户可以改地址/账号后重试。
    fn poll_login(&mut self, ctx: &egui::Context) {
        let Some(draft) = self.login.poll() else {
            return;
        };
        let relogin_id = draft.relogin_account_id.clone();
        let mut should_refresh = false;

        match &mut self.data {
            DataLayer::Unavailable(reason) => {
                self.login
                    .set_error(format!("数据目录不可用，无法保存账号：{reason}"));
            }
            DataLayer::Ready(store) => {
                if let Some(account_id) = relogin_id {
                    if let Err(message) = save_relogin(store, &draft, &account_id) {
                        self.login.set_error(message);
                        return;
                    }
                    let display = if draft.provider == AccountProvider::DeepSeekPlatform {
                        draft.email.clone()
                    } else {
                        store.display_email(&draft.email)
                    };
                    log::info!("账号凭据已原地更新：{display}");
                    self.toast = Some((format!("已重新登录 {display}"), Instant::now()));
                    self.login.close();
                    self.runtime.remove(&account_id);
                    self.last_round = None;
                    self.rebuild_menu();
                    self.update_badge();
                    should_refresh = true;
                } else {
                    if let Err(message) = save_login(store, &draft) {
                        self.login.set_error(message);
                        return;
                    }

                    // 日志只留打码后的账号，避免明文邮箱长期堆积在日志里
                    let display = if draft.provider == AccountProvider::DeepSeekPlatform {
                        draft.email.clone()
                    } else {
                        store.display_email(&draft.email)
                    };
                    log::info!("账号已保存：{display}");
                    self.toast = Some((format!("已登录 {display}"), Instant::now()));
                    self.login.close();
                    // 新账号要立刻出现在托盘菜单里，并尽快刷新一次状态
                    self.rebuild_menu();
                    self.update_badge();
                    self.last_round = None;
                }
            }
        }

        if should_refresh {
            // 重新登录成功后立即验证新凭据，不必等下一轮定时刷新。
            self.try_start_refresh(ctx, true);
        }
        ctx.request_repaint();
    }

    /// 维护成功提示的过期。
    fn update_toast(&mut self, ctx: &egui::Context) {
        let Some((_, shown_at)) = &self.toast else {
            return;
        };
        let elapsed = shown_at.elapsed();
        if elapsed >= TOAST_DURATION {
            self.toast = None;
            ctx.request_repaint();
        } else {
            // 只在这一条提示消失时唤醒一次，避免空转重绘
            ctx.request_repaint_after(TOAST_DURATION - elapsed);
        }
    }

    /// 当前生效的请求超时（来自 `app.json`）。
    fn request_timeout(&self) -> Duration {
        let seconds = match &self.data {
            DataLayer::Ready(store) => store.config().request_timeout_seconds,
            // 数据层不可用时用内置默认值：弹窗此时不会真的发请求（入口被禁用）
            DataLayer::Unavailable(_) => crate::config::DEFAULT_REQUEST_TIMEOUT_SECONDS,
        };
        Duration::from_secs(seconds.max(1))
    }

    /// 弹出面板：先把窗口移到托盘图标旁边，再显示并抢焦点。
    fn show_panel(&mut self, ctx: &egui::Context) {
        // 顺序很重要：先移动、再显示，用户就不会看到面板从上一个位置“飞”过来
        let position = self.panel_position(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        // 主动聚焦：面板只有拿到焦点，才能在用户点击别处时收到失焦事件并自动收起
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);

        self.panel_visible = true;
        self.shown_at = Some(Instant::now());
        // 立刻请求重绘，避免等下一帧才看到面板
        ctx.request_repaint();
    }

    /// 收起面板。
    ///
    /// 只把窗口设为不可见，不销毁窗口、也不退出进程——进程要继续待在托盘里。
    fn hide_panel(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        self.panel_visible = false;
        self.shown_at = None;
        self.last_hidden_at = Some(Instant::now());
    }

    /// 是否刚在去抖窗口内收起过面板。
    fn recently_hidden(&self) -> bool {
        self.last_hidden_at
            .map(|at| at.elapsed() < TOGGLE_DEBOUNCE)
            .unwrap_or(false)
    }

    /// 计算面板左上角应该放在哪里（单位：egui 逻辑点）。
    ///
    /// 这里只做“把平台数据翻译成统一输入”的活：托盘图标矩形来自操作系统，单位是
    /// **物理像素**，而 egui 的窗口位置用**逻辑点**。在 125% / 150% 缩放的 Windows 上
    /// 两者相差 1.25 / 1.5 倍，直接混用会让面板明显偏移，所以先换算再交给
    /// [`compute_panel_position`] 这个纯函数处理（纯函数才能写单元测试）。
    fn panel_position(&self, ctx: &egui::Context) -> egui::Pos2 {
        let pixels_per_point = ctx.pixels_per_point().max(0.1);

        // 当前显示器的逻辑尺寸，用来把面板夹回屏幕内。
        // 拿不到时用一个保守的默认值，保证面板不会被算到负数坐标去。
        let monitor = ctx
            .input(|i| i.viewport().monitor_size)
            .unwrap_or_else(|| egui::vec2(1280.0, 800.0));

        let icon = self.tray.rect().map(|rect| IconBounds {
            left: rect.position.x as f32 / pixels_per_point,
            top: rect.position.y as f32 / pixels_per_point,
            right: (rect.position.x + f64::from(rect.size.width)) as f32 / pixels_per_point,
            bottom: (rect.position.y + f64::from(rect.size.height)) as f32 / pixels_per_point,
        });

        // 用窗口**当前**尺寸而不是初始常量：面板可以被用户缩放，
        // 定位必须跟着实际大小走，否则放大会顶出屏幕。
        let size = ctx
            .input(|i| i.viewport().inner_rect)
            .map(|rect| rect.size())
            .filter(|size| size.x > 1.0 && size.y > 1.0)
            .unwrap_or_else(|| egui::vec2(PANEL_WIDTH, PANEL_HEIGHT));

        compute_panel_position(icon, monitor, size)
    }

    /// 绘制整个面板。
    ///
    /// `ui` 是 eframe 提供的根 Ui（没有边距也没有背景），因此这里用 [`egui::CentralPanel`]
    /// 再包一层，才能得到带圆角/内边距的气泡外观。
    fn draw_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.apply_theme(ctx);
        let at_limit = self.data_account_count() >= MAX_ACCOUNTS;
        // 先在闭包外把需要的信息整理成"纯数据"：
        // 闭包内若再借用 `self.data`，会和 `&mut self` 的方法签名打架，
        // 提前算出字符串既避免了借用冲突，也让闭包只依赖局部变量。
        let storage_error = match &self.data {
            DataLayer::Unavailable(reason) => Some(reason.clone()),
            DataLayer::Ready(_) => None,
        };
        let toast = self.toast.as_ref().map(|(text, _)| text.clone());
        let cards = self.collect_cards();

        // 顶部按钮的点击结果：闭包里不能直接改 `self`，先记在局部变量里
        let mut open_login = false;
        // 总览卡片里「立即刷新」的点击结果
        let mut refresh_clicked = false;
        // 被点击要展开/收起的账号 id
        let mut toggle_account: Option<String> = None;
        // Key 列表里的复制与调额度动作：闭合绘制后再统一处理
        let mut key_actions = KeyUiActions::default();
        // 用量区交互：区间、API Key、指标与折叠
        let mut usage_actions = UsageUiActions::default();
        // 面板内的账号操作：请求删除（标题行右侧按钮触发）
        let mut delete_request: Option<String> = None;
        // 设置页入口的点击结果
        let mut open_settings = false;
        // 图钉（置顶）点击结果
        let mut toggle_pin = false;
        let mut toggle_theme = false;
        // 悬浮动画状态：先取出来，绘制闭包里才能可变借用
        let mut hover = std::mem::take(&mut self.hover);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    // 圆角 + 深色填充 + 细边框，构成气泡本体
                    .fill(theme().panel_bg)
                    .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_XL))
                    .stroke(egui::Stroke::new(1.0, theme().panel_border))
                    .inner_margin(egui::Margin::same(ui_style::size::MARGIN_LG)),
            )
            .show(ui, |ui| {
                // 顶部固定，其余内容滚动；操作反馈改为底部悬浮 Toast，
                // 不再占用一整条页脚高度。
                egui::Panel::top("panel_header")
                    .frame(egui::Frame::NONE.fill(theme().panel_bg))
                    .show(ui, |ui| {
                        draw_header(
                            ui,
                            &mut open_settings,
                            self.pinned,
                            &mut toggle_pin,
                            self.dark_mode,
                            &mut toggle_theme,
                        );

                        if let Some(reason) = storage_error.as_deref() {
                            ui.add_space(ui_style::space::XL);
                            draw_storage_banner(ui, reason);
                        }
                    });

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(ui_style::space::XL);

                        // 总览卡片始终显示（没有账号时也能从这里登录）
                        draw_account_summary(
                            ui,
                            &cards,
                            &mut open_login,
                            &mut refresh_clicked,
                            at_limit,
                        );
                        ui.add_space(ui_style::space::XL);

                        if cards.is_empty() {
                            draw_status_card(ui);
                            ui.add_space(ui_style::space::XXXL);
                            draw_empty_state(ui);
                        } else {
                            for card in &cards {
                                draw_account_card(
                                    ui,
                                    card,
                                    &mut toggle_account,
                                    &mut hover,
                                    &mut key_actions,
                                    &mut usage_actions,
                                    &mut delete_request,
                                );
                                ui.add_space(ui_style::space::LG);
                            }
                        }

                        // 给滚动区底部留一点空隙，避免最后一张卡片贴着页脚
                        ui.add_space(ui_style::space::SM);
                    });
            });

        // 操作反馈浮在面板底部上方，半透明、不参与布局，也不会再挤占滚动区。
        if let Some(text) = toast.as_deref() {
            draw_toast_overlay(ctx, text);
        }

        if open_login {
            self.login.open();
        }
        if refresh_clicked {
            // 手动刷新：绕过定时与启动延迟，但仍受"只跑一轮"约束
            self.try_start_refresh(ctx, true);
        }

        if let Some((account_id, range)) = usage_actions.range {
            self.set_usage_range(ctx, &account_id, range);
        }
        if let Some((account_id, api_key_id)) = usage_actions.api_key {
            self.set_usage_api_key(ctx, &account_id, api_key_id);
        }
        if let Some((account_id, metric)) = usage_actions.metric {
            self.set_usage_metric(ctx, &account_id, metric);
        }
        if let Some(account_id) = usage_actions.toggle_collapse {
            if !self.usage_collapsed.remove(&account_id) {
                self.usage_collapsed.insert(account_id);
            }
            self.maybe_request_usage(ctx);
            ctx.request_repaint();
        }

        if let Some((account_id, key_id)) = key_actions.copy_request.take() {
            self.request_copy(ctx, &account_id, &key_id);
        }

        if let Some(target) = key_actions.quota_request.take() {
            self.open_quota_editor(target);
        }

        if let Some(target) = key_actions.delete_request.take() {
            self.open_delete_key_confirm(target);
        }

        if let Some(account_id) = key_actions.create_key_request.take() {
            self.open_create_key_modal(ctx, &account_id);
        }

        if let Some(account_id) = key_actions.relogin_request.take() {
            self.open_relogin(ctx, &account_id);
        }

        if let Some(account_id) = delete_request {
            // 复用已有的二次确认弹窗
            self.pending_delete = Some(account_id);
            self.show_panel(ctx);
        }

        if let Some(account_id) = toggle_account {
            let expanded = !self.expanded_accounts.contains(&account_id);
            if expanded {
                self.expanded_accounts.insert(account_id.clone());
                log::debug!("展开账号 {account_id} 的 API Key");
                // 没有缓存就拉一次；有缓存则直接用
                if !self.account_keys.contains_key(&account_id) {
                    self.request_account_keys(ctx, &account_id);
                }
            } else {
                self.expanded_accounts.remove(&account_id);
                log::debug!("收起账号 {account_id} 的 API Key");
            }

            // 展开状态要跨重启保留：写进 accounts.json（失败只影响下次启动）
            if let DataLayer::Ready(store) = &mut self.data
                && let Err(err) = store.accounts_mut().set_expanded(&account_id, expanded)
            {
                log::warn!("保存展开状态失败：{err:#}");
            }
            ctx.request_repaint();
        }

        // 四角与四边都可以缩放；用全局指针判断，避免被滚动区抢走边缘事件。
        handle_window_resize(ctx, ctx.input(|input| input.viewport_rect()));
        draw_resize_ticks(ui);

        // 弹窗画在面板之后，保证它在最上层（Modal 自带遮罩与输入拦截）
        let timeout = self.request_timeout();
        self.login.show(ctx, timeout);

        // ---- 删除确认（从托盘子菜单进入） ----
        self.draw_delete_confirm(ctx);
        self.draw_quota_editor(ctx);
        self.draw_delete_key_confirm(ctx);
        self.draw_create_key_modal(ctx);

        if open_settings {
            self.open_settings(ctx);
        }

        if toggle_pin {
            self.set_pinned(ctx, !self.pinned);
        }
        if toggle_theme {
            self.set_dark_mode(ctx, !self.dark_mode);
        }

        // ---- 设置页 ----
        self.draw_settings(ctx);

        self.hover = hover;
    }

    /// 绘制调额度弹窗。
    ///
    /// 与删除确认一样使用项目内的 egui Modal，而不是系统对话框：风格统一，
    /// 也不会因为托盘窗口失焦而把弹窗丢到后台。保存期间禁用按钮并显示转圈，
    /// 避免用户重复提交；请求结果由 [`Self::poll_quota_update`] 回填。
    fn draw_quota_editor(&mut self, ctx: &egui::Context) {
        let Some(mut edit) = self.quota_edit.take() else {
            return;
        };

        let mut close = false;
        let mut submit = false;
        let current_text = if edit.current_quota > 0.0 {
            format!("${:.2}", edit.current_quota)
        } else {
            "不限额".to_owned()
        };
        let live_quota = parse_quota_amount(&edit.quota_input).ok();
        let will_exceed = live_quota
            .map(|quota| quota_will_exceed(quota, edit.quota_used, edit.reset_quota))
            .unwrap_or(false);

        let modal = egui::Modal::new(egui::Id::unique("quota-edit"))
            .backdrop_color(ui_style::modal_backdrop())
            .frame(
                egui::Frame::new()
                    .fill(theme().panel_bg)
                    .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_XL))
                    .stroke(egui::Stroke::new(1.0, theme().panel_border))
                    .inner_margin(egui::Margin::same(ui_style::size::MARGIN_LG)),
            )
            .show(ctx, |ui| {
                ui.set_width(ui_style::size::MODAL_MAX_WIDTH);
                ui.label(
                    egui::RichText::new("调整 API Key 额度")
                        .size(font::HEADING)
                        .strong()
                        .color(theme().text_primary),
                );
                ui.add_space(ui_style::space::XS);
                ui.label(
                    egui::RichText::new(format!("{}  {}", edit.key_name, edit.key_masked))
                        .size(font::BODY_SMALL)
                        .color(theme().text_secondary),
                );
                ui.add_space(ui_style::space::LG);
                ui.label(
                    egui::RichText::new(format!(
                        "已用 ${:.2} · 当前额度 {current_text}",
                        edit.quota_used
                    ))
                    .size(font::BODY_SMALL)
                    .color(theme().text_dim),
                );
                ui.add_space(ui_style::space::MD);

                ui.label(
                    egui::RichText::new("新额度（美元）")
                        .size(font::BODY_SMALL)
                        .color(theme().text_secondary),
                );
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("$")
                            .size(font::BODY)
                            .color(theme().text_dim),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut edit.quota_input)
                            .hint_text("0 表示不限额")
                            .desired_width(180.0),
                    );
                });
                ui.label(
                    egui::RichText::new("输入 0 表示不限额；修改额度不会自动清零已用额度。")
                        .size(font::SMALL)
                        .color(theme().text_dim),
                );

                if edit.quota_used > 0.0 {
                    ui.add_space(ui_style::space::MD);
                    ui.checkbox(
                        &mut edit.reset_quota,
                        egui::RichText::new("同时清零已用额度").size(font::BODY_SMALL),
                    );
                    ui.label(
                        egui::RichText::new("适合额度已耗尽、但想继续使用这个 Key 的情况。")
                            .size(font::SMALL)
                            .color(theme().text_dim),
                    );
                }

                if will_exceed {
                    ui.add_space(ui_style::space::MD);
                    ui.label(
                        egui::RichText::new(
                            "新额度低于已用额度，保存后该 Key 会立即达到上限；可勾选清零已用额度。",
                        )
                        .size(font::SMALL_PLUS)
                        .color(theme().danger),
                    );
                }

                if let Some(error) = &edit.error {
                    ui.add_space(ui_style::space::MD);
                    ui.label(
                        egui::RichText::new(error)
                            .size(font::SMALL_PLUS)
                            .color(theme().danger),
                    );
                }

                ui.add_space(ui_style::space::XXL);
                ui.horizontal(|ui| {
                    if ui_style::add_enabled_button(
                        ui,
                        !edit.submitting,
                        "取消",
                        ButtonTone::Neutral,
                        ButtonSize::Auto,
                    )
                    .clicked()
                    {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if edit.submitting {
                            "正在保存…"
                        } else {
                            "保存额度"
                        };
                        if ui_style::add_enabled_button(
                            ui,
                            !edit.submitting,
                            label,
                            ButtonTone::Primary,
                            ButtonSize::Auto,
                        )
                        .clicked()
                        {
                            submit = true;
                        }
                        if edit.submitting {
                            ui.spinner();
                        }
                    });
                });
            });

        if modal.backdrop_response.clicked() && !edit.submitting {
            close = true;
        }

        if submit {
            match parse_quota_amount(&edit.quota_input) {
                Ok(quota) => {
                    self.quota_edit = Some(edit);
                    self.start_quota_update(ctx, quota);
                }
                Err(message) => {
                    edit.error = Some(message);
                    self.quota_edit = Some(edit);
                }
            }
        } else if close {
            log::debug!("用户取消调整 API Key 额度");
        } else {
            self.quota_edit = Some(edit);
        }
    }

    /// 绘制「添加 API Key」弹窗。
    ///
    /// 创建成功后会切换到一次性结果页：显示完整 Key 并提供复制按钮。
    /// 完整 Key 只存在于该弹窗状态中，关闭后立即丢弃，不进入账号缓存。
    fn draw_create_key_modal(&mut self, ctx: &egui::Context) {
        let Some(mut state) = self.create_key.take() else {
            return;
        };

        let mut close = false;
        let mut submit = false;
        let mut retry_groups = false;
        let mut copy_created: Option<String> = None;

        if let Some(created) = state.created.clone() {
            let validity = match state.expiry_mode {
                ExpiryMode::Days => match parse_expires_days(&state.expires_in_days_input) {
                    Ok(Some(days)) => format!("有效期：{days} 天"),
                    _ => "有效期：永久".to_owned(),
                },
                ExpiryMode::DateTime => format!("结束时间：{}", selected_expiry_label(&state)),
            };
            let modal = egui::Modal::new(egui::Id::unique("create-key-success"))
                .backdrop_color(ui_style::modal_backdrop())
                .frame(ui_style::modal_frame().stroke(egui::Stroke::new(1.0, theme().accent)))
                .show(ctx, |ui| {
                    ui.set_width(ui_style::size::MODAL_MAX_WIDTH);

                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("✓")
                                .size(font::TITLE)
                                .strong()
                                .color(theme().accent),
                        );
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("API Key 已创建")
                                    .size(font::HEADING)
                                    .strong()
                                    .color(theme().text_primary),
                            );
                            ui.label(
                                egui::RichText::new("完整 Key 只展示这一次")
                                    .size(font::SMALL)
                                    .color(theme().text_dim),
                            );
                        });
                    });
                    ui.add_space(ui_style::space::LG);

                    ui_style::section_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui_style::field_label(ui, "名称");
                        ui.label(
                            egui::RichText::new(&created.info.name)
                                .size(font::BODY)
                                .strong()
                                .color(theme().text_primary),
                        );
                        ui.add_space(ui_style::space::SM);
                        ui_style::field_label(ui, "有效期");
                        ui.label(
                            egui::RichText::new(&validity)
                                .size(font::BODY_SMALL)
                                .color(theme().text_secondary),
                        );
                    });

                    if let Some(warning) = &created.warning {
                        ui.add_space(ui_style::space::MD);
                        ui_style::error_banner(ui, warning);
                    }

                    ui.add_space(ui_style::space::MD);
                    ui_style::field_label(ui, "完整 Key");
                    ui_style::section_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(&created.full_key)
                                .monospace()
                                .size(font::BODY_SMALL)
                                .color(theme().text_primary),
                        );
                    });
                    ui.add_space(ui_style::space::SM);
                    ui_style::hint_label(ui, "已自动复制到剪贴板，关闭后无法再次查看。");

                    ui.add_space(ui_style::space::XL);
                    ui.horizontal(|ui| {
                        if ui_style::add_button(
                            ui,
                            "复制 Key",
                            ButtonTone::Neutral,
                            ButtonSize::FormMd,
                        )
                        .clicked()
                        {
                            copy_created = Some(created.full_key.clone());
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui_style::add_button(
                                ui,
                                "完成",
                                ButtonTone::Primary,
                                ButtonSize::Form,
                            )
                            .clicked()
                            {
                                close = true;
                            }
                        });
                    });
                });
            if modal.backdrop_response.clicked() {
                close = true;
            }
        } else {
            let groups = state.groups.clone();
            let selected_group_label = state
                .selected_group_id
                .and_then(|id| groups.iter().find(|group| group.id == id))
                .map(|group| group.name.clone())
                .unwrap_or_else(|| "请选择分组".to_owned());
            let can_submit = !state.loading_groups && !state.submitting && !groups.is_empty();

            let modal = egui::Modal::new(egui::Id::unique("create-key-form"))
                .backdrop_color(ui_style::modal_backdrop())
                .frame(ui_style::modal_frame())
                .show(ctx, |ui| {
                    ui.set_width(ui_style::size::MODAL_MAX_WIDTH);

                    // 标题区：左侧品牌色竖条 + 标题/说明，和设置页、登录弹窗保持同一层级关系。
                    ui.horizontal(|ui| {
                        let (bar_rect, _) = ui.allocate_exact_size(
                            egui::vec2(
                                ui_style::size::ACCENT_BAR_WIDTH,
                                ui_style::size::ACCENT_BAR_HEIGHT,
                            ),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(
                            bar_rect,
                            egui::CornerRadius::same(ui_style::size::RADIUS_HAIRLINE),
                            theme().accent,
                        );
                        ui.vertical(|ui| {
                            ui.label(
                                egui::RichText::new("新增 API Key")
                                    .size(font::HEADING)
                                    .strong()
                                    .color(theme().text_primary),
                            );
                            ui.label(
                                egui::RichText::new("为当前账号创建独立凭据")
                                    .size(font::SMALL)
                                    .color(theme().text_dim),
                            );
                        });
                    });
                    ui.add_space(ui_style::space::LG);

                    ui_style::section_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());

                        ui_style::field_label(ui, "名称");
                        ui_style::singleline_input(ui, &mut state.name, "给这个 Key 起个名字");

                        ui.add_space(ui_style::space::MD);
                        ui_style::field_label(ui, "分组");
                        if state.loading_groups {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui_style::hint_label(ui, "正在加载可用分组…");
                            });
                        } else if groups.is_empty() {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new("没有可绑定分组")
                                        .size(font::BODY_SMALL)
                                        .color(theme().danger),
                                );
                                if ui_style::add_button(
                                    ui,
                                    "重试",
                                    ButtonTone::Neutral,
                                    ButtonSize::Auto,
                                )
                                .clicked()
                                {
                                    retry_groups = true;
                                }
                            });
                        } else {
                            egui::ComboBox::from_id_salt("create-key-group")
                                .selected_text(
                                    egui::RichText::new(selected_group_label)
                                        .size(font::SUBHEADING)
                                        .color(theme().text_primary),
                                )
                                .width(ui.available_width())
                                .show_ui(ui, |ui| {
                                    for group in &groups {
                                        ui.selectable_value(
                                            &mut state.selected_group_id,
                                            Some(group.id),
                                            egui::RichText::new(&group.name)
                                                .size(font::SUBHEADING)
                                                .color(theme().text_primary),
                                        );
                                    }
                                });
                        }

                        ui.add_space(ui_style::space::MD);
                        ui_style::field_label(ui, "额度（美元）");
                        ui_style::currency_input(ui, &mut state.quota_input, "$", "0 表示不限额");
                        ui_style::hint_label(ui, "0 表示不限额，修改额度不会自动清零已用额度。");

                        ui.add_space(ui_style::space::MD);
                        ui_style::field_label(ui, "有效期");
                        ui.horizontal(|ui| {
                            let days_selected = state.expiry_mode == ExpiryMode::Days;
                            let days_text = egui::RichText::new("按天数")
                                .size(font::BODY_SMALL)
                                .color(if days_selected {
                                    theme().on_accent
                                } else {
                                    theme().text_secondary
                                });
                            if ui
                                .add(ui_style::rich_button(
                                    days_text,
                                    if days_selected {
                                        ButtonTone::Primary
                                    } else {
                                        ButtonTone::Ghost
                                    },
                                    ButtonSize::Auto,
                                ))
                                .clicked()
                            {
                                state.expiry_mode = ExpiryMode::Days;
                            }

                            let datetime_selected = state.expiry_mode == ExpiryMode::DateTime;
                            let datetime_text = egui::RichText::new("指定时间")
                                .size(font::BODY_SMALL)
                                .color(if datetime_selected {
                                    theme().on_accent
                                } else {
                                    theme().text_secondary
                                });
                            if ui
                                .add(ui_style::rich_button(
                                    datetime_text,
                                    if datetime_selected {
                                        ButtonTone::Primary
                                    } else {
                                        ButtonTone::Ghost
                                    },
                                    ButtonSize::Auto,
                                ))
                                .clicked()
                            {
                                state.expiry_mode = ExpiryMode::DateTime;
                            }
                        });

                        match state.expiry_mode {
                            ExpiryMode::Days => {
                                ui_style::singleline_input(
                                    ui,
                                    &mut state.expires_in_days_input,
                                    "例如 30；0 表示永久",
                                );
                                ui_style::hint_label(
                                    ui,
                                    "从创建时间起计算，0 或留空表示永久有效。",
                                );
                            }
                            ExpiryMode::DateTime => {
                                ui.horizontal(|ui| {
                                    ui.set_height(ui_style::size::INPUT_HEIGHT);
                                    ui_style::date_picker(
                                        ui,
                                        &mut state.expires_date,
                                        "create-key-expiry-date",
                                    );
                                    ui_style::number_input(
                                        ui,
                                        &mut state.expires_hour,
                                        0..=23,
                                        " 时",
                                    );
                                    ui_style::number_input(
                                        ui,
                                        &mut state.expires_minute,
                                        0..=59,
                                        " 分",
                                    );
                                });
                                ui_style::hint_label(
                                    ui,
                                    "按本地时区选择结束日期与时间，必须晚于当前时间。",
                                );
                            }
                        }
                    });

                    if let Some(error) = &state.error {
                        ui.add_space(ui_style::space::MD);
                        ui_style::error_banner(ui, error);
                    }

                    ui.add_space(ui_style::space::XXL);
                    ui.horizontal(|ui| {
                        if ui_style::add_enabled_button(
                            ui,
                            !state.submitting,
                            "取消",
                            ButtonTone::Neutral,
                            ButtonSize::Form,
                        )
                        .clicked()
                        {
                            close = true;
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = if state.submitting {
                                "正在创建…"
                            } else {
                                "创建 Key"
                            };
                            if ui_style::add_enabled_button(
                                ui,
                                can_submit,
                                label,
                                ButtonTone::Primary,
                                ButtonSize::Form,
                            )
                            .clicked()
                            {
                                submit = true;
                            }
                            if state.submitting {
                                ui.spinner();
                            }
                        });
                    });
                });
            if modal.backdrop_response.clicked() && !state.submitting {
                close = true;
            }
        }

        if let Some(value) = copy_created {
            ctx.copy_text(value);
            self.toast = Some(("已复制完整 Key".to_owned(), Instant::now()));
            ctx.request_repaint_after(TOAST_DURATION);
        }

        if submit {
            self.create_key = Some(state);
            self.start_create_key(ctx);
        } else if retry_groups {
            self.create_key = Some(state);
            self.start_load_create_groups(ctx);
        } else if close {
            log::debug!("关闭创建 API Key 弹窗");
        } else {
            self.create_key = Some(state);
        }
    }

    /// 绘制删除 API Key 的二次确认弹窗。
    ///
    /// 删除是不可恢复操作，弹窗必须明确展示 Key 名称与打码值，避免用户误删
    /// 同一账号下的其他 Key。请求期间禁用按钮并保留弹窗，失败时显示原因。
    fn draw_delete_key_confirm(&mut self, ctx: &egui::Context) {
        let Some(state) = self.delete_key.take() else {
            return;
        };

        let mut close = false;
        let mut submit = false;
        let modal = egui::Modal::new(egui::Id::unique("delete-key-confirm"))
            .backdrop_color(ui_style::modal_backdrop())
            .frame(
                egui::Frame::new()
                    .fill(theme().panel_bg)
                    .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_XL))
                    .stroke(egui::Stroke::new(1.0, theme().panel_border))
                    .inner_margin(egui::Margin::same(ui_style::size::MARGIN_LG)),
            )
            .show(ctx, |ui| {
                ui.set_width(ui_style::size::MODAL_MAX_WIDTH);
                ui.label(
                    egui::RichText::new("删除 API Key")
                        .size(font::HEADING)
                        .strong()
                        .color(theme().text_primary),
                );
                ui.add_space(ui_style::space::SM);
                ui.label(
                    egui::RichText::new(format!("{}  {}", state.key_name, state.key_masked))
                        .size(font::BODY_SMALL)
                        .color(theme().text_secondary),
                );
                ui.add_space(ui_style::space::LG);
                ui.label(
                    egui::RichText::new("删除后无法恢复，正在使用这个 Key 的客户端会立即失效。")
                        .size(font::BODY_SMALL)
                        .color(theme().danger_text_soft),
                );

                if let Some(error) = &state.error {
                    ui.add_space(ui_style::space::MD);
                    ui.label(
                        egui::RichText::new(error)
                            .size(font::SMALL_PLUS)
                            .color(theme().danger),
                    );
                }

                ui.add_space(ui_style::space::XXL);
                ui.horizontal(|ui| {
                    if ui_style::add_enabled_button(
                        ui,
                        !state.submitting,
                        "取消",
                        ButtonTone::Neutral,
                        ButtonSize::Auto,
                    )
                    .clicked()
                    {
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = if state.submitting {
                            "正在删除…"
                        } else {
                            "确认删除"
                        };
                        if ui_style::add_enabled_button(
                            ui,
                            !state.submitting,
                            label,
                            ButtonTone::Danger,
                            ButtonSize::Auto,
                        )
                        .clicked()
                        {
                            submit = true;
                        }
                        if state.submitting {
                            ui.spinner();
                        }
                    });
                });
            });

        if modal.backdrop_response.clicked() && !state.submitting {
            close = true;
        }

        if submit {
            self.delete_key = Some(state);
            self.start_delete_key(ctx);
        } else if close {
            log::debug!("用户取消删除 API Key");
        } else {
            self.delete_key = Some(state);
        }
    }

    /// 绘制"退出账号并删除记录"的确认框。
    ///
    /// 删除是破坏性操作，必须二次确认（已与需求方确认）。确认框画在面板里而不是
    /// 系统对话框：一是风格统一，二是面板已经显示时不需要额外抢焦点。
    fn draw_delete_confirm(&mut self, ctx: &egui::Context) {
        let Some(id) = self.pending_delete.clone() else {
            return;
        };

        let display = match &self.data {
            DataLayer::Ready(store) => store
                .accounts()
                .iter()
                .find(|account| account.id == id)
                .map(|account| store.display_email(&account.email))
                .unwrap_or_else(|| "该账号".to_owned()),
            DataLayer::Unavailable(_) => "该账号".to_owned(),
        };

        let mut confirm = false;
        let mut cancel = false;

        let modal = egui::Modal::new(egui::Id::unique("delete-confirm"))
            .backdrop_color(ui_style::modal_backdrop())
            .frame(
                egui::Frame::new()
                    .fill(theme().panel_bg)
                    .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_XL))
                    .stroke(egui::Stroke::new(1.0, theme().panel_border))
                    .inner_margin(egui::Margin::same(ui_style::size::MARGIN_LG)),
            )
            .show(ctx, |ui| {
                ui.set_width(ui_style::size::SETTINGS_WIDTH);
                ui.label(
                    egui::RichText::new("退出账号并删除记录")
                        .size(font::HEADING)
                        .strong()
                        .color(theme().text_primary),
                );
                ui.add_space(ui_style::space::MD);
                ui.label(
                    egui::RichText::new(format!(
                        "确定退出 {display} 并删除记录？删除后需要重新登录才能恢复。"
                    ))
                    .size(font::BODY)
                    .color(theme().text_secondary),
                );
                ui.add_space(ui_style::space::XXL);
                ui.horizontal(|ui| {
                    if ui_style::add_button(ui, "取消", ButtonTone::Neutral, ButtonSize::Sm)
                        .clicked()
                    {
                        cancel = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui_style::add_button(ui, "删除", ButtonTone::Danger, ButtonSize::Md)
                            .clicked()
                        {
                            confirm = true;
                        }
                    });
                });
            });

        if modal.backdrop_response.clicked() {
            cancel = true;
        }

        if confirm {
            self.delete_account(&id);
            self.pending_delete = None;
        } else if cancel {
            self.pending_delete = None;
        }
    }
}

impl eframe::App for TrayApp {
    /// 清屏色设为全透明。
    ///
    /// 面板自己绘制带圆角的背景，窗口四角的像素必须保持透明，
    /// 否则会露出一块方形的底色，圆角就白做了。
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    /// 每帧的“逻辑阶段”：处理托盘指令与焦点变化，不做任何绘制。
    ///
    /// eframe 在面板隐藏时仍会调用这个函数（此时通过 `request_repaint` 唤醒），
    /// 因此“隐藏状态下点击托盘显示面板”的链路是通的；
    /// 而 [`eframe::App::ui`] 只在真正需要绘制时才被调用。
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.first_frame {
            self.first_frame = false;
            if self.debug_show_on_start || self.debug_open_login {
                // 调试模式：启动就弹出面板，方便截图与手工验收
                self.show_panel(ctx);
            } else {
                // 启动后立刻收起：托盘程序不该在启动时弹出一个空面板
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }

            if self.debug_open_login {
                // 打开弹窗必须在面板显示之后：弹窗画在面板窗口里
                self.login.open();
            }
        }

        self.handle_tray_commands(ctx);
        self.handle_escape(ctx);
        self.poll_login(ctx);
        self.poll_refresh(ctx);
        self.poll_account_keys(ctx);

        // 展开中的账号要保证有新鲜的用量曲线（缺失或超过 TTL 才拉）
        self.maybe_request_usage(ctx);

        // 启动后恢复上次的展开状态（记录在 accounts.json 的 expanded 字段里）
        if !self.restored_expansion {
            self.restored_expansion = true;
            let stored: Vec<String> = match &self.data {
                DataLayer::Ready(store) => store
                    .accounts()
                    .iter()
                    .filter(|item| item.expanded)
                    .map(|item| item.id.clone())
                    .collect(),
                DataLayer::Unavailable(_) => Vec::new(),
            };
            if !stored.is_empty() {
                log::debug!("恢复 {} 个账号的展开状态", stored.len());
                for id in stored {
                    self.expanded_accounts.insert(id);
                }
                self.request_next_keys(ctx);
            }
        }

        // 调试开关：只做一次，方便自动化截图看到展开态
        if self.debug_expand {
            self.debug_expand = false;
            let first_id = match &self.data {
                DataLayer::Ready(store) => store.accounts().first().map(|item| item.id.clone()),
                DataLayer::Unavailable(_) => None,
            };
            if let Some(id) = first_id {
                self.expanded_accounts.insert(id.clone());
                self.request_account_keys(ctx, &id);
                // 调试路径也写盘，这样"重启后是否保持展开"也能被自动化验收
                if let DataLayer::Ready(store) = &mut self.data
                    && let Err(err) = store.accounts_mut().set_expanded(&id, true)
                {
                    log::warn!("保存展开状态失败：{err:#}");
                }
            }
        }
        self.poll_usage(ctx);
        self.poll_copy(ctx);
        self.poll_quota_update(ctx);
        self.poll_delete_key(ctx);
        self.poll_create_key(ctx);
        self.poll_autostart(ctx);
        self.poll_settings_save(ctx);
        self.maybe_refresh_floating_ball_data(ctx);
        self.render_floating_ball(ctx);
        self.poll_update(ctx);
        self.maybe_start_refresh(ctx);
        self.handle_focus(ctx);
        self.update_toast(ctx);

        // 心跳：eframe 是"按需重绘"的，空闲时不会再调用 logic()，
        // 定时刷新与"刷新中"的轮询就永远不会发生。这里主动排下一次唤醒：
        //
        // * 有刷新在跑：500ms 后醒来收结果（界面保持"刷新中"的时间尽量短）；
        // * 空闲：5s 一次，足够让定时刷新到点触发，又不至于空转太频繁。
        //
        // 面板隐藏时 eframe 只跑逻辑、不绘制，所以心跳不会导致无谓的渲染。
        let tick = if self.refresh_rx.is_some() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(5)
        };
        ctx.request_repaint_after(tick);
    }

    /// 每帧的“绘制阶段”：只有面板应当可见时才画。
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if !self.panel_visible {
            return;
        }

        // 从根 Ui 里拿到 Context 的副本（内部引用计数，克隆很便宜），
        // 这样 draw_panel 既需要 ui 也需要 ctx 时不会产生借用冲突。
        let ctx = ui.ctx().clone();
        self.draw_panel(ui, &ctx);
    }
}

/// 托盘图标在**逻辑点**坐标系下的矩形。
///
/// 把 `tray_icon::Rect`（物理像素）换算后包成这个结构，是为了让定位逻辑可以
/// 脱离平台 API 单独测试。
#[derive(Debug, Clone, Copy, PartialEq)]
struct IconBounds {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl IconBounds {
    /// 图标中心的纵坐标，用来判断图标位于屏幕上半部分还是下半部分。
    fn center_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }
}

/// 纯函数：算出面板左上角的逻辑坐标。
///
/// 规则：
///
/// 1. 右边缘对齐图标右边缘（托盘图标基本都贴在屏幕右侧，这样面板不会越界）；
/// 2. 图标在屏幕上半部分（macOS 菜单栏）→ 面板放在图标下方；
///    图标在下半部分（Windows 任务栏）→ 面板放在图标上方，避免被屏幕底边切掉；
/// 3. 最后把坐标夹回屏幕可见范围，并给屏幕边缘留 [`SCREEN_MARGIN`] 的间隙；
/// 4. 拿不到图标矩形时，按平台习惯给出兜底位置（见 [`fallback_position`]）。
fn compute_panel_position(
    icon: Option<IconBounds>,
    monitor: egui::Vec2,
    panel: egui::Vec2,
) -> egui::Pos2 {
    let (x, y) = match icon {
        Some(icon) => {
            let place_above = icon.center_y() > monitor.y * 0.5;
            (
                icon.right - panel.x,
                if place_above {
                    icon.top - panel.y - TRAY_GAP
                } else {
                    icon.bottom + TRAY_GAP
                },
            )
        }
        None => fallback_position(monitor, panel),
    };

    // 夹回屏幕内。`max(SCREEN_MARGIN)` 处理“面板比屏幕还大”这种极端情况，
    // 避免出现 min > max 导致 clamp panic。
    let max_x = (monitor.x - panel.x - SCREEN_MARGIN).max(SCREEN_MARGIN);
    let max_y = (monitor.y - panel.y - SCREEN_MARGIN).max(SCREEN_MARGIN);

    egui::pos2(x.clamp(SCREEN_MARGIN, max_x), y.clamp(SCREEN_MARGIN, max_y))
}

/// 拿不到托盘图标位置时的兜底定位。
///
/// * macOS：菜单栏在屏幕顶部，面板贴在右上角往下一点；
/// * Windows：任务栏默认在屏幕底部，面板贴在右下角往上一点（额外让开任务栏高度）。
fn fallback_position(monitor: egui::Vec2, size: egui::Vec2) -> (f32, f32) {
    #[cfg(target_os = "macos")]
    {
        (monitor.x - size.x - SCREEN_MARGIN, 32.0)
    }

    #[cfg(not(target_os = "macos"))]
    {
        (
            monitor.x - size.x - SCREEN_MARGIN,
            monitor.y - size.y - SCREEN_MARGIN - 48.0,
        )
    }
}

/// 绘制标题区：产品名 + 版本号 + 一句话说明 + 顶部登录入口。
///
/// 返回 `true` 表示用户点击了「登陆新用户」按钮。
/// 之所以把按钮放在标题区（面板最顶部），是需求方明确的位置要求：
/// 登录是这块面板最主要的新增操作，必须第一眼可见。
/// 画一个图钉（置顶开关）。
///
/// 同样是矢量绘制：`📌` 这类 emoji 在中文字体里可能缺字形（变豆腐块），
/// 自己画"圆头 + 斜针 + 底座"在任何字体环境下都可靠。
fn draw_pin(ui: &mut egui::Ui, pinned: bool) -> egui::Response {
    let size = ui_style::size::ICON_MD;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = response.hovered();
    let color = if pinned {
        theme().accent
    } else if hovered {
        theme().text_primary
    } else {
        theme().text_secondary
    };
    let painter = ui.painter();
    let center = rect.center();

    // 钉头（实心圆）
    painter.circle_filled(
        egui::pos2(center.x - 1.6, center.y - 2.4),
        size * 0.17,
        color,
    );
    // 针身（斜线）
    painter.line_segment(
        [
            egui::pos2(center.x - 0.2, center.y - 0.8),
            egui::pos2(center.x + 4.6, center.y + 5.0),
        ],
        egui::Stroke::new(1.6, color),
    );
    // 底座横线，让"钉住"的语义更直白
    painter.line_segment(
        [
            egui::pos2(center.x - 4.6, center.y + 5.2),
            egui::pos2(center.x + 1.4, center.y + 5.2),
        ],
        egui::Stroke::new(1.6, color),
    );

    let hint = if pinned {
        "已钉住：点击面板外部不会收起；再次点击取消钉住"
    } else {
        "钉住面板：点击面板外部时保持打开"
    };
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(hint)
}

/// 画一个矢量齿轮（设置入口）。
///
/// 与 chevron 同理：egui 不能渲染 SVG，而齿轮用两三个圆 + 8 根短线就能画得很干净，
/// 比字体符号 `⚙` 在不同系统下大小/粗细不一要稳。
fn draw_gear(ui: &mut egui::Ui) -> egui::Response {
    let size = ui_style::size::ICON_MD;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = response.hovered();
    let color = if hovered {
        theme().accent
    } else {
        theme().text_secondary
    };
    let center = rect.center();
    let painter = ui.painter();

    // 齿：从外圈往外伸 8 段短线
    let inner = size * 0.29;
    let outer = size * 0.44;
    for index in 0..8 {
        let angle = index as f32 * std::f32::consts::TAU / 8.0;
        let (sin, cos) = angle.sin_cos();
        painter.line_segment(
            [
                egui::pos2(center.x + cos * inner, center.y + sin * inner),
                egui::pos2(center.x + cos * outer, center.y + sin * outer),
            ],
            egui::Stroke::new(1.5, color),
        );
    }
    // 齿轮外圈 + 中心孔
    painter.circle_stroke(center, inner, egui::Stroke::new(1.5, color));
    painter.circle_stroke(center, size * 0.11, egui::Stroke::new(1.3, color));

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 日间/夜间主题切换按钮。
///
/// 当前是夜间时显示太阳，表示点击切到日间；当前是日间时显示月亮。
fn draw_theme_toggle(ui: &mut egui::Ui, dark_mode: bool) -> egui::Response {
    let size = ui_style::size::ICON_MD;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = response.hovered();
    let color = if hovered {
        theme().accent
    } else {
        theme().text_secondary
    };
    let center = rect.center();
    let painter = ui.painter();

    if dark_mode {
        // 太阳：空心圆 + 8 条短射线。
        painter.circle_stroke(center, size * 0.25, egui::Stroke::new(1.5, color));
        for index in 0..8 {
            let angle = index as f32 * std::f32::consts::TAU / 8.0;
            let (sin, cos) = angle.sin_cos();
            painter.line_segment(
                [
                    egui::pos2(center.x + cos * size * 0.36, center.y + sin * size * 0.36),
                    egui::pos2(center.x + cos * size * 0.48, center.y + sin * size * 0.48),
                ],
                egui::Stroke::new(1.4, color),
            );
        }
    } else {
        // 月亮：实心圆再用面板底色挖掉一块，形成月牙。
        painter.circle_filled(center, size * 0.34, color);
        painter.circle_filled(
            center + egui::vec2(size * 0.18, -size * 0.12),
            size * 0.29,
            theme().panel_bg,
        );
    }

    let hint = if dark_mode {
        "切换到日间模式"
    } else {
        "切换到夜间模式"
    };
    response
        .on_hover_text(hint)
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 顶部条：只保留标题、版本号与齿轮。
fn draw_header(
    ui: &mut egui::Ui,
    open_settings: &mut bool,
    pinned: bool,
    toggle_pin: &mut bool,
    dark_mode: bool,
    toggle_theme: &mut bool,
) {
    // 手动分配整行，再分别给左标题和右工具组一个独立 Ui 矩形。
    // 这样 right_to_left 有明确的裁剪区，按钮不会因为父 horizontal 的
    // 剩余宽度/裁剪计算而被隐藏。
    let available = ui.available_rect_before_wrap();
    let row_rect = egui::Rect::from_min_size(
        available.min,
        egui::vec2(available.width(), ui_style::size::HEADER_ROW_HEIGHT),
    );
    let controls_width = ui_style::size::HEADER_CONTROLS_WIDTH.min(row_rect.width() * 0.5);
    let left_rect = egui::Rect::from_min_max(
        row_rect.min,
        egui::pos2(row_rect.right() - controls_width, row_rect.bottom()),
    );
    let right_rect = egui::Rect::from_min_max(
        egui::pos2(row_rect.right() - controls_width, row_rect.top()),
        row_rect.max,
    );

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(left_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            draw_window_drag_handle(ui);
            let title = ui
                .add(
                    egui::Label::new(
                        egui::RichText::new("用量统计托盘")
                            .size(font::SUBHEADING)
                            .strong()
                            .color(theme().text_primary),
                    )
                    .sense(egui::Sense::drag()),
                )
                .on_hover_cursor(egui::CursorIcon::Grab);
            if title.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
        },
    );

    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(right_rect)
            .layout(egui::Layout::right_to_left(egui::Align::Center)),
        |ui| {
            if draw_gear(ui).clicked() {
                *open_settings = true;
            }
            if draw_theme_toggle(ui, dark_mode).clicked() {
                *toggle_theme = true;
            }
            if draw_pin(ui, pinned).clicked() {
                *toggle_pin = true;
            }
            ui.label(
                egui::RichText::new(format!("v{}", crate::APP_VERSION))
                    .size(font::BODY_SMALL)
                    .color(theme().text_secondary),
            );
        },
    );
    ui.advance_cursor_after_rect(row_rect);

    // 标题下的品牌色细线：让面板在深色任务栏旁也能一眼认出是哪个产品。
    ui.add_space(ui_style::space::SM);
    let (line_rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 2.0), egui::Sense::hover());
    ui.painter().rect_filled(
        line_rect,
        egui::CornerRadius::same(ui_style::size::RADIUS_HAIRLINE),
        theme().accent,
    );
}

/// 标题栏左侧的拖动抓手。
///
/// 画六个小圆点而不是用字符：不同中文字体下 `⠿` / `⋮⋮` 的字形宽度差异很大。
fn draw_window_drag_handle(ui: &mut egui::Ui) {
    let size = egui::vec2(
        ui_style::size::DRAG_HANDLE_WIDTH,
        ui_style::size::DRAG_HANDLE_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::drag());
    let active = response.hovered() || response.dragged();
    if active {
        ui.ctx().set_cursor_icon(if response.dragged() {
            egui::CursorIcon::Grabbing
        } else {
            egui::CursorIcon::Grab
        });
    }
    if response.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let painter = ui.painter();
    let color = if active {
        theme().accent
    } else {
        theme().text_dim
    };
    let center = rect.center();
    for row in -1..=1 {
        for column in 0..2 {
            painter.circle_filled(
                egui::pos2(
                    center.x + (column as f32 - 0.5) * 4.0,
                    center.y + row as f32 * 4.0,
                ),
                1.2,
                color,
            );
        }
    }
}

/// 根据窗口内的指针位置判断缩放方向。
///
/// 四角优先于四边：角落区域较大（18 点），边区域只有 8 点。这样用户从
/// 右下角拖动时一定得到 SouthEast，而不是只改高度的 South。
fn resize_direction_at(
    pos: egui::Pos2,
    bounds: egui::Rect,
) -> Option<egui::viewport::ResizeDirection> {
    const CORNER: f32 = 18.0;
    const EDGE: f32 = 8.0;

    if !bounds.contains(pos) {
        return None;
    }

    let near_left = pos.x <= bounds.left() + CORNER;
    let near_right = pos.x >= bounds.right() - CORNER;
    let near_top = pos.y <= bounds.top() + CORNER;
    let near_bottom = pos.y >= bounds.bottom() - CORNER;
    let corner = match (near_left, near_right, near_top, near_bottom) {
        (true, _, true, _) => Some(egui::viewport::ResizeDirection::NorthWest),
        (_, true, true, _) => Some(egui::viewport::ResizeDirection::NorthEast),
        (true, _, _, true) => Some(egui::viewport::ResizeDirection::SouthWest),
        (_, true, _, true) => Some(egui::viewport::ResizeDirection::SouthEast),
        _ => None,
    };
    if corner.is_some() {
        return corner;
    }

    if pos.x <= bounds.left() + EDGE {
        Some(egui::viewport::ResizeDirection::West)
    } else if pos.x >= bounds.right() - EDGE {
        Some(egui::viewport::ResizeDirection::East)
    } else if pos.y <= bounds.top() + EDGE {
        Some(egui::viewport::ResizeDirection::North)
    } else if pos.y >= bounds.bottom() - EDGE {
        Some(egui::viewport::ResizeDirection::South)
    } else {
        None
    }
}

/// 缩放方向对应的鼠标指针。
fn resize_cursor(direction: egui::viewport::ResizeDirection) -> egui::CursorIcon {
    use egui::viewport::ResizeDirection;
    match direction {
        ResizeDirection::North => egui::CursorIcon::ResizeNorth,
        ResizeDirection::South => egui::CursorIcon::ResizeSouth,
        ResizeDirection::East => egui::CursorIcon::ResizeEast,
        ResizeDirection::West => egui::CursorIcon::ResizeWest,
        ResizeDirection::NorthEast => egui::CursorIcon::ResizeNorthEast,
        ResizeDirection::SouthEast => egui::CursorIcon::ResizeSouthEast,
        ResizeDirection::NorthWest => egui::CursorIcon::ResizeNorthWest,
        ResizeDirection::SouthWest => egui::CursorIcon::ResizeSouthWest,
    }
}

/// 用全局指针位置处理窗口四角/四边缩放。
///
/// 不依赖 `Ui::interact`：面板内部的 ScrollArea / 页脚会覆盖窗口边缘，
/// 交互区域在同一层里可能被它们抢走。这里在每帧末尾统一判断指针位置，
/// 左键按下时直接让 winit 进入系统缩放；指针靠近边缘时同步显示缩放光标。
fn handle_window_resize(ctx: &egui::Context, bounds: egui::Rect) {
    let Some(pos) = ctx.input(|input| input.pointer.hover_pos()) else {
        return;
    };
    let Some(direction) = resize_direction_at(pos, bounds) else {
        return;
    };
    ctx.set_cursor_icon(resize_cursor(direction));
    if ctx.input(|input| input.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
    }
}

/// 四角画三条短斜线，提示这些位置可以缩放。
fn draw_resize_ticks(ui: &mut egui::Ui) {
    let bounds = ui.max_rect();
    let left = bounds.left();
    let right = bounds.right();
    let top = bounds.top();
    let bottom = bounds.bottom();
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.2, theme().resize_handle);
    for offset in [4.0, 8.0, 12.0] {
        painter.line_segment(
            [
                egui::pos2(right - offset, bottom - 1.0),
                egui::pos2(right - 1.0, bottom - offset),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(left + offset, bottom - 1.0),
                egui::pos2(left + 1.0, bottom - offset),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(right - offset, top + 1.0),
                egui::pos2(right - 1.0, top + offset),
            ],
            stroke,
        );
        painter.line_segment(
            [
                egui::pos2(left + offset, top + 1.0),
                egui::pos2(left + 1.0, top + offset),
            ],
            stroke,
        );
    }
}

/// 徽标颜色：全部在线（品牌薄荷绿）。
const BADGE_ONLINE: [u8; 3] = [121, 244, 189];

/// 徽标颜色：有账号离线（琥珀）。
const BADGE_OFFLINE: [u8; 3] = [251, 191, 36];

/// 徽标颜色：有账号凭据损坏（红）。
const BADGE_INVALID: [u8; 3] = [248, 113, 113];

/// 根据账号状态数量决定托盘徽标颜色。
///
/// 优先级：凭据损坏（红）> 离线（黄）> 全部在线（绿）> 其它（无徽标）。
/// "凭据损坏"排最前是因为它需要人工处理，比"临时离线"更值得提醒。
///
/// 抽成纯函数便于测试：徽标一旦算错，用户会看到误导性的颜色。
fn aggregate_badge(total: usize, online: usize, offline: usize, invalid: usize) -> Option<[u8; 3]> {
    if total == 0 {
        return None;
    }
    if invalid > 0 {
        return Some(BADGE_INVALID);
    }
    if offline > 0 {
        return Some(BADGE_OFFLINE);
    }
    if online == total {
        return Some(BADGE_ONLINE);
    }
    // 还有账号处于"未知/刷新中"，此时不给颜色，避免误导
    None
}

/// 徽标颜色的中文说明（日志用）。
fn badge_label(rgb: [u8; 3]) -> &'static str {
    match rgb {
        BADGE_ONLINE => "绿色（全部在线）",
        BADGE_OFFLINE => "黄色（有账号离线）",
        BADGE_INVALID => "红色（有账号凭据损坏）",
        _ => "其它",
    }
}

/// 状态点颜色。
fn status_color(status: AccountStatus) -> egui::Color32 {
    match status {
        AccountStatus::Online => theme().accent,
        AccountStatus::Refreshing => theme().info,
        AccountStatus::CredentialsInvalid => theme().danger,
        AccountStatus::Offline => theme().offline,
        AccountStatus::Unknown => theme().text_dim,
    }
}

/// 绘制账号总览卡片：总数 + 在线/离线/凭据损坏的数量 + 手动刷新按钮。
///
/// `refresh_clicked` 用来把"点了立即刷新"回传给调用方：
/// 绘制函数拿不到 `&mut TrayApp`，只能通过局部标志传递。
fn draw_account_summary(
    ui: &mut egui::Ui,
    cards: &[AccountCard],
    login_clicked: &mut bool,
    refresh_clicked: &mut bool,
    at_limit: bool,
) {
    let online = cards
        .iter()
        .filter(|card| card.status == AccountStatus::Online)
        .count();
    let invalid = cards
        .iter()
        .filter(|card| card.status == AccountStatus::CredentialsInvalid)
        .count();
    let offline = cards.len().saturating_sub(online + invalid);

    egui::Frame::new()
        .fill(theme().card_bg)
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
        .inner_margin(egui::Margin::same(ui_style::size::MARGIN_MD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("已登录 {} 个账号", cards.len()))
                        .size(font::SUBHEADING)
                        .strong()
                        .color(theme().text_primary),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui_style::add_button(ui, "立即刷新", ButtonTone::Neutral, ButtonSize::Xs)
                        .on_hover_text("立刻拉取一次状态，不必等定时刷新")
                        .clicked()
                    {
                        *refresh_clicked = true;
                    }

                    // 登录入口：品牌色高亮，放进总览卡片，省掉独立一整行
                    let response = if at_limit {
                        ui.add_enabled_ui(false, |ui| {
                            ui_style::add_button(
                                ui,
                                "＋ 登录",
                                ButtonTone::Primary,
                                ButtonSize::Tiny,
                            )
                        })
                        .inner
                    } else {
                        ui_style::add_button(ui, "＋ 登录", ButtonTone::Primary, ButtonSize::Tiny)
                    };
                    if response
                        .on_hover_text(if at_limit {
                            format!("已达 {MAX_ACCOUNTS} 个账号上限，请先退出一个账号")
                        } else {
                            "填写实例地址与账号密码，登录后账号会出现在这里".to_owned()
                        })
                        .clicked()
                    {
                        *login_clicked = true;
                    }
                });
            });
            ui.add_space(ui_style::space::XS);
            ui.label(
                egui::RichText::new(format!(
                    "在线 {online} · 离线 {offline} · 凭据损坏 {invalid}"
                ))
                .size(font::BODY)
                .color(theme().text_secondary),
            );
        });
}

/// 画一个 Lucide 风格的折叠箭头（chevron）。
///
/// 为什么不加载 SVG：egui 没有 SVG 渲染能力，引入 `resvg`/`usvg` 会带来十几个依赖，
/// 而这个图标只是三个点的折线——直接用 `Shape::line` 画，任何 DPI 下都是矢量清晰的。
/// 几何参考 [lucide](https://lucide.dev) 的 `chevron-down`（MIT）：
/// 24×24 网格上的折线 `(6,9) → (12,15) → (18,9)`，线宽 2，圆头圆角。
/// 展开/收起只换折线朝向：收起时朝右，展开时朝下。
fn draw_chevron(ui: &mut egui::Ui, expanded: bool, hover_t: f32) -> egui::Response {
    let size = ui_style::size::ICON_MD;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let center = rect.center();

    let painter = ui.painter();
    // 圆角底片：平时几乎透明，悬浮时随动画淡入，让箭头本身也像一颗"按钮"
    painter.rect_filled(
        rect.shrink(1.0),
        egui::CornerRadius::same(ui_style::size::RADIUS_XS),
        lerp_color(theme().overlay_subtle, theme().hover_card_bg, hover_t),
    );

    // 折线的半宽：16px 的图标取 4.2，视觉上接近 Lucide 在 16px 下的比例
    let half = size * 0.26;
    let points = if expanded {
        // 朝下：∨
        vec![
            egui::pos2(center.x - half, center.y - half * 0.5),
            egui::pos2(center.x, center.y + half * 0.55),
            egui::pos2(center.x + half, center.y - half * 0.5),
        ]
    } else {
        // 朝右：›
        vec![
            egui::pos2(center.x - half * 0.5, center.y - half),
            egui::pos2(center.x + half * 0.55, center.y),
            egui::pos2(center.x - half * 0.5, center.y + half),
        ]
    };

    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new(
            1.7,
            lerp_color(theme().text_secondary, theme().text_hover, hover_t * 0.8),
        ),
    ));

    response
}

/// 绘制单个账号卡片。
/// 绘制单个账号卡片。
///
/// 点标题行可以展开/收起该账号的 API Key 明细；`toggle` 把被点击的账号 id 回传给调用方
/// （绘制函数拿不到 `&mut TrayApp`）。
fn draw_account_card(
    ui: &mut egui::Ui,
    card: &AccountCard,
    toggle: &mut Option<String>,
    hover: &mut HoverState,
    key_actions: &mut KeyUiActions,
    usage_actions: &mut UsageUiActions,
    delete_request: &mut Option<String>,
) {
    // 悬浮进度：上一帧矩形的命中 + 指数淡入，避免"闪一下"
    let hover_key = format!("acct:{}", card.id);
    let t = hover.t(ui.ctx(), &hover_key);
    let frame = egui::Frame::new()
        .fill(lerp_color(theme().card_bg, theme().hover_card_bg, t))
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
        .inner_margin(egui::Margin::same(ui_style::size::MARGIN_MD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // 标题行只有左侧账号信息可点击展开/收起；右侧按钮使用独立
            // 控件自己的点击响应，不能再叠加整行手势。
            let mut header_clicked = false;
            let is_deepseek = card.provider == AccountProvider::DeepSeekPlatform;
            let can_create_key =
                !is_deepseek && card.keys.as_ref().is_some_and(|state| !state.loading);

            // 手动画出整行矩形，左右各自拥有明确的最大矩形，避免 horizontal
            // 按内容收缩导致右侧按钮停在中间。
            let available = ui.available_rect_before_wrap();
            let row_rect =
                egui::Rect::from_min_size(available.min, egui::vec2(available.width(), 46.0));
            let action_width = 104.0;
            let gap = 8.0;
            let left_rect = egui::Rect::from_min_max(
                row_rect.min,
                egui::pos2(row_rect.right() - action_width - gap, row_rect.bottom()),
            );
            let right_rect = egui::Rect::from_min_max(
                egui::pos2(row_rect.right() - action_width, row_rect.top()),
                row_rect.max,
            );

            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(left_rect)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
                |ui| {
                    let chevron = draw_chevron(ui, card.expanded, t);
                    if chevron.clicked() {
                        header_clicked = true;
                    }
                    ui.label(
                        egui::RichText::new("●")
                            .size(font::BODY_SMALL)
                            .color(status_color(card.status)),
                    );
                    // 邮箱与实例地址上下排布，正好填满右侧两个操作按钮形成的
                    // 纵向空间，不再在标题行下方单独留一条网址占位。
                    ui.vertical(|ui| {
                        let email = ui
                            .add(
                                egui::Label::new(
                                    egui::RichText::new(&card.display)
                                        .size(font::SUBHEADING)
                                        .strong()
                                        .color(lerp_color(
                                            theme().text_primary,
                                            theme().text_hover,
                                            t * 0.45,
                                        )),
                                )
                                .truncate()
                                .sense(egui::Sense::click()),
                            )
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if email.clicked() {
                            header_clicked = true;
                        }
                        let link_hint = if card.provider == AccountProvider::DeepSeekPlatform {
                            "DeepSeek 官方网站\n使用 API Key 读取余额，不提供用量曲线。"
                        } else {
                            "Sub2API 中转站"
                        };
                        let link_color = if card.provider == AccountProvider::DeepSeekPlatform {
                            theme().text_secondary
                        } else {
                            theme().text_dim
                        };
                        let link_response = ui.add(
                            egui::Label::new(
                                egui::RichText::new(&card.base_url)
                                    .size(font::BODY_SMALL)
                                    .color(link_color),
                            )
                            .truncate(),
                        );
                        link_response.on_hover_ui(|ui| {
                            ui.set_max_width(ui_style::size::TOOLTIP_MAX_WIDTH);
                            ui.label(
                                egui::RichText::new(link_hint)
                                    .size(font::BODY_SMALL)
                                    .color(theme().text_primary),
                            );
                        });
                    });
                },
            );

            if card.expanded {
                ui.scope_builder(
                    egui::UiBuilder::new()
                        .max_rect(right_rect)
                        .layout(egui::Layout::top_down(egui::Align::Max)),
                    |ui| {
                        if !is_deepseek {
                            let add_clicked = ui
                                .add_enabled_ui(can_create_key, |ui| {
                                    ui_style::add_button(
                                        ui,
                                        "新增key",
                                        ButtonTone::Neutral,
                                        ButtonSize::Header,
                                    )
                                })
                                .inner
                                .on_hover_text("在当前账号下创建一个新的 API Key")
                                .clicked();
                            if add_clicked {
                                key_actions.create_key_request = Some(card.id.clone());
                            }
                        }
                        if ui_style::add_button(
                            ui,
                            "登出账户",
                            ButtonTone::Neutral,
                            ButtonSize::Header,
                        )
                        .on_hover_text("退出这个账号并删除本地记录")
                        .clicked()
                        {
                            *delete_request = Some(card.id.clone());
                        }
                    },
                );
            }
            ui.advance_cursor_after_rect(row_rect);

            // 只把左侧账号信息区作为展开/收起热区；右侧按钮区不能覆盖，
            // 否则父级整行手势会抢走「新增key」「登出账户」的点击。
            if header_clicked {
                *toggle = Some(card.id.clone());
            }

            // 收起态只保留"账户 + 网址"；余额、订阅、刷新时间等全部收进展开态，
            // 这样一屏能看更多账号（需求方要求）。
            if card.expanded {
                let mut status_line = card.status.label().to_owned();
                if let Some(balance) = card.balance {
                    let symbol = if is_deepseek { "¥" } else { "$" };
                    status_line.push_str(&format!(" · 余额 {symbol}{balance:.2}"));
                }
                ui.label(
                    egui::RichText::new(status_line)
                        .size(font::BODY)
                        .color(theme().text_secondary),
                );

                if let Some(summary) = &card.summary {
                    ui.label(
                        egui::RichText::new(summary)
                            .size(font::BODY_SMALL)
                            .color(theme().text_dim),
                    );
                }

                ui.label(
                    egui::RichText::new(&card.refreshed)
                        .size(font::SMALL)
                        .color(theme().text_dim),
                );

                // 失败原因只展示一行，完整信息在日志里
                if let Some(error) = &card.error {
                    let first_line = error.lines().next().unwrap_or_default();
                    ui.label(
                        egui::RichText::new(first_line)
                            .size(font::SMALL)
                            .color(theme().danger),
                    );
                }

                if card.status == AccountStatus::CredentialsInvalid {
                    ui.add_space(ui_style::space::XS);
                    if ui_style::add_button(ui, "重新登录", ButtonTone::Primary, ButtonSize::Lg)
                        .on_hover_text("保留账号和 Key 缓存，只更新本地登录凭据")
                        .clicked()
                    {
                        key_actions.relogin_request = Some(card.id.clone());
                    }
                }

                if !is_deepseek {
                    ui.add_space(ui_style::space::SM);
                    ui.separator();
                    // 用量曲线：走用户态接口，看到的是本账号自己的数据
                    draw_usage_section(ui, card, usage_actions);
                    ui.add_space(ui_style::space::SM);
                    ui.separator();
                    draw_account_keys(ui, card, hover, key_actions);
                }
            }
        });

    hover.store(&hover_key, frame.response.rect);
}

/// 把 token 数格式化成紧凑文本（`1.2k` / `3.4M`）。
fn fmt_tokens(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

/// 是否用加载占位替换整个用量区。
///
/// 已有缓存曲线时，即使后台正在刷新也要继续显示旧图；只有第一次加载
/// 或切换到尚无缓存的 Key/区间时才显示 spinner。
fn should_show_usage_spinner(loading: bool, has_cached_points: bool) -> bool {
    loading && !has_cached_points
}

/// 绘制"用量"区：折叠标题、Key 选择、区间切换、指标切换、曲线与汇总。
fn draw_usage_section(ui: &mut egui::Ui, card: &AccountCard, actions: &mut UsageUiActions) {
    let has_usage = card
        .usage_points
        .iter()
        .any(|point| point.requests != 0 || point.total_tokens != 0 || point.actual_cost != 0.0);
    let mut toggle_collapse = false;
    ui.horizontal(|ui| {
        let chevron = draw_chevron(ui, !card.usage_collapsed, 0.0);
        if chevron.clicked() {
            toggle_collapse = true;
        }
        let title = ui
            .add(
                egui::Label::new(
                    egui::RichText::new("我的用量")
                        .size(usage_font::BODY)
                        .strong()
                        .color(theme().text_primary),
                )
                .sense(egui::Sense::click()),
            )
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if title.clicked() {
            toggle_collapse = true;
        }
        if card.usage_collapsed {
            ui.label(
                egui::RichText::new("已收起")
                    .size(usage_font::CAPTION)
                    .color(theme().text_dim),
            );
        } else if card.usage_loading && has_usage {
            ui.add(egui::Spinner::new().size(usage_font::BODY_SMALL));
            ui.label(
                egui::RichText::new("更新中...")
                    .size(usage_font::CAPTION)
                    .color(theme().text_dim),
            );
        }
    });
    if toggle_collapse {
        actions.toggle_collapse = Some(card.id.clone());
    }
    if card.usage_collapsed {
        return;
    }

    let keys = card
        .keys
        .as_ref()
        .map(|state| state.keys.as_slice())
        .unwrap_or(&[]);
    let selected_key_label = card
        .usage_api_key_id
        .as_ref()
        .and_then(|id| keys.iter().find(|key| &key.id == id))
        .map(|key| key.name.clone())
        .unwrap_or_else(|| {
            if card.usage_api_key_id.is_some() {
                "已选 Key".to_owned()
            } else {
                "全部 Key".to_owned()
            }
        });

    ui.add_space(ui_style::space::XXS);
    ui.horizontal(|ui| {
        // 「全部 Key」是默认项；Key 列表加载完成后再出现具体选项。
        egui::ComboBox::from_id_salt(("usage-key", card.id.as_str()))
            .selected_text(
                egui::RichText::new(selected_key_label)
                    .size(usage_font::SMALL)
                    .color(theme().text_primary),
            )
            .width(ui_style::size::USAGE_SELECT_WIDTH)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(card.usage_api_key_id.is_none(), "全部 Key")
                    .clicked()
                {
                    actions.api_key = Some((card.id.clone(), None));
                }
                for key in keys {
                    let selected = card.usage_api_key_id.as_deref() == Some(key.id.as_str());
                    if ui.selectable_label(selected, &key.name).clicked() {
                        actions.api_key = Some((card.id.clone(), Some(key.id.clone())));
                    }
                }
            });

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 从右往左排，视觉顺序仍是 24 小时 -> 7 天 -> 30 天。
            for range in UsageRange::ALL.iter().rev() {
                let selected = *range == card.usage_range;
                let text = egui::RichText::new(range.label())
                    .size(usage_font::SMALL)
                    .color(if selected {
                        theme().on_accent
                    } else {
                        theme().text_secondary
                    });
                let tone = if selected {
                    ButtonTone::Primary
                } else {
                    ButtonTone::Ghost
                };
                let button = ui_style::rich_button(text, tone, ButtonSize::Auto);
                if ui.add(button).clicked() && !selected {
                    actions.range = Some((card.id.clone(), *range));
                }
            }
        });
    });

    ui.add_space(ui_style::space::HAIRLINE);
    ui.horizontal_wrapped(|ui| {
        for metric in UsageMetric::ALL {
            let selected = metric == card.usage_metric;
            let text = egui::RichText::new(metric.label())
                .size(usage_font::CAPTION)
                .color(if selected {
                    theme().on_accent
                } else {
                    theme().text_secondary
                });
            let tone = if selected {
                ButtonTone::Primary
            } else {
                ButtonTone::Ghost
            };
            let button = ui_style::rich_button(text, tone, ButtonSize::Auto);
            if ui.add(button).clicked() && !selected {
                actions.metric = Some((card.id.clone(), metric));
            }
        }
    });

    ui.add_space(ui_style::space::XS);
    // 只有首次加载或切换到没有缓存的区间时，才让 spinner 占据图表位置。
    // 周期性刷新继续显示旧曲线，避免图表短暂消失再出现。
    if should_show_usage_spinner(card.usage_loading, !card.usage_points.is_empty()) {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("加载用量...")
                    .size(usage_font::SMALL)
                    .color(theme().text_dim),
            );
        });
        return;
    }

    if let Some(error) = &card.usage_error {
        ui.label(
            egui::RichText::new(error)
                .size(usage_font::SMALL)
                .color(theme().danger),
        );
        if card.usage_points.is_empty() {
            return;
        }
        ui.label(
            egui::RichText::new("以下是上次成功加载的数据")
                .size(usage_font::CAPTION)
                .color(theme().text_dim),
        );
    }

    if card.usage_points.is_empty() || !has_usage {
        ui.label(
            egui::RichText::new("该区间暂无用量")
                .size(usage_font::SMALL)
                .color(theme().text_dim),
        );
        return;
    }

    draw_usage_chart(ui, &card.usage_points, card.usage_metric);
    draw_usage_totals(ui, card);
}

/// 用量汇总指标卡片的固定宽度（逻辑点）。
///
/// 固定宽度让拉伸托盘时每个指标保持一致节奏，不会出现某个卡片被拉得很长、
/// 另一个被压成一字一行的情况。
const USAGE_METRIC_CHIP_WIDTH: f32 = 96.0;

/// 用量汇总指标卡片之间的水平间距。
const USAGE_METRIC_CHIP_GAP: f32 = 6.0;

/// 绘制用量区底部的完整指标。
fn draw_usage_totals(ui: &mut egui::Ui, card: &AccountCard) {
    let totals = UsageTotals::from_points(&card.usage_points);
    let scope = if card.usage_api_key_id.is_some() {
        "当前 Key 合计"
    } else {
        "全部 Key 合计"
    };

    ui.add_space(ui_style::space::HAIRLINE);
    ui.label(
        egui::RichText::new(scope)
            .size(usage_font::CAPTION)
            .strong()
            .color(theme().text_secondary),
    );

    let metrics = [
        ("总 Token", fmt_tokens(totals.total_tokens)),
        ("输入", fmt_tokens(totals.input_tokens)),
        ("输出", fmt_tokens(totals.output_tokens)),
        ("命中率", format!("{:.1}%", totals.cache_hit_rate_percent())),
        ("Cache 写", fmt_tokens(totals.cache_creation_tokens)),
        ("Cache 读", fmt_tokens(totals.cache_read_tokens)),
        ("请求", totals.requests.to_string()),
        ("花费", format!("${:.3}", totals.actual_cost)),
    ];

    // 每个指标固定宽度，按当前可用宽度计算一行最多能放几个；
    // 只有一行放不下时才换行，避免宽窗口下仍被硬塞成两列长条。
    let per_row = usage_metrics_per_row(ui.available_width());
    for (row_index, row) in metrics.chunks(per_row).enumerate() {
        if row_index > 0 {
            ui.add_space(ui_style::space::HAIRLINE);
        }
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = USAGE_METRIC_CHIP_GAP;
            for (label, value) in row {
                usage_metric_chip(ui, label, value, USAGE_METRIC_CHIP_WIDTH);
            }
        });
    }
}

/// 计算一行能放多少个固定宽度指标。
///
/// 公式按 `n * width + (n - 1) * gap <= available` 推导；至少返回 1，
/// 保证极窄窗口也不会除零或出现零列布局。
fn usage_metrics_per_row(available_width: f32) -> usize {
    let available = available_width.max(1.0);
    let step = USAGE_METRIC_CHIP_WIDTH + USAGE_METRIC_CHIP_GAP;
    let count = ((available + USAGE_METRIC_CHIP_GAP) / step).floor() as usize;
    count.max(1)
}

/// 一个固定宽度的紧凑指标小标签。
fn usage_metric_chip(ui: &mut egui::Ui, label: &str, value: &str, width: f32) {
    let text = format!("{label} {value}");
    egui::Frame::new()
        .fill(theme().empty_bg)
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_SM))
        .inner_margin(egui::Margin::symmetric(5, 2))
        .show(ui, |ui| {
            // Frame 的左右内边距各 5，内容宽度要扣掉，保证总宽度固定。
            ui.set_min_width((width - 10.0).max(0.0));
            ui.add(
                egui::Label::new(
                    egui::RichText::new(&text)
                        .size(usage_font::CAPTION)
                        .color(theme().text_secondary),
                )
                .truncate(),
            )
            .on_hover_text(text);
        });
}

/// 构造折线与基线之间的面积网格。
///
/// egui 的 `convex_polygon` 只适合凸多边形；用量曲线会上下起伏，直接喂给
/// 它会产生跨越折线的错误三角面。这里按每个相邻时间点生成两个三角形：
///
/// ```text
/// top_i ---- top_i+1
///   |  \      |
///   |   \     |
/// bot_i ---- bot_i+1
/// ```
///
/// 所有三角形共用一个 Mesh，颜色透明度只会混合一次，不会在接缝处叠深。
fn usage_area_mesh(line: &[egui::Pos2], baseline_y: f32, color: egui::Color32) -> egui::Mesh {
    let mut mesh = egui::Mesh::default();
    if line.len() < 2 {
        return mesh;
    }

    mesh.reserve_vertices(line.len() * 2);
    mesh.reserve_triangles((line.len() - 1) * 2);
    for point in line {
        mesh.colored_vertex(*point, color);
        mesh.colored_vertex(egui::pos2(point.x, baseline_y), color);
    }
    for index in 0..line.len() - 1 {
        let top_left = (index * 2) as u32;
        let bottom_left = top_left + 1;
        let top_right = top_left + 2;
        let bottom_right = top_left + 3;
        mesh.add_triangle(top_left, bottom_left, top_right);
        mesh.add_triangle(top_right, bottom_left, bottom_right);
    }
    mesh
}

/// 提取当前指标下真正有数据的时间点，并保留它们在完整时间轴中的原始索引。
///
/// 补零后的时间桶只用于横轴范围与汇总；曲线不应该把这些零值桶画出来，
/// 否则会出现贴基线的长直线，悬浮时还会弹出全 0 的提示。
fn usage_chart_samples(
    points: &[crate::api::UsagePoint],
    metric: UsageMetric,
) -> Vec<(usize, f32)> {
    points
        .iter()
        .enumerate()
        .filter_map(|(index, point)| {
            let value = metric.value(point) as f32;
            (value > 0.0).then_some((index, value))
        })
        .collect()
}

/// 用原生绘制画 token 用量折线：网格 + 面积 + 折线 + 悬浮提示。
///
/// 点数最多 31 个，全部在 CPU 上算完直接交给 painter，
/// 不引入图表库，也不做逐帧分配（`line` 是每帧临时 Vec，但只有几十个点）。
fn draw_usage_chart(ui: &mut egui::Ui, points: &[crate::api::UsagePoint], metric: UsageMetric) {
    let width = ui.available_width().max(80.0);
    let axis_height = ui_style::size::USAGE_AXIS_HEIGHT;
    let plot_height = ui_style::size::USAGE_PLOT_HEIGHT;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(width, plot_height + axis_height),
        egui::Sense::hover(),
    );
    let plot = egui::Rect::from_min_max(
        rect.min,
        egui::pos2(rect.right(), rect.bottom() - axis_height),
    );
    let painter = ui.painter();

    for step in 0..=3 {
        let y = plot.top() + plot.height() * step as f32 / 3.0;
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(1.0, theme().grid_line),
        );
    }

    let max_value = metric.axis_max(points);
    painter.text(
        egui::pos2(plot.right(), plot.top() + 2.0),
        egui::Align2::RIGHT_TOP,
        metric.format(max_value as f64),
        egui::FontId::proportional(usage_font::MICRO),
        theme().text_dim,
    );
    let count = points.len();
    let x_at = |index: usize| -> f32 {
        if count <= 1 {
            plot.center().x
        } else {
            plot.left() + plot.width() * index as f32 / (count - 1) as f32
        }
    };
    let y_at =
        |value: f32| -> f32 { plot.bottom() - plot.height() * (value / max_value).clamp(0.0, 1.0) };

    let samples = usage_chart_samples(points, metric);
    if samples.is_empty() {
        painter.text(
            plot.center(),
            egui::Align2::CENTER_CENTER,
            "该指标暂无数据",
            egui::FontId::proportional(usage_font::CAPTION),
            theme().text_dim,
        );
        return;
    }

    let line: Vec<egui::Pos2> = samples
        .iter()
        .map(|(index, value)| egui::pos2(x_at(*index), y_at(*value)))
        .collect();

    painter.add(egui::Shape::mesh(usage_area_mesh(
        &line,
        plot.bottom(),
        theme().chart_fill,
    )));
    painter.add(egui::Shape::line(
        line.clone(),
        egui::Stroke::new(1.8, theme().accent),
    ));

    // 只为真实数据点画圆点；无数据时间桶已经不在 samples 中。
    for point in &line {
        painter.circle_filled(*point, 2.0, theme().accent);
    }

    let hovered_sample = response
        .hover_pos()
        .filter(|pos| plot.contains(*pos))
        .and_then(|pointer| {
            // 只在指针足够靠近真实数据点时显示悬浮提示；空白时段不弹出全 0 数据。
            let (point_index, value) =
                samples.iter().min_by(|(left_index, _), (right_index, _)| {
                    let left_distance = (x_at(*left_index) - pointer.x).abs();
                    let right_distance = (x_at(*right_index) - pointer.x).abs();
                    left_distance.total_cmp(&right_distance)
                })?;
            let x = x_at(*point_index);
            let hover_threshold = (plot.width() / count.max(1) as f32 * 1.5).max(10.0);
            ((x - pointer.x).abs() <= hover_threshold).then_some((*point_index, *value, x))
        });

    if let Some((point_index, value, x)) = hovered_sample {
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, theme().resize_handle),
        );
        painter.circle_filled(egui::pos2(x, y_at(value)), 3.2, theme().text_hover);

        let point = &points[point_index];
        let text = format!(
            "{}\n总 {} · 输入 {} · 输出 {}\nCache 写 {} · 读 {} · 命中率 {:.1}%\n{} 次请求 · 实际 ${:.3}",
            point.label,
            fmt_tokens(point.total_tokens),
            fmt_tokens(point.input_tokens),
            fmt_tokens(point.output_tokens),
            fmt_tokens(point.cache_creation_tokens),
            fmt_tokens(point.cache_read_tokens),
            point.cache_hit_rate_percent(),
            point.requests,
            point.actual_cost
        );
        let galley = painter.layout(
            text,
            egui::FontId::proportional(usage_font::CAPTION),
            theme().text_primary,
            plot.width() * 0.62,
        );
        let mut pos = egui::pos2(x + 10.0, plot.top() + 4.0);
        if pos.x + galley.size().x > plot.right() {
            pos.x = (x - galley.size().x - 10.0).max(plot.left());
        }
        let background = egui::Rect::from_min_size(
            pos - egui::vec2(5.0, 3.0),
            galley.size() + egui::vec2(10.0, 6.0),
        );
        painter.rect_filled(
            background,
            egui::CornerRadius::same(ui_style::size::RADIUS_SM),
            theme().tooltip_bg,
        );
        painter.galley(pos, galley, theme().text_primary);
    }

    // 横轴只标首尾两个真实时间桶，空间足够时再补中间点。
    let axis_font = egui::FontId::proportional(usage_font::TINY);
    let first = short_usage_label(&points[0].label);
    let last = short_usage_label(&points[count.saturating_sub(1)].label);
    painter.text(
        egui::pos2(plot.left(), rect.bottom()),
        egui::Align2::LEFT_BOTTOM,
        first,
        axis_font.clone(),
        theme().text_dim,
    );
    if count > 1 {
        painter.text(
            egui::pos2(plot.right(), rect.bottom()),
            egui::Align2::RIGHT_BOTTOM,
            last,
            axis_font.clone(),
            theme().text_dim,
        );
    }
    if count > 2 {
        let middle = short_usage_label(&points[count / 2].label);
        painter.text(
            egui::pos2(plot.center().x, rect.bottom()),
            egui::Align2::CENTER_BOTTOM,
            middle,
            axis_font,
            theme().text_dim,
        );
    }
}

/// 把服务端时间标签缩短为横轴可读形式。
///
/// `2026-10-18 02:00` -> `10-18 02:00`，`2026-10-18` -> `10-18`。
/// 只处理接口约定的 ASCII 标签；异常格式原样返回。
fn short_usage_label(label: &str) -> String {
    let mut parts = label.splitn(2, ' ');
    let date = parts.next().unwrap_or(label);
    let time = parts.next();
    let short_date = date.get(5..).unwrap_or(date);
    match time {
        Some(time) => format!("{short_date} {time}"),
        None => short_date.to_owned(),
    }
}

/// 绘制展开后的 API Key 明细。
///
/// 每个 Key 一行块：名称 + 打码值、已用/总额度、近 5 小时用量、订阅名。
/// 后端只提供 5 小时/1 天/7 天窗口，没有 1 小时口径，这里如实标注"近 5 小时"。
fn draw_account_keys(
    ui: &mut egui::Ui,
    card: &AccountCard,
    hover: &mut HoverState,
    key_actions: &mut KeyUiActions,
) {
    let Some(state) = &card.keys else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("正在拉取 Key…")
                    .size(font::BODY_SMALL)
                    .color(theme().text_secondary),
            );
        });
        return;
    };

    if state.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("正在拉取 Key…")
                    .size(font::BODY_SMALL)
                    .color(theme().text_secondary),
            );
        });
        return;
    }

    if let Some(error) = &state.error {
        ui.label(
            egui::RichText::new(error)
                .size(font::BODY_SMALL)
                .color(theme().danger),
        );
        // 离线回退：明确告诉用户这是缓存，数据可能已经变化
        if state.cached {
            ui.label(
                egui::RichText::new("缓存数据 · 可能已过期，联网后会刷新")
                    .size(font::SMALL)
                    .color(theme().text_dim),
            );
        }
    }
    if state.error.is_some() && !state.cached {
        return;
    }

    if state.keys.is_empty() {
        ui.label(
            egui::RichText::new("该账号没有 API Key")
                .size(font::BODY_SMALL)
                .color(theme().text_dim),
        );
        return;
    }

    for key in &state.keys {
        ui.add_space(ui_style::space::XXS);

        let hover_key = format!("key:{}:{}", card.id, key.id);
        let kt = hover.t(ui.ctx(), &hover_key);
        let used = key.quota_used.unwrap_or(0.0);
        let quota_text = match key.quota {
            Some(quota) if quota > 0.0 => {
                format!("已用 ${used:.2} / 总额度 ${quota:.2}")
            }
            _ => format!("已用 ${used:.2} / 总额度 不限"),
        };
        // 底色随淡入进度插值：0 时完全透明，1 时是比卡片略亮的深蓝，
        // 文字同时提亮，保证高亮后依然清晰（不再"白底盖字"）。
        let row_frame =
            egui::Frame::new()
                .fill(lerp_color(
                    egui::Color32::TRANSPARENT,
                    theme().hover_card_bg,
                    kt,
                ))
                .stroke(egui::Stroke::new(
                    1.0,
                    lerp_color(egui::Color32::TRANSPARENT, theme().accent, kt * 0.7),
                ))
                .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_MD))
                .inner_margin(egui::Margin::symmetric(7, 4))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    // 第一行：名称 + 打码值（左），订阅名（右）。
                    // 复制只允许通过下方显式「复制」按钮触发，避免误点标题覆盖剪贴板。
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(format!("{}  {}", key.name, key.masked))
                                .size(font::BODY)
                                .color(lerp_color(
                                    theme().text_primary,
                                    theme().text_hover,
                                    kt * 0.4,
                                )),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(group) = key.group.as_ref().or(card.subscription.as_ref()) {
                                ui.label(egui::RichText::new(group).size(font::SMALL).color(
                                    lerp_color(theme().text_dim, theme().text_secondary, kt),
                                ));
                            }
                        });
                    });

                    // 第二行：Key 自身额度。限速统计只在对应窗口启用时单独展示；
                    // 三个窗口都没配置时不显示任何限速描述。
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(&quota_text)
                                .size(font::BODY_SMALL)
                                .color(lerp_color(
                                    theme().text_secondary,
                                    theme().text_hover,
                                    kt * 0.3,
                                )),
                        );
                    });

                    let rate_windows = [
                        (
                            "5 小时",
                            key.rate_limit_5h.unwrap_or(0.0),
                            key.usage_5h.unwrap_or(0.0),
                        ),
                        (
                            "1 天",
                            key.rate_limit_1d.unwrap_or(0.0),
                            key.usage_1d.unwrap_or(0.0),
                        ),
                        (
                            "7 天",
                            key.rate_limit_7d.unwrap_or(0.0),
                            key.usage_7d.unwrap_or(0.0),
                        ),
                    ];
                    let rate_stat_texts: Vec<String> = rate_windows
                        .iter()
                        .filter(|(_, limit, _)| *limit > 0.0)
                        .map(|(label, limit, used)| format!("{label}限速 ${used:.2} / ${limit:.2}"))
                        .collect();
                    if !rate_stat_texts.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for text in &rate_stat_texts {
                                ui.label(egui::RichText::new(text).size(font::SMALL_PLUS).color(
                                    lerp_color(theme().text_dim, theme().text_secondary, kt * 0.3),
                                ));
                            }
                        });
                    }

                    // 第三行：操作按钮只在鼠标悬浮到当前 Key 时出现。
                    // 不保留空白行：未悬浮时直接不分配这段高度，让 Key 列表更紧凑。
                    if kt > 0.05 {
                        ui.horizontal(|ui| {
                            // 固定最小尺寸 + 更大字号，避免按钮在托盘里显得又小又难点；
                            // 三个按钮宽度一致，悬浮出现时整体更整齐。
                            if ui_style::add_button(
                                ui,
                                "复制",
                                ButtonTone::Neutral,
                                ButtonSize::KeyAction,
                            )
                            .on_hover_text("复制完整 Key 到剪贴板")
                            .clicked()
                            {
                                key_actions.copy_request = Some((card.id.clone(), key.id.clone()));
                            }
                            if ui_style::add_button(
                                ui,
                                "调额度",
                                ButtonTone::Neutral,
                                ButtonSize::KeyAction,
                            )
                            .on_hover_text("调整这个 Key 的额度")
                            .clicked()
                            {
                                key_actions.quota_request = Some(QuotaEditTarget {
                                    account_id: card.id.clone(),
                                    key_id: key.id.clone(),
                                    key_name: key.name.clone(),
                                    key_masked: key.masked.clone(),
                                    current_quota: key.quota.unwrap_or(0.0),
                                    quota_used: used,
                                });
                            }
                            if ui_style::add_button(
                                ui,
                                "删除",
                                ButtonTone::Danger,
                                ButtonSize::KeyAction,
                            )
                            .on_hover_text("删除这个 API Key（不可恢复）")
                            .clicked()
                            {
                                key_actions.delete_request = Some(DeleteKeyTarget {
                                    account_id: card.id.clone(),
                                    key_id: key.id.clone(),
                                    key_name: key.name.clone(),
                                    key_masked: key.masked.clone(),
                                });
                            }
                        });
                    }
                });

        // 刚复制成功：在方块上方盖一个"已复制"浮窗，约 1.6 秒后消失。
        if card.copied_key.as_deref() == Some(key.id.as_str()) {
            let rect = row_frame.response.rect;
            let chip = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 66.0, rect.top() - 12.0),
                egui::vec2(62.0, 20.0),
            );
            let painter = ui.painter();
            painter.rect_filled(
                chip,
                egui::CornerRadius::same(ui_style::size::RADIUS_LG),
                theme().accent,
            );
            painter.text(
                chip.center(),
                egui::Align2::CENTER_CENTER,
                "已复制",
                egui::FontId::proportional(font::BODY_SMALL),
                theme().on_accent,
            );
        }

        hover.store(&hover_key, row_frame.response.rect);
    }
}

/// 绘制实例登录状态卡片。
///
/// 目前是占位内容；实例配置与登录接入后，这里会显示实例地址与连接状态。
fn draw_status_card(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme().card_bg)
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
        .inner_margin(egui::Margin::same(ui_style::size::MARGIN_MD))
        .show(ui, |ui| {
            // 撑满宽度，否则卡片会按内容收缩，看起来像个小方块
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("●")
                        .size(font::BODY_LARGE)
                        .color(theme().text_secondary),
                );
                ui.label(
                    egui::RichText::new("尚未登录任何实例")
                        .size(font::HEADING)
                        .strong()
                        .color(theme().text_primary),
                );
            });

            ui.add_space(ui_style::space::SM);
            ui.label(
                egui::RichText::new("配置 Sub2API 实例地址并登录后，托盘会定时拉取账户状态。")
                    .size(font::BODY_LARGE)
                    .color(theme().text_secondary),
            );
        });
}

/// 绘制账户列表的占位区。
///
/// 面板高度是固定的，账户列表接入前这里必须有点内容，否则面板下半截会是一大片空白。
/// 等真实列表做好后，直接把这个函数替换成滚动列表即可。
fn draw_empty_state(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme().empty_bg)
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
        .inner_margin(egui::Margin::same(ui_style::size::MARGIN_MD))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(ui_style::space::SM);
                ui.label(
                    egui::RichText::new("账户状态列表")
                        .size(font::BODY_LARGE)
                        .color(theme().text_secondary),
                );
                ui.add_space(ui_style::space::XS);
                ui.label(
                    egui::RichText::new("登录后按账户展示在线状态、剩余额度与最近使用时间。")
                        .size(font::BODY)
                        .color(theme().text_dim),
                );
                ui.add_space(ui_style::space::SM);
            });
        });
}

/// 在面板底部绘制半透明操作反馈浮层。
///
/// 使用 `Area` 而不是布局里的 `Panel`：浮层不会占用滚动区高度，出现/消失时
/// 不会把账号卡片上下推挤；`Order::Foreground` 保证它盖在内容和滚动条之上。
fn draw_toast_overlay(ctx: &egui::Context, text: &str) {
    let background = ui_style::toast_background();

    egui::Area::new(egui::Id::unique("toast-overlay"))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
        .show(ctx, |ui| {
            ui.set_max_width(320.0);
            egui::Frame::new()
                .fill(background)
                .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
                .stroke(egui::Stroke::new(1.0, theme().accent))
                .inner_margin(egui::Margin::symmetric(12, 7))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(text)
                            .size(font::BODY_SMALL_PLUS)
                            .color(theme().text_primary),
                    );
                });
        });
}

/// 把登录成功的草稿落库：加密凭据 → 组装账号 → 去重并写盘。
///
/// 返回 `Err` 时携带的是**可以直接展示**的中文原因（弹窗会原样显示），
/// 这样"重复账号""磁盘满""DPAPI 失败"都能给用户一个可行动的提示。
///
/// 这里刻意先加密再 `add`：
/// `add` 内部会做唯一性校验并原子写盘，任何一步失败都不会留下半条记录。
fn save_relogin(
    store: &mut crate::data::DataStore,
    draft: &LoginDraft,
    account_id: &str,
) -> Result<(), String> {
    let sealed = crate::secrets::seal_credentials(store.secrets(), account_id, &draft.credentials)
        .map_err(|err| {
            log::warn!("重新登录后凭据加密失败：{err:#}");
            "凭据加密失败，无法更新账号".to_owned()
        })?;

    let updated = store
        .accounts_mut()
        .update_credentials(
            account_id,
            sealed,
            draft.expires_at.clone(),
            Some(accounts::now_rfc3339()),
        )
        .map_err(|err| format!("更新账号失败：{err:#}"))?;
    if !updated {
        return Err("账号已不存在，无法原地重新登录".to_owned());
    }
    Ok(())
}

fn save_login(store: &mut crate::data::DataStore, draft: &LoginDraft) -> Result<(), String> {
    // 需求方要求：同时最多 10 个账号。在落库前拦下，避免登录成功却保存失败。
    if store.accounts().len() >= MAX_ACCOUNTS {
        return Err(format!(
            "最多同时登录 {MAX_ACCOUNTS} 个账号，请先在托盘里删除一个再添加"
        ));
    }

    // 先用空 secrets 建出账号，拿到它的 id（macOS 钥匙串条目以 id 为 account）
    let mut account = Account::new_with_provider(
        draft.provider,
        draft.base_url.clone(),
        draft.email.clone(),
        draft.role,
        String::new(),
    );
    account.last_login_at = Some(accounts::now_rfc3339());
    account.token_expires_at = draft.expires_at.clone();

    account.secrets =
        crate::secrets::seal_credentials(store.secrets(), &account.id, &draft.credentials)
            .map_err(|err| {
                log::warn!("凭据加密失败：{err:#}");
                "凭据加密失败，无法保存账号".to_owned()
            })?;

    store
        .accounts_mut()
        .add(account)
        // `add` 的错误已经带上下文（"该账号已存在…" / "写入失败…"），直接展示
        .map_err(|err| format!("保存账号失败：{err:#}"))
}

/// 绘制"数据目录不可用"的红色提示条。
///
/// 出现这个提示时，程序**不会**创建/改写任何文件，也不会静默换一个目录：
/// 需求方明确要求"报错并提示用户换目录"，避免用户以为账号已经保存成功。
fn draw_storage_banner(ui: &mut egui::Ui, reason: &str) {
    egui::Frame::new()
        .fill(theme().danger_bg)
        .corner_radius(egui::CornerRadius::same(ui_style::size::RADIUS_LG))
        .stroke(egui::Stroke::new(1.0, theme().danger_border))
        .inner_margin(egui::Margin::same(ui_style::size::MARGIN_SM))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new("数据目录不可写，账号无法保存")
                    .size(font::BODY_LARGE)
                    .strong()
                    .color(theme().danger_text),
            );
            ui.add_space(ui_style::space::XS);
            // 只显示首行：完整错误链可能很长，界面放不下，完整内容在日志里
            ui.label(
                egui::RichText::new(reason.lines().next().unwrap_or_default())
                    .size(font::BODY_SMALL)
                    .color(theme().danger_text_soft),
            );
            ui.add_space(ui_style::space::XS);
            ui.label(
                egui::RichText::new("请把程序移动到可写目录（例如桌面或 D 盘）后重启。")
                    .size(font::BODY_SMALL)
                    .color(theme().danger_text_soft),
            );
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 常见笔记本分辨率（逻辑点），用于构造测试场景。
    const MONITOR: egui::Vec2 = egui::Vec2::new(1440.0, 900.0);

    fn panel() -> egui::Vec2 {
        egui::vec2(PANEL_WIDTH, PANEL_HEIGHT)
    }

    #[test]
    fn 额度输入只接受非负有限数字() {
        assert_eq!(parse_quota_amount("0").unwrap(), 0.0);
        assert_eq!(parse_quota_amount(" 12.5 ").unwrap(), 12.5);
        assert!(parse_quota_amount("").is_err());
        assert!(parse_quota_amount("abc").is_err());
        assert!(parse_quota_amount("-1").is_err());
        assert!(parse_quota_amount("NaN").is_err());
        assert!(parse_quota_amount("inf").is_err());
    }

    #[test]
    fn 有效期天数只接受正整数() {
        assert_eq!(parse_expires_days("").unwrap(), None);
        assert_eq!(parse_expires_days("0").unwrap(), None);
        assert_eq!(parse_expires_days("30").unwrap(), Some(30));
        assert!(parse_expires_days("-1").is_err());
        assert!(parse_expires_days("abc").is_err());
        assert!(parse_expires_days("1.5").is_err());
    }

    #[test]
    fn 日期选择器能格式化为本地结束时间() {
        let state = CreateKeyState {
            account_id: String::new(),
            name: String::new(),
            quota_input: String::new(),
            expiry_mode: ExpiryMode::DateTime,
            expires_in_days_input: String::new(),
            expires_date: jiff::civil::date(2026, 12, 31),
            expires_hour: 23,
            expires_minute: 59,
            groups: Vec::new(),
            selected_group_id: None,
            loading_groups: false,
            submitting: false,
            error: None,
            created: None,
        };
        assert_eq!(selected_expiry_label(&state), "2026-12-31 23:59");
        assert!(selected_expiry_zoned(&state).is_ok());
    }

    #[test]
    fn 额度输入初值裁掉多余小数零() {
        assert_eq!(format_quota_input(0.0), "0");
        assert_eq!(format_quota_input(-2.0), "0");
        assert_eq!(format_quota_input(10.0), "10");
        assert_eq!(format_quota_input(12.5), "12.5");
        assert_eq!(format_quota_input(12.3456), "12.3456");
    }

    #[test]
    fn 新额度低于已用时提示会立即耗尽() {
        assert!(quota_will_exceed(5.0, 5.0, false));
        assert!(quota_will_exceed(2.0, 8.0, false));
        assert!(!quota_will_exceed(10.0, 8.0, false));
        assert!(
            !quota_will_exceed(0.0, 8.0, false),
            "0 表示不限额，不应提示"
        );
        assert!(
            !quota_will_exceed(2.0, 8.0, true),
            "勾选清零后不应提示立即耗尽"
        );
    }

    #[test]
    fn 徽标聚合规则() {
        // 没有账号：不显示徽标
        assert_eq!(aggregate_badge(0, 0, 0, 0), None);
        // 全部在线：绿
        assert_eq!(aggregate_badge(2, 2, 0, 0), Some(BADGE_ONLINE));
        // 有离线：黄（即使也有在线的）
        assert_eq!(aggregate_badge(2, 1, 1, 0), Some(BADGE_OFFLINE));
        // 有凭据损坏：红（优先级最高，即使也有离线）
        assert_eq!(aggregate_badge(3, 1, 1, 1), Some(BADGE_INVALID));
        // 还有未知/刷新中：不给颜色，避免误导
        assert_eq!(aggregate_badge(2, 1, 0, 0), None);
    }

    #[test]
    fn 徽标颜色有中文说明() {
        assert!(badge_label(BADGE_ONLINE).contains("在线"));
        assert!(badge_label(BADGE_OFFLINE).contains("离线"));
        assert!(badge_label(BADGE_INVALID).contains("凭据损坏"));
    }

    #[test]
    fn 在线状态用品牌色离线用灰色() {
        assert_eq!(status_color(AccountStatus::Online), theme().accent);
        assert_ne!(
            status_color(AccountStatus::Online),
            status_color(AccountStatus::Offline)
        );
        assert_ne!(
            status_color(AccountStatus::CredentialsInvalid),
            status_color(AccountStatus::Online)
        );
    }

    /// Windows 任务栏场景：图标在屏幕右下角 → 面板应放在图标**上方**，右边缘对齐。
    #[test]
    fn 图标在右下角时面板向右上展开() {
        let icon = IconBounds {
            left: 1390.0,
            top: 862.0,
            right: 1414.0,
            bottom: 886.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, icon.right - PANEL_WIDTH);
        assert_eq!(pos.y, icon.top - PANEL_HEIGHT - TRAY_GAP);
    }

    /// macOS 菜单栏场景：图标在屏幕右上角 → 面板应放在图标**下方**。
    #[test]
    fn 图标在右上角时面板向下展开() {
        let icon = IconBounds {
            left: 1380.0,
            top: 0.0,
            right: 1404.0,
            bottom: 24.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, icon.right - PANEL_WIDTH);
        assert_eq!(pos.y, icon.bottom + TRAY_GAP);
    }

    /// 图标贴着屏幕左边时，面板不应跑到屏幕外，而应被夹到左边距处。
    #[test]
    fn 图标靠左时面板被夹回屏幕内() {
        let icon = IconBounds {
            left: 4.0,
            top: 400.0,
            right: 28.0,
            bottom: 424.0,
        };

        let pos = compute_panel_position(Some(icon), MONITOR, panel());

        assert_eq!(pos.x, SCREEN_MARGIN);
        // 纵向仍在图标下方，未触发夹取
        assert_eq!(pos.y, icon.bottom + TRAY_GAP);
    }

    /// 拿不到图标矩形时，面板必须仍然落在屏幕内（且不出现负坐标）。
    #[test]
    fn 缺少图标信息时使用兜底位置() {
        let pos = compute_panel_position(None, MONITOR, panel());

        assert!(pos.x >= SCREEN_MARGIN);
        assert!(pos.y >= SCREEN_MARGIN);
        assert!(pos.x + PANEL_WIDTH <= MONITOR.x - SCREEN_MARGIN);
        assert!(pos.y + PANEL_HEIGHT <= MONITOR.y - SCREEN_MARGIN);
    }

    /// 屏幕比面板还小的极端情况不应该 panic（clamp 的上下界会重叠）。
    #[test]
    fn 屏幕过小时不会崩溃() {
        let tiny = egui::vec2(200.0, 200.0);
        let pos = compute_panel_position(None, tiny, panel());

        assert!(pos.x.is_finite());
        assert!(pos.y.is_finite());
    }

    #[test]
    fn 用量区间映射到服务端参数() {
        assert_eq!(UsageRange::Day.granularity(), "hour");
        assert_eq!(UsageRange::Week.granularity(), "day");
        assert_eq!(UsageRange::Month.granularity(), "day");
        assert_eq!(UsageRange::Day.back_days(), 1);
        assert_eq!(UsageRange::Week.back_days(), 7);
        assert_eq!(UsageRange::Month.back_days(), 30);
    }

    #[test]
    fn 用量区间日期是合法的年月日() {
        for range in UsageRange::ALL {
            let (start, end) = range.dates();
            // 形如 2026-10-18：10 个字符，且 start <= end
            assert_eq!(start.len(), 10, "start={start}");
            assert_eq!(end.len(), 10, "end={end}");
            assert!(start <= end, "start={start} end={end}");
        }
    }

    /// 构造测试用量点。
    fn test_usage_point() -> crate::api::UsagePoint {
        crate::api::UsagePoint {
            label: "2026-10-18".to_owned(),
            requests: 3,
            input_tokens: 500,
            output_tokens: 100,
            cache_creation_tokens: 100,
            cache_read_tokens: 1400,
            total_tokens: 2100,
            cost: 0.04,
            actual_cost: 0.03,
        }
    }

    #[test]
    fn 用量指标切换取到正确字段() {
        let point = test_usage_point();
        assert_eq!(UsageMetric::Total.value(&point), 2100.0);
        assert_eq!(UsageMetric::Input.value(&point), 500.0);
        assert_eq!(UsageMetric::Output.value(&point), 100.0);
        assert!((UsageMetric::CacheHitRate.value(&point) - 70.0).abs() < f64::EPSILON);
    }

    #[test]
    fn 曲线跳过当前指标为零的时间点() {
        let mut empty = test_usage_point();
        empty.label = "2026-10-18 03:00".to_owned();
        empty.requests = 0;
        empty.input_tokens = 0;
        empty.output_tokens = 0;
        empty.cache_creation_tokens = 0;
        empty.cache_read_tokens = 0;
        empty.total_tokens = 0;
        empty.cost = 0.0;
        empty.actual_cost = 0.0;

        let points = vec![test_usage_point(), empty.clone(), test_usage_point()];
        let samples = usage_chart_samples(&points, UsageMetric::Total);
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].0, 0, "应保留原始时间轴索引");
        assert_eq!(samples[1].0, 2, "零值桶不应进入曲线");
        assert_eq!(samples[1].1, 2100.0);

        let all_empty = vec![empty];
        assert!(usage_chart_samples(&all_empty, UsageMetric::Total).is_empty());
    }

    #[test]
    fn 用量指标按定宽尽可能排满一行() {
        assert_eq!(usage_metrics_per_row(1.0), 1);
        assert_eq!(usage_metrics_per_row(96.0), 1);
        assert_eq!(usage_metrics_per_row(198.0), 2);
        assert_eq!(usage_metrics_per_row(300.0), 3);
        assert_eq!(usage_metrics_per_row(504.0), 5);
        assert_eq!(usage_metrics_per_row(1000.0), 9);
    }

    #[test]
    fn 用量汇总包含输入输出与命中率() {
        let point = test_usage_point();
        let totals = UsageTotals::from_points(&[point]);
        assert_eq!(totals.requests, 3);
        assert_eq!(totals.input_tokens, 500);
        assert_eq!(totals.output_tokens, 100);
        assert_eq!(totals.cache_creation_tokens, 100);
        assert_eq!(totals.cache_read_tokens, 1400);
        assert_eq!(totals.total_tokens, 2100);
        assert!((totals.cache_hit_rate_percent() - 70.0).abs() < f64::EPSILON);
        assert!((totals.actual_cost - 0.03).abs() < f64::EPSILON);
    }

    #[test]
    fn 四角缩放优先于四边() {
        let bounds = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(360.0, 460.0));
        use egui::viewport::ResizeDirection;
        assert_eq!(
            resize_direction_at(egui::pos2(2.0, 2.0), bounds),
            Some(ResizeDirection::NorthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(358.0, 2.0), bounds),
            Some(ResizeDirection::NorthEast)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(2.0, 458.0), bounds),
            Some(ResizeDirection::SouthWest)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(358.0, 458.0), bounds),
            Some(ResizeDirection::SouthEast)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(180.0, 2.0), bounds),
            Some(ResizeDirection::North)
        );
        assert_eq!(
            resize_direction_at(egui::pos2(358.0, 230.0), bounds),
            Some(ResizeDirection::East)
        );
        assert_eq!(resize_direction_at(egui::pos2(180.0, 230.0), bounds), None);
    }

    #[test]
    fn 日夜主题使用不同色板() {
        set_palette(true);
        let dark = theme();
        set_palette(false);
        let light = theme();
        set_palette(true);

        assert_ne!(dark.panel_bg, light.panel_bg);
        assert_ne!(dark.text_primary, light.text_primary);
        assert_ne!(dark.accent, light.accent);
        assert!(light.panel_bg.r() > 200, "日间背景应当是浅色");
    }

    #[test]
    fn 面积网格按相邻点生成三角形() {
        let line = vec![
            egui::pos2(0.0, 30.0),
            egui::pos2(10.0, 10.0),
            egui::pos2(20.0, 25.0),
        ];
        let mesh = usage_area_mesh(&line, 100.0, egui::Color32::RED);
        assert_eq!(mesh.vertices.len(), 6, "每个点应生成曲线顶点和基线顶点");
        assert_eq!(mesh.indices.len(), 12, "两个区间应各生成两个三角形");
        assert_eq!(mesh.vertices[0].pos, line[0]);
        assert_eq!(mesh.vertices[1].pos, egui::pos2(0.0, 100.0));
        assert_eq!(mesh.vertices[4].pos, line[2]);

        let empty = usage_area_mesh(&line[..1], 100.0, egui::Color32::RED);
        assert!(empty.vertices.is_empty());
        assert!(empty.indices.is_empty());
    }

    #[test]
    fn 刷新时保留已有用量曲线() {
        assert!(should_show_usage_spinner(true, false));
        assert!(!should_show_usage_spinner(true, true));
        assert!(!should_show_usage_spinner(false, false));
    }

    #[test]
    fn token数量格式化() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1660), "1.7k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }
}

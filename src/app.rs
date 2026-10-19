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

use crate::accounts::{self, Account};
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

/// 面板背景色（深色，接近 sub2api logo 的深蓝黑）。
pub const PANEL_BG: egui::Color32 = egui::Color32::from_rgb(15, 23, 42);

/// 面板边框色，用来在浅色桌面上勾勒出气泡的轮廓。
/// 卡片悬浮时的高亮底色：比 CARD_BG 亮一档，文字对比度反而更高。
const HOVER_CARD_BG: egui::Color32 = egui::Color32::from_rgb(62, 78, 104);

/// 线性插值两个颜色（用于悬浮淡入淡出）。
fn lerp_color(from: egui::Color32, to: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    egui::Color32::from_rgba_premultiplied(
        mix(from.r(), to.r()),
        mix(from.g(), to.g()),
        mix(from.b(), to.b()),
        mix(from.a(), to.a()),
    )
}

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

/// 用量拉取结果：`(账号 id, 区间, 点集或错误文案)`。
type UsageReply = (
    String,
    UsageRange,
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
}

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

pub const PANEL_BORDER: egui::Color32 = egui::Color32::from_rgb(51, 65, 85);

/// 卡片（状态区）背景色。
pub const CARD_BG: egui::Color32 = egui::Color32::from_rgb(30, 41, 59);

/// 空状态占位区的背景色，比卡片再暗一点，形成层次。
const EMPTY_BG: egui::Color32 = egui::Color32::from_rgb(22, 33, 51);

/// 主要文字颜色。
pub const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(226, 232, 240);

/// 次要文字颜色（说明文字、版本号、快捷键提示）。
pub const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(148, 163, 184);

/// 更弱的文字颜色，用于占位说明一类的“可读但不打扰”的内容。
pub const TEXT_DIM: egui::Color32 = egui::Color32::from_rgb(100, 116, 139);

/// 品牌强调色（取自 sub2api logo 的薄荷绿渐变起始色）。
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(121, 244, 189);

/// 操作成功提示在面板底部的停留时长。
const TOAST_DURATION: Duration = Duration::from_secs(4);

/// 启动后延迟多久做第一轮状态刷新。
///
/// 不立刻刷新有两个原因：托盘图标与窗口还没完全就绪；用户刚启动就看到一堆请求也没必要。
const FIRST_REFRESH_DELAY: Duration = Duration::from_secs(5);

/// 面板打开时，数据超过这个时长就补刷一次。
const STALE_AFTER: Duration = Duration::from_secs(60);

/// 全局字号增量：需求方反馈整体字体偏小，所有 .size(x) 统一加 1 码。
const FONT_BOOST: f32 = 1.0;

/// 把设计稿字号放大 [FONT_BOOST] 后返回，便于统一调大/回调。
pub(crate) fn fs(base: f32) -> f32 {
    base + FONT_BOOST
}

/// 同时允许登录的账号上限（需求方要求：同时最多 10 个）。
const MAX_ACCOUNTS: usize = 10;

/// 用量曲线的缓存有效期：到点后重新拉，避免频繁打 Heavy 限流接口。
const USAGE_TTL: Duration = Duration::from_secs(60);

/// 面板里一张账号卡片需要的展示数据。
///
/// 在绘制前一次性算好（见 `collect_cards`），这样绘制闭包只依赖局部变量，
/// 不会和 `&mut self` 打架；也避免在闭包里反复查运行态。
struct AccountCard {
    /// 账号 id（点击展开要用）。
    id: String,
    /// 展示用邮箱（已按开关打码）。
    display: String,
    /// 实例地址。
    base_url: String,
    /// 角色中文标签。
    role_label: &'static str,
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
    /// 该区间补齐后的用量点。
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

    /// 面板展开时进行中的拉取（同一时间只允许一个）。
    keys_rx: Option<(String, Receiver<ApiKeysReply>)>,

    /// 面板是否被钉住（置顶显示，点外部不自动收起）。
    pinned: bool,

    /// 每个账号选中的用量区间（默认 24 小时）。
    usage_range: HashMap<String, UsageRange>,

    /// 用量曲线缓存：`(账号 id, 区间)` → 曲线。
    usage: HashMap<(String, UsageRange), UsageSeries>,

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

    /// 正在复制的 Key id（用于按钮态/日志）。
    copying_key: Option<String>,

    /// 复制结果通道：`(key_id, 完整值或错误文案)`。
    copy_rx: Option<Receiver<CopyReply>>,

    /// 复制成功的浮窗：`(账号 id, Key id, 出现时刻)`。
    copy_notice: Option<(String, String, Instant)>,

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

        // 深色配色：托盘气泡贴在深色任务栏旁边时观感更统一。
        // 这里直接改“两套主题”的样式，而不是只改当前主题：否则用户把系统切到浅色后，
        // 按钮、分隔线等控件会变回浅色，和固定的深色面板底色打架。
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = PANEL_BG;
        visuals.window_fill = PANEL_BG;
        cc.egui_ctx
            .all_styles_mut(|style| style.visuals = visuals.clone());

        // 钉住状态是持久化的 UI 偏好，启动时从 app.json 读回
        let pinned = match &data {
            DataLayer::Ready(store) => store.config().panel_pinned,
            DataLayer::Unavailable(_) => false,
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
            keys_rx: None,
            pinned,
            usage_range: HashMap::new(),
            usage: HashMap::new(),
            usage_rx: None,
            hover: HoverState::default(),
            debug_expand: std::env::var_os("SUB2API_TRAY_DEBUG_EXPAND").is_some(),
            restored_expansion: false,
            settings: None,
            copying_key: None,
            copy_rx: None,
            copy_notice: None,
            update_checking: false,
            update_result: None,
            update_url: None,
            update_rx: None,
        };

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
        match crate::tray::build_menu() {
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
                format!("Sub2API 托盘 · {total} 个账号（{online} 在线）")
            }
            DataLayer::Unavailable(_) => "Sub2API 托盘 · 数据目录不可用".to_owned(),
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
                // 运行态里的角色更新（例如管理员被降级），没有就用持久化时的
                let role = runtime
                    .map(|item| item.role)
                    .filter(|role| *role != crate::accounts::AccountRole::Unknown)
                    .unwrap_or(account.role);

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

                AccountCard {
                    id: account.id.clone(),
                    display: store.display_email(&account.email),
                    base_url: account.base_url.clone(),
                    role_label: role.label(),
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
                    usage_range: self.usage_range_of(&account.id),
                    usage_points: self
                        .usage
                        .get(&(account.id.clone(), self.usage_range_of(&account.id)))
                        .map(|series| series.points.clone())
                        .unwrap_or_default(),
                    usage_loading: self
                        .usage
                        .get(&(account.id.clone(), self.usage_range_of(&account.id)))
                        .map(|series| series.loading)
                        .unwrap_or(false),
                    usage_error: self
                        .usage
                        .get(&(account.id.clone(), self.usage_range_of(&account.id)))
                        .and_then(|series| series.error.clone()),
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
    /// 与弹窗那条路径的区别：结果写进 `account_keys`，供面板卡片内联展示；
    /// 同一时间只允许一个拉取（`keys_rx` 只存一个）。
    fn request_account_keys(&mut self, ctx: &egui::Context, account_id: &str) {
        if self.keys_rx.is_some() {
            log::debug!("已有展开拉取在进行中，忽略本次请求");
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

        self.account_keys.insert(
            id.clone(),
            AccountKeysState {
                loading: true,
                cached: false,
                ..AccountKeysState::default()
            },
        );

        let (sender, receiver) = std::sync::mpsc::channel();
        self.keys_rx = Some((id.clone(), receiver));
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

    /// 切换某个账号的用量区间，并立刻确保有数据。
    fn set_usage_range(&mut self, ctx: &egui::Context, account_id: &str, range: UsageRange) {
        self.usage_range.insert(account_id.to_owned(), range);
        self.maybe_request_usage(ctx);
        ctx.request_repaint();
    }

    /// 后台拉取某账号某区间的用量曲线。
    fn request_usage(&mut self, ctx: &egui::Context, account_id: &str, range: UsageRange) {
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

        self.usage.entry((id.clone(), range)).or_default().loading = true;
        let (sender, receiver) = std::sync::mpsc::channel();
        self.usage_rx = Some((id.clone(), range, receiver));
        let repaint = ctx.clone();

        std::thread::spawn(move || {
            let result =
                match crate::secrets::open_credentials(secret_store.as_ref(), &id, &secrets_blob) {
                    Ok(credentials) => match crate::api::HttpClient::new(&base_url, timeout) {
                        Ok(client) => client
                            .usage_trend(&credentials.access_token, &start, &end, granularity)
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
        let Some((account_id, range, receiver)) = self.usage_rx.as_ref() else {
            return;
        };
        let Ok(result) = receiver.try_recv() else {
            return;
        };
        let key = (account_id.clone(), *range);
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

    /// 为展开中的账号补齐用量曲线（过期或缺失才拉；同一时间只拉一个）。
    fn maybe_request_usage(&mut self, ctx: &egui::Context) {
        if self.usage_rx.is_some() {
            return;
        }
        let ids: Vec<String> = match &self.data {
            DataLayer::Ready(store) => store
                .accounts()
                .iter()
                .filter(|item| self.expanded_accounts.contains(&item.id))
                .map(|item| item.id.clone())
                .collect(),
            DataLayer::Unavailable(_) => Vec::new(),
        };

        for id in ids {
            let range = self.usage_range_of(&id);
            let needs_fetch = match self.usage.get(&(id.clone(), range)) {
                Some(series) if series.loading => false,
                Some(series) => series
                    .fetched_at
                    .map(|at| at.elapsed() > USAGE_TTL)
                    .unwrap_or(true),
                None => true,
            };
            if needs_fetch {
                self.request_usage(ctx, &id, range);
                return;
            }
        }
    }

    /// 依次为"已展开但还没拉到 Key"的账号发起拉取（同一时间只跑一个）。
    ///
    /// 重启后可能同时有多个账号处于展开状态，而 `keys_rx` 只允许一个在途请求，
    /// 因此每次请求结束都会再调一次这里，把剩下的补上。
    fn request_next_keys(&mut self, ctx: &egui::Context) {
        if self.keys_rx.is_some() {
            return;
        }
        let next = match &self.data {
            DataLayer::Ready(store) => store
                .accounts()
                .iter()
                .filter(|item| self.expanded_accounts.contains(&item.id))
                .map(|item| item.id.clone())
                .find(|id| !self.account_keys.contains_key(id)),
            DataLayer::Unavailable(_) => None,
        };
        if let Some(id) = next {
            self.request_account_keys(ctx, &id);
        }
    }

    /// 取一次"面板展开"的 Key 拉取结果。
    fn poll_account_keys(&mut self, ctx: &egui::Context) {
        let Some((account_id, receiver)) = self.keys_rx.as_ref() else {
            return;
        };

        let reply = match receiver.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                let account_id = account_id.clone();
                self.keys_rx = None;
                self.account_keys.insert(
                    account_id,
                    AccountKeysState {
                        loading: false,
                        cached: false,
                        error: Some("拉取 Key 的线程意外退出".to_owned()),
                        keys: Vec::new(),
                    },
                );
                ctx.request_repaint();
                return;
            }
        };

        let account_id = account_id.clone();
        self.keys_rx = None;

        let ApiKeysReply::List(result) = reply;
        let state = match result {
            Ok(keys) => {
                self.cache_api_keys(&account_id, &keys);
                AccountKeysState {
                    loading: false,
                    cached: false,
                    error: None,
                    keys,
                }
            }
            Err(message) => {
                // 拉取失败时回退到 accounts.json 里的缓存，至少让用户看到上次结果
                let cached_keys = self.cached_api_keys(&account_id);
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
        self.account_keys.insert(account_id, state);
        // 可能还有别的展开账号没拉取，接着来
        self.request_next_keys(ctx);
        ctx.request_repaint();
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
    fn open_settings(&mut self) {
        let DataLayer::Ready(store) = &self.data else {
            self.toast = Some(("数据目录不可用，无法打开设置".to_owned(), Instant::now()));
            return;
        };
        let config = store.config();
        self.settings = Some(SettingsDraft {
            mask_accounts: config.mask_accounts,
            autostart: crate::autostart::is_enabled(),
            refresh_interval_seconds: config.refresh_interval_seconds,
            request_timeout_seconds: config.request_timeout_seconds,
        });
    }

    /// 把设置草稿写盘并立即生效（打码会影响菜单与卡片，需要重建）。
    fn save_settings(&mut self) -> Result<(), String> {
        let Some(draft) = self.settings.clone() else {
            return Ok(());
        };
        let DataLayer::Ready(store) = &mut self.data else {
            return Err("数据目录不可用".to_owned());
        };

        let mut config = store.config().clone();
        config.mask_accounts = draft.mask_accounts;
        config.refresh_interval_seconds = draft.refresh_interval_seconds.clamp(30, 86400);
        config.request_timeout_seconds = draft.request_timeout_seconds.clamp(1, 120);

        crate::config::save(store.dir(), &config)
            .map_err(|err| format!("保存 app.json 失败：{err:#}"))?;
        *store.config_mut() = config;

        // 开机自启：与当前系统状态不同才写，避免每次保存都动注册表
        if draft.autostart != crate::autostart::is_enabled() {
            let executable =
                std::env::current_exe().map_err(|err| format!("无法定位当前程序路径：{err}"))?;
            crate::autostart::set_enabled(draft.autostart, &executable)
                .map_err(|err| format!("设置开机自启失败：{err:#}"))?;
        }

        // 打码开关会影响菜单标题与卡片，立即重建
        self.rebuild_menu();
        self.toast = Some(("设置已保存".to_owned(), Instant::now()));
        Ok(())
    }

    /// 绘制设置页（打码开关、刷新间隔、请求超时）。
    fn draw_settings(&mut self, ctx: &egui::Context) {
        // 先拷贝出与更新检查相关的只读状态，避免与 settings 的可变借用冲突
        let update_checking = self.update_checking;
        let update_result = self.update_result.clone();
        let update_url = self.update_url.clone();

        let Some(draft) = self.settings.as_mut() else {
            return;
        };

        let mut close = false;
        let mut save = false;
        let mut check_update = false;
        let mut copy_update_url: Option<String> = None;

        let modal = egui::Modal::new(egui::Id::unique("settings-dialog"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(PANEL_BG)
                    .corner_radius(egui::CornerRadius::same(12))
                    .stroke(egui::Stroke::new(1.0, PANEL_BORDER))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(280.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("设置")
                            .size(fs(14.0))
                            .strong()
                            .color(TEXT_PRIMARY),
                    );
                });
                ui.add_space(10.0);

                ui.checkbox(
                    &mut draft.mask_accounts,
                    egui::RichText::new("账号名打码显示（只影响界面）").size(fs(12.0)),
                );

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("状态刷新间隔")
                            .size(fs(12.0))
                            .color(TEXT_SECONDARY),
                    );
                    ui.add(
                        egui::DragValue::new(&mut draft.refresh_interval_seconds)
                            .range(30..=86400)
                            .speed(10.0),
                    );
                    ui.label(egui::RichText::new("秒").size(fs(11.0)).color(TEXT_DIM));
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("请求超时")
                            .size(fs(12.0))
                            .color(TEXT_SECONDARY),
                    );
                    ui.add(egui::DragValue::new(&mut draft.request_timeout_seconds).range(1..=120));
                    ui.label(egui::RichText::new("秒").size(fs(11.0)).color(TEXT_DIM));
                });

                ui.add_space(6.0);
                ui.checkbox(
                    &mut draft.autostart,
                    egui::RichText::new("开机自动启动（登录时运行）").size(fs(12.0)),
                );

                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized(
                            [92.0, 24.0],
                            egui::Button::new(egui::RichText::new("检查更新").size(fs(11.0))),
                        )
                        .clicked()
                    {
                        check_update = true;
                    }
                    if update_checking {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new("检查中…")
                                .size(fs(10.0))
                                .color(TEXT_DIM),
                        );
                    }
                });
                if let Some(message) = update_result.as_deref() {
                    ui.label(
                        egui::RichText::new(message)
                            .size(fs(10.0))
                            .color(TEXT_SECONDARY),
                    );
                }
                if let Some(url) = update_url.as_deref()
                    && ui
                        .add_sized(
                            [120.0, 22.0],
                            egui::Button::new(egui::RichText::new("复制下载链接").size(fs(10.0))),
                        )
                        .clicked()
                {
                    copy_update_url = Some(url.to_owned());
                }

                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new("保存后立即生效并写入 app.json。")
                        .size(fs(10.0))
                        .color(TEXT_DIM),
                );
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    let save_button = egui::Button::new(
                        egui::RichText::new("保存")
                            .size(fs(12.0))
                            .strong()
                            .color(egui::Color32::from_rgb(15, 23, 42)),
                    )
                    .fill(ACCENT);
                    if ui.add_sized([80.0, 26.0], save_button).clicked() {
                        save = true;
                    }
                    if ui
                        .add_sized(
                            [80.0, 26.0],
                            egui::Button::new(egui::RichText::new("取消").size(fs(12.0))),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            });

        if modal.backdrop_response.clicked() {
            close = true;
        }

        if let Some(url) = copy_update_url {
            ctx.copy_text(url);
            self.toast = Some(("已复制下载链接".to_owned(), Instant::now()));
        }
        if check_update {
            self.start_update_check(ctx);
        }

        if save {
            match self.save_settings() {
                Ok(()) => close = true,
                Err(message) => {
                    self.toast = Some((message, Instant::now()));
                }
            }
        }
        if close {
            self.settings = None;
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

        // 账号没了，展开状态与 Key 缓存也要一起清掉
        self.expanded_accounts.remove(id);
        self.account_keys.remove(id);

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
            log::debug!("Esc：关闭设置页");
            self.settings = None;
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

        match &mut self.data {
            DataLayer::Unavailable(reason) => {
                self.login
                    .set_error(format!("数据目录不可用，无法保存账号：{reason}"));
            }
            DataLayer::Ready(store) => {
                if let Err(message) = save_login(store, &draft) {
                    self.login.set_error(message);
                    return;
                }

                // 日志只留打码后的账号，避免明文邮箱长期堆积在日志里
                let display = store.display_email(&draft.email);
                log::info!("账号已保存：{display}");
                self.toast = Some((format!("已登录 {display}"), Instant::now()));
                self.login.close();
                // 新账号要立刻出现在托盘菜单里，并尽快刷新一次状态
                self.rebuild_menu();
                self.update_badge();
                self.last_round = None;
                ctx.request_repaint();
            }
        }
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
        // 被点击要复制的 (账号 id, Key id)
        let mut copy_request: Option<(String, String)> = None;
        // 用量区间切换请求
        let mut usage_switch: Option<(String, UsageRange)> = None;
        // 面板内的账号操作：请求删除（标题行右侧按钮触发）
        let mut delete_request: Option<String> = None;
        // 设置页入口的点击结果
        let mut open_settings = false;
        // 图钉（置顶）点击结果
        let mut toggle_pin = false;
        // 悬浮动画状态：先取出来，绘制闭包里才能可变借用
        let mut hover = std::mem::take(&mut self.hover);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    // 圆角 + 深色填充 + 细边框，构成气泡本体
                    .fill(PANEL_BG)
                    .corner_radius(egui::CornerRadius::same(12))
                    .stroke(egui::Stroke::new(1.0, PANEL_BORDER))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ui, |ui| {
                // 布局分三段：顶部（标题 + 入口按钮 + 错误提示）固定、
                // 底部（数据目录 + 退出）固定、中间内容可滚动。
                // 这样即使账号很多或出现很长的错误提示，也不会把页脚挤出去与正文重叠。
                egui::Panel::top("panel_header")
                    .frame(egui::Frame::NONE.fill(PANEL_BG))
                    .show(ui, |ui| {
                        draw_header(ui, &mut open_settings, self.pinned, &mut toggle_pin);

                        if let Some(reason) = storage_error.as_deref() {
                            ui.add_space(12.0);
                            draw_storage_banner(ui, reason);
                        }
                    });

                egui::Panel::bottom("panel_footer")
                    .frame(
                        // 页脚要不透明并留出上边距：滚动区的内容一旦超出，
                        // 会正好被页脚裁掉，透明背景会让被裁的卡片文字"压"在页脚上。
                        egui::Frame::NONE.fill(PANEL_BG).inner_margin(egui::Margin {
                            left: 0,
                            right: 0,
                            top: 8,
                            bottom: 0,
                        }),
                    )
                    .show(ui, |ui| {
                        draw_footer(ui, toast.as_deref());
                    });

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.add_space(12.0);

                        // 总览卡片始终显示（没有账号时也能从这里登录）
                        draw_account_summary(
                            ui,
                            &cards,
                            &mut open_login,
                            &mut refresh_clicked,
                            at_limit,
                        );
                        ui.add_space(12.0);

                        if cards.is_empty() {
                            draw_status_card(ui);
                            ui.add_space(16.0);
                            draw_empty_state(ui);
                        } else {
                            for card in &cards {
                                draw_account_card(
                                    ui,
                                    card,
                                    &mut toggle_account,
                                    &mut hover,
                                    &mut copy_request,
                                    &mut usage_switch,
                                    &mut delete_request,
                                );
                                ui.add_space(10.0);
                            }
                        }

                        // 给滚动区底部留一点空隙，避免最后一张卡片贴着页脚
                        ui.add_space(6.0);
                    });
            });

        if open_login {
            self.login.open();
        }
        if refresh_clicked {
            // 手动刷新：绕过定时与启动延迟，但仍受"只跑一轮"约束
            self.try_start_refresh(ctx, true);
        }

        if let Some((account_id, range)) = usage_switch {
            self.set_usage_range(ctx, &account_id, range);
        }

        if let Some((account_id, key_id)) = copy_request {
            self.request_copy(ctx, &account_id, &key_id);
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

        // ---- 右下角缩放手柄 ----
        //
        // 无边框窗口的系统缩放边缘只有几个像素宽，而且会把拖拽"抢"走
        // （egui 收不到 pointer delta）。所以手柄**往内缩 8px**，由我们自己算尺寸：
        // 既避开系统命中区，又能画一个看得见的抓手。
        let grip = egui::Rect::from_min_size(
            egui::pos2(ui.max_rect().right() - 24.0, ui.max_rect().bottom() - 24.0),
            egui::vec2(16.0, 16.0),
        );
        let grip_response = ui.interact(
            grip,
            egui::Id::unique("panel-resize-grip"),
            egui::Sense::drag(),
        );
        if grip_response.hovered() || grip_response.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
        }
        if grip_response.dragged() {
            let delta = ui.ctx().input(|input| input.pointer.delta());
            if delta != egui::Vec2::ZERO {
                let current = ui
                    .ctx()
                    .input(|input| input.viewport().inner_rect)
                    .map(|rect| rect.size())
                    .unwrap_or_else(|| egui::vec2(PANEL_WIDTH, PANEL_HEIGHT));
                let next = egui::vec2(
                    (current.x + delta.x).max(MIN_PANEL_WIDTH),
                    (current.y + delta.y).max(MIN_PANEL_HEIGHT),
                );
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(next));
            }
        }

        // 三条斜线，暗示"可以拖"
        let painter = ui.painter();
        let stroke = egui::Stroke::new(
            1.4,
            if grip_response.hovered() {
                ACCENT
            } else {
                TEXT_DIM
            },
        );
        for offset in [3.0, 7.0, 11.0] {
            painter.line_segment(
                [
                    egui::pos2(grip.right() - offset, grip.bottom() - 2.0),
                    egui::pos2(grip.right() - 2.0, grip.bottom() - offset),
                ],
                stroke,
            );
        }

        // 弹窗画在面板之后，保证它在最上层（Modal 自带遮罩与输入拦截）
        let timeout = self.request_timeout();
        self.login.show(ctx, timeout);

        // ---- 删除确认（从托盘子菜单进入） ----
        self.draw_delete_confirm(ctx);

        if open_settings {
            self.open_settings();
        }

        if toggle_pin {
            self.set_pinned(ctx, !self.pinned);
        }

        // ---- 设置页 ----
        self.draw_settings(ctx);

        self.hover = hover;
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
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(PANEL_BG)
                    .corner_radius(egui::CornerRadius::same(12))
                    .stroke(egui::Stroke::new(1.0, PANEL_BORDER))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(280.0);
                ui.label(
                    egui::RichText::new("退出账号并删除记录")
                        .size(fs(14.0))
                        .strong()
                        .color(TEXT_PRIMARY),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(format!(
                        "确定退出 {display} 并删除记录？删除后需要重新登录才能恢复。"
                    ))
                    .size(fs(11.0))
                    .color(TEXT_SECONDARY),
                );
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_sized([88.0, 28.0], egui::Button::new("取消"))
                        .clicked()
                    {
                        cancel = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let delete_button =
                            egui::Button::new("删除").fill(egui::Color32::from_rgb(185, 28, 28));
                        if ui.add_sized([96.0, 28.0], delete_button).clicked() {
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
    let size = fs(16.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = response.hovered();
    let color = if pinned {
        ACCENT
    } else if hovered {
        TEXT_PRIMARY
    } else {
        TEXT_SECONDARY
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

    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// 画一个矢量齿轮（设置入口）。
///
/// 与 chevron 同理：egui 不能渲染 SVG，而齿轮用两三个圆 + 8 根短线就能画得很干净，
/// 比字体符号 `⚙` 在不同系统下大小/粗细不一要稳。
fn draw_gear(ui: &mut egui::Ui) -> egui::Response {
    let size = fs(16.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    let hovered = response.hovered();
    let color = if hovered { ACCENT } else { TEXT_SECONDARY };
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

/// 顶部条：只保留标题、版本号与齿轮。
fn draw_header(ui: &mut egui::Ui, open_settings: &mut bool, pinned: bool, toggle_pin: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("Sub2API 托盘")
                .size(fs(13.0))
                .strong()
                .color(TEXT_PRIMARY),
        );

        // 版本号与齿轮靠右，弱化处理，避免抢标题的视觉重心
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if draw_gear(ui).clicked() {
                *open_settings = true;
            }
            // 图钉：切换"置顶显示"（钉住后点面板外部不会收起）
            if draw_pin(ui, pinned).clicked() {
                *toggle_pin = true;
            }
            ui.label(
                egui::RichText::new(format!("v{}", crate::APP_VERSION))
                    .size(fs(10.0))
                    .color(TEXT_SECONDARY),
            );
        });
    });

    // 标题下的品牌色细线：让面板在深色任务栏旁也能一眼认出是哪个产品
    ui.add_space(6.0);
    let (line_rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 2.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(line_rect, egui::CornerRadius::same(1), ACCENT);
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
        AccountStatus::Online => ACCENT,
        AccountStatus::Refreshing => egui::Color32::from_rgb(96, 165, 250),
        AccountStatus::CredentialsInvalid => egui::Color32::from_rgb(248, 113, 113),
        AccountStatus::Offline => egui::Color32::from_rgb(148, 163, 184),
        AccountStatus::Unknown => TEXT_DIM,
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
        .fill(CARD_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("已登录 {} 个账号", cards.len()))
                        .size(fs(13.0))
                        .strong()
                        .color(TEXT_PRIMARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_sized(
                            [76.0, 22.0],
                            egui::Button::new(egui::RichText::new("立即刷新").size(fs(10.0))),
                        )
                        .on_hover_text("立刻拉取一次状态，不必等定时刷新")
                        .clicked()
                    {
                        *refresh_clicked = true;
                    }

                    // 登录入口：品牌色高亮，放进总览卡片，省掉独立一整行
                    let login = egui::Button::new(
                        egui::RichText::new("＋ 登录")
                            .size(fs(10.0))
                            .strong()
                            .color(egui::Color32::from_rgb(15, 23, 42)),
                    )
                    .fill(ACCENT);
                    let response = if at_limit {
                        ui.add_enabled_ui(false, |ui| ui.add_sized([64.0, 22.0], login))
                            .inner
                    } else {
                        ui.add_sized([64.0, 22.0], login)
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
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(format!(
                    "在线 {online} · 离线 {offline} · 凭据损坏 {invalid}"
                ))
                .size(fs(11.0))
                .color(TEXT_SECONDARY),
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
    let size = fs(16.0);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let center = rect.center();

    let painter = ui.painter();
    // 圆角底片：平时几乎透明，悬浮时随动画淡入，让箭头本身也像一颗"按钮"
    painter.rect_filled(
        rect.shrink(1.0),
        egui::CornerRadius::same(5),
        lerp_color(egui::Color32::from_white_alpha(8), HOVER_CARD_BG, hover_t),
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
            lerp_color(TEXT_SECONDARY, egui::Color32::WHITE, hover_t * 0.8),
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
    copy_request: &mut Option<(String, String)>,
    usage_switch: &mut Option<(String, UsageRange)>,
    delete_request: &mut Option<String>,
) {
    // 悬浮进度：上一帧矩形的命中 + 指数淡入，避免"闪一下"
    let hover_key = format!("acct:{}", card.id);
    let t = hover.t(ui.ctx(), &hover_key);
    let frame = egui::Frame::new()
        .fill(lerp_color(CARD_BG, HOVER_CARD_BG, t))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            // 标题行里的"退出"按钮被点时，不应再触发整行的展开/收起
            let mut exit_clicked = false;
            let header = ui
                .horizontal(|ui| {
                    // 矢量箭头（替代原来的 ▶/▼ 字符：字符在不同字体下粗细不一，很丑）
                    draw_chevron(ui, card.expanded, t);
                    ui.label(
                        egui::RichText::new("●")
                            .size(fs(10.0))
                            .color(status_color(card.status)),
                    );
                    ui.label(
                        egui::RichText::new(&card.display)
                            .size(fs(13.0))
                            .strong()
                            .color(lerp_color(TEXT_PRIMARY, egui::Color32::WHITE, t * 0.45)),
                    );
                    // 展开时右侧给"退出"按钮，角色名弱化在它左边。
                    // 面板被缩窄时只隐藏角色名（退出按钮始终在），否则会挤在一起。
                    if card.expanded {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(egui::RichText::new("退出").size(fs(10.0)))
                                        .small(),
                                )
                                .on_hover_text("退出这个账号并删除本地记录")
                                .clicked()
                            {
                                exit_clicked = true;
                                *delete_request = Some(card.id.clone());
                            }
                            if ui.available_width() > 90.0 {
                                ui.label(
                                    egui::RichText::new(card.role_label)
                                        .size(fs(10.0))
                                        .color(TEXT_SECONDARY),
                                );
                            }
                        });
                    }
                })
                .response
                .interact(egui::Sense::click())
                .on_hover_cursor(egui::CursorIcon::PointingHand);

            if header.clicked() && !exit_clicked {
                *toggle = Some(card.id.clone());
            }

            ui.label(
                egui::RichText::new(&card.base_url)
                    .size(fs(10.0))
                    .color(TEXT_DIM),
            );

            // 收起态只保留"账户 + 网址"；余额、订阅、刷新时间等全部收进展开态，
            // 这样一屏能看更多账号（需求方要求）。
            if card.expanded {
                let mut status_line = card.status.label().to_owned();
                if let Some(balance) = card.balance {
                    status_line.push_str(&format!(" · 余额 ${balance:.2}"));
                }
                ui.label(
                    egui::RichText::new(status_line)
                        .size(fs(11.0))
                        .color(TEXT_SECONDARY),
                );

                if let Some(summary) = &card.summary {
                    ui.label(egui::RichText::new(summary).size(fs(10.0)).color(TEXT_DIM));
                }

                ui.label(
                    egui::RichText::new(&card.refreshed)
                        .size(fs(9.0))
                        .color(TEXT_DIM),
                );

                // 失败原因只展示一行，完整信息在日志里
                if let Some(error) = &card.error {
                    let first_line = error.lines().next().unwrap_or_default();
                    ui.label(
                        egui::RichText::new(first_line)
                            .size(fs(9.0))
                            .color(egui::Color32::from_rgb(248, 113, 113)),
                    );
                }

                ui.add_space(6.0);
                ui.separator();

                // 用量曲线：走用户态接口，看到的是本账号自己的数据
                draw_usage_section(ui, card, usage_switch);
                ui.add_space(6.0);
                ui.separator();

                draw_account_keys(ui, card, hover, copy_request);
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

/// 绘制"用量"区：区间切换按钮 + token 曲线 + 合计。
fn draw_usage_section(
    ui: &mut egui::Ui,
    card: &AccountCard,
    usage_switch: &mut Option<(String, UsageRange)>,
) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("我的用量")
                .size(fs(11.0))
                .strong()
                .color(TEXT_PRIMARY),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // 从右往左排，视觉顺序仍是 24 小时 → 7 天 → 30 天
            for range in UsageRange::ALL.iter().rev() {
                let selected = *range == card.usage_range;
                let text = egui::RichText::new(range.label())
                    .size(fs(9.0))
                    .color(if selected {
                        egui::Color32::from_rgb(15, 23, 42)
                    } else {
                        TEXT_SECONDARY
                    });
                let button = egui::Button::new(text).fill(if selected {
                    ACCENT
                } else {
                    egui::Color32::TRANSPARENT
                });
                if ui.add(button).clicked() && !selected {
                    *usage_switch = Some((card.id.clone(), *range));
                }
            }
        });
    });

    if card.usage_loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("加载用量…")
                    .size(fs(9.0))
                    .color(TEXT_DIM),
            );
        });
        return;
    }

    if let Some(error) = &card.usage_error {
        ui.label(
            egui::RichText::new(error)
                .size(fs(9.0))
                .color(egui::Color32::from_rgb(248, 113, 113)),
        );
        return;
    }

    let has_usage = card
        .usage_points
        .iter()
        .any(|point| point.requests != 0 || point.total_tokens != 0 || point.actual_cost != 0.0);
    if card.usage_points.is_empty() || !has_usage {
        ui.label(
            egui::RichText::new("该区间暂无用量")
                .size(fs(9.0))
                .color(TEXT_DIM),
        );
        return;
    }

    draw_usage_chart(ui, &card.usage_points);

    let total: i64 = card
        .usage_points
        .iter()
        .map(|point| point.total_tokens)
        .sum();
    let requests: i64 = card.usage_points.iter().map(|point| point.requests).sum();
    let cost: f64 = card
        .usage_points
        .iter()
        .map(|point| point.actual_cost)
        .sum();
    ui.label(
        egui::RichText::new(format!(
            "本账号合计 {} tokens · {} 次请求 · ${cost:.3}",
            fmt_tokens(total),
            requests
        ))
        .size(fs(9.0))
        .color(TEXT_SECONDARY),
    );
}

/// 用原生绘制画 token 用量折线：网格 + 面积 + 折线 + 悬浮提示。
///
/// 点数最多 31 个，全部在 CPU 上算完直接交给 painter，
/// 不引入图表库，也不做逐帧分配（`line` 是每帧临时 Vec，但只有几十个点）。
fn draw_usage_chart(ui: &mut egui::Ui, points: &[crate::api::UsagePoint]) {
    let width = ui.available_width().max(80.0);
    let axis_height = fs(12.0);
    let plot_height = fs(72.0);
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
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(12)),
        );
    }

    let max_value = points
        .iter()
        .map(|point| point.total_tokens)
        .max()
        .unwrap_or(0)
        .max(1) as f32;
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

    let line: Vec<egui::Pos2> = points
        .iter()
        .enumerate()
        .map(|(index, point)| egui::pos2(x_at(index), y_at(point.total_tokens as f32)))
        .collect();

    let mut area = line.clone();
    let left = line.first().copied().unwrap_or(plot.center());
    let right = line.last().copied().unwrap_or(plot.center());
    area.push(egui::pos2(right.x, plot.bottom()));
    area.push(egui::pos2(left.x, plot.bottom()));
    painter.add(egui::Shape::convex_polygon(
        area,
        egui::Color32::from_rgba_unmultiplied(121, 244, 189, 36),
        egui::Stroke::NONE,
    ));
    painter.add(egui::Shape::line(
        line.clone(),
        egui::Stroke::new(1.8, ACCENT),
    ));

    // 只给有数据的时间桶画点，避免补零后的 0 值把曲线压成一排圆点。
    for (index, point) in points.iter().enumerate() {
        if point.total_tokens > 0 {
            painter.circle_filled(line[index], 2.0, ACCENT);
        }
    }

    if let Some(pointer) = response.hover_pos().filter(|pos| plot.contains(*pos)) {
        let ratio = ((pointer.x - plot.left()) / plot.width()).clamp(0.0, 1.0);
        let index = (ratio * (count.max(2) - 1) as f32).round() as usize;
        let index = index.min(count.saturating_sub(1));
        let x = x_at(index);
        painter.line_segment(
            [egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())],
            egui::Stroke::new(1.0, egui::Color32::from_white_alpha(70)),
        );
        painter.circle_filled(
            egui::pos2(x, y_at(points[index].total_tokens as f32)),
            3.2,
            egui::Color32::WHITE,
        );

        let text = format!(
            "{}\n{} tokens · {} 次请求 · ${:.3}",
            points[index].label,
            fmt_tokens(points[index].total_tokens),
            points[index].requests,
            points[index].actual_cost
        );
        let galley =
            painter.layout_no_wrap(text, egui::FontId::proportional(fs(9.0)), TEXT_PRIMARY);
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
            egui::CornerRadius::same(4),
            egui::Color32::from_rgba_unmultiplied(15, 23, 42, 240),
        );
        painter.galley(pos, galley, TEXT_PRIMARY);
    }

    // 横轴只标首尾两个真实时间桶，空间足够时再补中间点。
    let axis_font = egui::FontId::proportional(fs(8.0));
    let first = short_usage_label(&points[0].label);
    let last = short_usage_label(&points[count.saturating_sub(1)].label);
    painter.text(
        egui::pos2(plot.left(), rect.bottom()),
        egui::Align2::LEFT_BOTTOM,
        first,
        axis_font.clone(),
        TEXT_DIM,
    );
    if count > 1 {
        painter.text(
            egui::pos2(plot.right(), rect.bottom()),
            egui::Align2::RIGHT_BOTTOM,
            last,
            axis_font.clone(),
            TEXT_DIM,
        );
    }
    if count > 2 {
        let middle = short_usage_label(&points[count / 2].label);
        painter.text(
            egui::pos2(plot.center().x, rect.bottom()),
            egui::Align2::CENTER_BOTTOM,
            middle,
            axis_font,
            TEXT_DIM,
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
    copy_request: &mut Option<(String, String)>,
) {
    let Some(state) = &card.keys else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("正在拉取 Key…")
                    .size(fs(10.0))
                    .color(TEXT_SECONDARY),
            );
        });
        return;
    };

    if state.loading {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                egui::RichText::new("正在拉取 Key…")
                    .size(fs(10.0))
                    .color(TEXT_SECONDARY),
            );
        });
        return;
    }

    if let Some(error) = &state.error {
        ui.label(
            egui::RichText::new(error)
                .size(fs(10.0))
                .color(egui::Color32::from_rgb(248, 113, 113)),
        );
        // 离线回退：明确告诉用户这是缓存，数据可能已经变化
        if state.cached {
            ui.label(
                egui::RichText::new("缓存数据 · 可能已过期，联网后会刷新")
                    .size(fs(9.0))
                    .color(TEXT_DIM),
            );
        }
    }
    if state.error.is_some() && !state.cached {
        return;
    }

    if state.keys.is_empty() {
        ui.label(
            egui::RichText::new("该账号没有 API Key")
                .size(fs(10.0))
                .color(TEXT_DIM),
        );
        return;
    }

    for key in &state.keys {
        ui.add_space(3.0);

        let hover_key = format!("key:{}:{}", card.id, key.id);
        let kt = hover.t(ui.ctx(), &hover_key);
        // 底色随淡入进度插值：0 时完全透明，1 时是比卡片略亮的深蓝，
        // 文字同时提亮，保证高亮后依然清晰（不再"白底盖字"）。
        let row_frame = egui::Frame::new()
            .fill(lerp_color(egui::Color32::TRANSPARENT, HOVER_CARD_BG, kt))
            .stroke(egui::Stroke::new(
                1.0,
                lerp_color(egui::Color32::TRANSPARENT, ACCENT, kt * 0.7),
            ))
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(7, 4))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                // 第一行：名称 + 打码值（左），订阅名（右）——订阅右置可省掉一整行
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{}  {}", key.name, key.masked))
                            .size(fs(11.0))
                            .color(lerp_color(TEXT_PRIMARY, egui::Color32::WHITE, kt * 0.4)),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(group) = key.group.as_ref().or(card.subscription.as_ref()) {
                            ui.label(egui::RichText::new(group).size(fs(9.0)).color(lerp_color(
                                TEXT_DIM,
                                TEXT_SECONDARY,
                                kt,
                            )));
                        }
                    });
                });

                // 第二行：额度与用量
                let used = key.quota_used.unwrap_or(0.0);
                let quota_text = match key.quota {
                    Some(quota) if quota > 0.0 => {
                        format!("已用 ${used:.2} / 总额度 ${quota:.2}")
                    }
                    _ => format!("已用 ${used:.2} / 总额度 不限"),
                };
                let usage_5h = key.usage_5h.unwrap_or(0.0);
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{quota_text} · 近 5 小时 ${usage_5h:.2}"))
                            .size(fs(10.0))
                            .color(lerp_color(TEXT_SECONDARY, egui::Color32::WHITE, kt * 0.3)),
                    );
                    // 悬浮时行内提示"点击复制"：比系统 tooltip 轻，不会盖住内容
                    if kt > 0.05 {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                egui::RichText::new("点击复制")
                                    .size(fs(9.0))
                                    .color(lerp_color(egui::Color32::TRANSPARENT, ACCENT, kt)),
                            );
                        });
                    }
                });
            });

        // 整块可点：点击复制完整 Key（结果由后台线程回传，成功后盖浮窗）
        let row_response = row_frame
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if row_response.clicked() {
            *copy_request = Some((card.id.clone(), key.id.clone()));
        }

        // 刚复制成功：在方块上方盖一个"已复制"浮窗（约 1.6 秒后消失）
        if card.copied_key.as_deref() == Some(key.id.as_str()) {
            let rect = row_frame.response.rect;
            let chip = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 66.0, rect.top() - 12.0),
                egui::vec2(62.0, 20.0),
            );
            let painter = ui.painter();
            painter.rect_filled(chip, egui::CornerRadius::same(10), ACCENT);
            painter.text(
                chip.center(),
                egui::Align2::CENTER_CENTER,
                "已复制",
                egui::FontId::proportional(fs(10.0)),
                egui::Color32::from_rgb(15, 23, 42),
            );
        }

        // 整块可点：点击复制完整 Key（后台取完整值，成功后盖"已复制"浮窗）
        let row_response = row_frame
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand);
        if row_response.clicked() {
            *copy_request = Some((card.id.clone(), key.id.clone()));
        }

        // 刚复制成功：在方块上方盖浮窗，约 1.6 秒后自动消失
        if card.copied_key.as_deref() == Some(key.id.as_str()) {
            let rect = row_frame.response.rect;
            let chip = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 70.0, rect.top() - 12.0),
                egui::vec2(64.0, 20.0),
            );
            let painter = ui.painter();
            painter.rect_filled(chip, egui::CornerRadius::same(10), ACCENT);
            painter.text(
                chip.center(),
                egui::Align2::CENTER_CENTER,
                "已复制",
                egui::FontId::proportional(fs(10.0)),
                egui::Color32::from_rgb(15, 23, 42),
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
        .fill(CARD_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            // 撑满宽度，否则卡片会按内容收缩，看起来像个小方块
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("●")
                        .size(fs(12.0))
                        .color(TEXT_SECONDARY),
                );
                ui.label(
                    egui::RichText::new("尚未登录任何实例")
                        .size(fs(14.0))
                        .strong()
                        .color(TEXT_PRIMARY),
                );
            });

            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("配置 Sub2API 实例地址并登录后，托盘会定时拉取账户状态。")
                    .size(fs(12.0))
                    .color(TEXT_SECONDARY),
            );
        });
}

/// 绘制账户列表的占位区。
///
/// 面板高度是固定的，账户列表接入前这里必须有点内容，否则面板下半截会是一大片空白。
/// 等真实列表做好后，直接把这个函数替换成滚动列表即可。
fn draw_empty_state(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(EMPTY_BG)
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new("账户状态列表")
                        .size(fs(12.0))
                        .color(TEXT_SECONDARY),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("登录后按账户展示在线状态、剩余额度与最近使用时间。")
                        .size(fs(11.0))
                        .color(TEXT_DIM),
                );
                ui.add_space(6.0);
            });
        });
}

/// 绘制页脚：成功提示（可选）+ 数据目录提示 + 操作提示 + 退出按钮。
///
/// `data_line` 与 `toast` 由调用方提前算好（见 `draw_panel`），
/// 这样闭包里不需要再借用 `self`。
fn draw_footer(ui: &mut egui::Ui, toast: Option<&str>) {
    // 底栏只保留操作反馈（如"已复制"）；退出入口统一放在托盘右键菜单里。
    if let Some(text) = toast {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("✓ {text}"))
                    .size(fs(11.0))
                    .color(ACCENT),
            );
        });
    }
}

/// 把登录成功的草稿落库：加密凭据 → 组装账号 → 去重并写盘。
///
/// 返回 `Err` 时携带的是**可以直接展示**的中文原因（弹窗会原样显示），
/// 这样"重复账号""磁盘满""DPAPI 失败"都能给用户一个可行动的提示。
///
/// 这里刻意先加密再 `add`：
/// `add` 内部会做唯一性校验并原子写盘，任何一步失败都不会留下半条记录。
fn save_login(store: &mut crate::data::DataStore, draft: &LoginDraft) -> Result<(), String> {
    // 需求方要求：同时最多 10 个账号。在落库前拦下，避免登录成功却保存失败。
    if store.accounts().len() >= MAX_ACCOUNTS {
        return Err(format!(
            "最多同时登录 {MAX_ACCOUNTS} 个账号，请先在托盘里删除一个再添加"
        ));
    }

    // 先用空 secrets 建出账号，拿到它的 id（macOS 钥匙串条目以 id 为 account）
    let mut account = Account::new(
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
        .fill(egui::Color32::from_rgb(69, 26, 26))
        .corner_radius(egui::CornerRadius::same(10))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(153, 27, 27)))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new("数据目录不可写，账号无法保存")
                    .size(fs(12.0))
                    .strong()
                    .color(egui::Color32::from_rgb(252, 165, 165)),
            );
            ui.add_space(4.0);
            // 只显示首行：完整错误链可能很长，界面放不下，完整内容在日志里
            ui.label(
                egui::RichText::new(reason.lines().next().unwrap_or_default())
                    .size(fs(10.0))
                    .color(egui::Color32::from_rgb(254, 202, 202)),
            );
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new("请把程序移动到可写目录（例如桌面或 D 盘）后重启。")
                    .size(fs(10.0))
                    .color(egui::Color32::from_rgb(254, 202, 202)),
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
        assert_eq!(status_color(AccountStatus::Online), ACCENT);
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

    #[test]
    fn token数量格式化() {
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1660), "1.7k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }
}

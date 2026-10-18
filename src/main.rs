//! Sub2API 托盘应用的入口。
//!
//! 这个文件有意保持“薄”：只做三件事，其余细节都交给下面的模块。
//!
//! 1. 初始化日志（开发期观察托盘与面板行为的主要手段）；
//! 2. 描述根视口（也就是那块靠托盘弹出的面板窗口）的初始形态；
//! 3. 初始化数据层，并把 [`app::TrayApp`] 交给 eframe/winit 的事件循环。
//!
//! 模块划分：
//!
//! * [`tray`]：系统托盘图标、右键菜单、托盘事件到应用指令的转发；
//! * [`app`]：面板窗口的显示/隐藏/定位以及界面绘制；
//! * [`fonts`]：加载系统中文字体，避免界面里的中文变成“豆腐块”；
//! * [`paths`]：数据目录解析、可写探测、原子写入；
//! * [`config`]：`app.json` 的读取、默认值与修复；
//! * [`accounts`]：`accounts.json` 的 schema、排序、去重与增删；
//! * [`secrets`]：Windows DPAPI / macOS 钥匙串的凭据加解密；
//! * [`mask`]：账号名打码（只影响展示）；
//! * [`data`]：把上述能力组装成对上层友好的门面。
//!
//! 之所以这样拆，是因为托盘、面板、数据、加密、展示各自独立演进：
//! 后续的登录流程（0003）与账号菜单（0004）只会改其中一两个模块。

// Windows 的 release 构建不弹出额外的控制台窗口。
// 代价是 release 版本看不到日志输出；排障请用 debug 构建，
// 或者按 docs/plans 里“日志落盘”的规划，后续把日志写入本地文件。
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod accounts;
mod api;
mod app;
mod config;
mod data;
mod fonts;
mod login;
mod mask;
mod paths;
mod refresh;
mod secrets;
mod single_instance;
mod state;
mod tray;

/// 应用显示名称：同时用作窗口标题和托盘提示文字的前缀。
pub const APP_NAME: &str = "Sub2API 托盘";

/// 编译期写入的版本号，来源是 `Cargo.toml` 里的 `package.version`。
///
/// 用 `env!` 而不是运行时读取，保证界面里展示的版本和二进制完全一致。
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 程序入口。
///
/// 返回值用 `anyhow::Result`，这样 `main` 内部可以直接用 `?` 传播错误；
/// 一旦启动失败（例如创建托盘图标失败），控制台或日志里能看到完整的原因链，
/// 而不是静默退出。
fn main() -> anyhow::Result<()> {
    init_logging();

    log::info!("{APP_NAME} v{APP_VERSION} 启动");

    // 数据层初始化放在创建窗口之前：
    // 一来失败时能立刻写进日志，二来面板第一帧就要决定是否显示"数据目录不可写"提示。
    // 这里刻意不返回 Err：托盘程序即使读不到配置，也应该能启动并把问题告诉用户，
    // 而不是在用户双击图标后什么都不发生。
    // 单实例：必须在写任何数据文件之前拿到锁，否则"第二个副本"已经有机会改盘
    let _instance_guard = match single_instance::acquire() {
        Ok(guard) => guard,
        Err(err) => {
            log::warn!("{err:#}，本次启动退出");
            eprintln!("{err:#}");
            return Ok(());
        }
    };

    let data_layer = data::init();

    // 根视口的初始形态。
    // 这些属性只是“初值”：运行期间面板会被反复隐藏/显示，
    // 每次显示前的最终位置由 app 模块按托盘图标的实际位置重新计算。
    let viewport = egui::ViewportBuilder::default()
        .with_title(APP_NAME)
        .with_inner_size([app::PANEL_WIDTH, app::PANEL_HEIGHT])
        // 托盘气泡面板的尺寸固定，避免用户拖动后内部布局错位
        .with_resizable(false)
        // 去掉系统标题栏，做成一整块贴附托盘的气泡
        .with_decorations(false)
        // 允许圆角以外的区域完全透明，视觉上才像“悬浮气泡”而不是一个方块窗口
        .with_transparent(true)
        // 面板是附属窗口，不应该在任务栏（Windows）/ Dock（macOS）里再出现一个条目
        .with_taskbar(false)
        .with_always_on_top()
        // 启动时不显示，等用户第一次点击托盘图标再弹出
        .with_visible(false)
        .with_icon(window_icon());

    let native_options = eframe::NativeOptions {
        viewport,
        // 固定使用 glow（OpenGL）后端。
        // wgpu 在 Windows 上无法提供带 alpha 的交换链，透明窗口会退化成不透明，
        // 面板圆角就会露出方形底色；这是一个已知的平台限制，不是我们的 bug。
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };

    eframe::run_native(
        APP_NAME,
        native_options,
        // 创建回调在 winit 事件循环开始运行之后、主线程上执行，
        // 这一点很关键：tray-icon 要求事件循环已经在运行（macOS 尤其严格），
        // 因此托盘图标必须在这里创建，而不能在 main 的最前面创建。
        //
        // 数据层用 `move` 送进闭包：创建回调是 `FnOnce`，移动所有权可以避免额外的共享状态。
        Box::new(move |cc| Ok(Box::new(app::TrayApp::new(cc, data_layer)?))),
    )
    .map_err(|err| anyhow::anyhow!("eframe 事件循环启动失败：{err}"))
}

/// 初始化日志。
///
/// 默认级别为 `info`；需要观察托盘事件细节时，
/// 在启动前设置环境变量 `RUST_LOG=debug`（Windows PowerShell 示例）：
///
/// ```powershell
/// $env:RUST_LOG = "debug"; cargo run
/// ```
fn init_logging() {
    let env = env_logger::Env::default().default_filter_or("info");
    env_logger::Builder::from_env(env)
        // 带毫秒的时间戳，便于判断“点击托盘 → 面板弹出”之间的延迟
        .format_timestamp_millis()
        .init();
}

/// 构造窗口图标。
///
/// 复用托盘图标的那份 PNG：虽然面板本身不显示在任务栏里，
/// 但窗口图标仍会影响部分平台的窗口切换器与 Alt+Tab 列表。
/// 图标解码失败不应该让程序启动失败，因此这里降级为默认图标并记录警告。
fn window_icon() -> egui::IconData {
    match tray::load_icon_rgba() {
        Ok((rgba, width, height)) => egui::IconData {
            rgba,
            width,
            height,
        },
        Err(err) => {
            log::warn!("窗口图标加载失败，改用默认图标：{err:#}");
            egui::IconData::default()
        }
    }
}

/// 测试辅助：本地假实例（`TcpListener`）的公共部件。
///
/// 为什么需要它：三个端到端用例都要"起一个假 Sub2API、读请求、回 JSON"。
/// 把这些细节集中在这里，既避免复制粘贴，也把踩过的坑固化下来：
///
/// 1. 必须**读完整个请求体**再关闭连接。只读请求头就关闭时，Windows 会因为
///    套接字里还有未读数据而发 RST，客户端偶发 `os error 10053`；
/// 2. 网络用例串行执行（[`lock_network`]）：并行起多个 loopback 监听仍会抖动；
/// 3. 即便做到上面两点，本机安全软件仍会偶发中止 loopback 连接（同样是 10053）。
///    这不是代码缺陷，所以用例允许对**网络类错误**做有限重试（[`is_transient`]），
///    其它错误（解析、鉴权、断言失败）不重试，避免掩盖真实问题。
///
/// 放在文件末尾是因为 clippy 的 `items_after_test_module` 不允许在测试模块之后再定义条目。
/// 只影响测试，不影响运行时行为。
#[cfg(test)]
pub mod test_support {
    use std::io::{Read as _, Write as _};
    use std::net::TcpStream;
    use std::sync::{Mutex, MutexGuard};

    /// 网络用例的全局互斥锁。
    pub static NETWORK_LOCK: Mutex<()> = Mutex::new(());

    /// 获取网络用例锁；即使之前有测试 panic 过也能继续（忽略中毒）。
    pub fn lock_network() -> MutexGuard<'static, ()> {
        NETWORK_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 判断一条错误信息是否属于"环境导致的临时网络失败"。
    ///
    /// 只认两种：我们自己的网络文案（"无法连接…"）与 Windows 的 10053
    /// （软件中止了一个已建立的连接，常见于安全软件干预 loopback）。
    /// 其它错误（解析失败、401、断言不符）必须原样暴露，不能被重试掩盖。
    pub fn is_transient(message: &str) -> bool {
        message.contains("无法连接") || message.contains("10053")
    }

    /// 网络用例的重试次数（含首次）。
    pub const RETRY_ATTEMPTS: usize = 3;

    /// 临时网络失败之间的等待，避免瞬间重连再次被中止。
    pub const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

    /// 重试一个返回 `Result<_, String>` 的操作：仅当错误信息是临时网络错误时重试。
    ///
    /// 登录线程与刷新线程把错误包装成了字符串（`WorkerReply::Failed` /
    /// `RefreshOutcome::Offline`），所以这里用字符串判断；非网络错误立即返回。
    pub fn retry_message<T>(mut op: impl FnMut() -> Result<T, String>) -> Result<T, String> {
        let mut last_error = None;

        for attempt in 1..=RETRY_ATTEMPTS {
            match op() {
                Ok(value) => return Ok(value),
                Err(message) if is_transient(&message) && attempt < RETRY_ATTEMPTS => {
                    eprintln!("临时网络错误（第 {attempt} 次尝试）：{message}，稍后重试");
                    last_error = Some(message);
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(message) => return Err(message),
            }
        }

        Err(last_error.unwrap_or_else(|| "重试次数耗尽".to_owned()))
    }

    /// 读取一个完整的 HTTP 请求（请求头 + 按 `content-length` 读完请求体）。
    ///
    /// 返回原始文本（请求行 + 头 + body），调用方按需做包含判断。
    /// 读不到数据时返回已读到的部分，不 panic。
    pub fn read_request(stream: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0_u8; 1024];

        // 1) 读到头部结束（\r\n\r\n）
        let mut header_end = None;
        while header_end.is_none() {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(position) = buf.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        header_end = Some(position + 4);
                    }
                }
                Err(_) => break,
            }
        }

        // 2) 按 content-length 读完 body，避免"提前关闭 → RST"
        if let Some(end) = header_end {
            let headers = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let content_length = headers
                .split("content-length:")
                .nth(1)
                .and_then(|rest| rest.split("\r\n").next())
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);

            while buf.len() < end + content_length {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
            }
        }

        String::from_utf8_lossy(&buf).into_owned()
    }

    /// 回一个 JSON 响应并正常关闭连接（不设置 keep-alive，客户端会自己重连）。
    pub fn write_json(stream: &mut TcpStream, status_line: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).ok();
        stream.flush().ok();
    }
}

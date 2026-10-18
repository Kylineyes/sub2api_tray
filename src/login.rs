//! 登录弹窗：面板顶部入口的模态表单 + 后台登录线程 + 2FA 第二步。
//!
//! # 职责边界
//!
//! * 本模块只负责"问用户要三个字段、把请求发出去、把结果翻译成界面状态"；
//! * 真正落库（加密保存、去重、写 `accounts.json`）在 `app` 里完成，
//!   因为那需要 [`crate::data::DataStore`]，而弹窗不应该依赖数据层；
//! * 网络细节（信封解析、错误分类）在 [`crate::api`]，本模块只消费 `WorkerReply`。
//!
//! # 线程模型
//!
//! `ureq` 是阻塞客户端，绝不能在界面线程里调用。所以每次提交都 `spawn` 一个线程，
//! 结果通过 `mpsc` 回传，并用 `egui::Context::request_repaint()` 唤醒界面：
//!
//! ```text
//! 点击确认 → 校验输入 → spawn(登录线程) → 界面继续跑
//!                                   ↓
//!                      mpsc<WorkerReply> → poll() → 更新弹窗状态
//! ```
//!
//! # 状态机
//!
//! ```text
//! Editing ──提交──▶ Submitting ──成功──▶ (由 app 关闭弹窗)
//!    ▲                  │
//!    │                  ├──需要 2FA──▶ NeedTotp ──提交验证码──▶ Submitting
//!    └──────────────────┴──失败──────▶ Editing（显示错误）
//! ```

use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use crate::accounts::{self, AccountRole};
use crate::api::{HttpClient, LoginOutcome};
use crate::secrets::Credentials;

/// 弹窗宽度（egui 点）。面板总宽 360，留出边距后 300 比较合适。
const DIALOG_WIDTH: f32 = 300.0;

/// 面板里的输入框统一用这个高度，视觉上更整齐。
const INPUT_HEIGHT: f32 = 26.0;

/// 弹窗当前阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// 填写链接/账户/密码。
    Editing,
    /// 请求进行中（输入与按钮全部禁用）。
    Submitting,
    /// 需要 6 位两步验证码。
    NeedTotp,
}

/// 表单字段。
#[derive(Debug, Default, Clone)]
struct LoginForm {
    /// 实例地址（用户输入原样保存，提交时再归一化）。
    base_url: String,
    /// 登录邮箱。
    email: String,
    /// 密码：只存在内存里，提交后立刻清空。
    password: String,
    /// 6 位验证码（仅在 2FA 阶段使用）。
    totp_code: String,
}

/// 登录成功后交给 `app` 落库的数据。
///
/// 这里带上 `credentials` 而不是直接写 `accounts.json`，是为了让"加密 + 去重 + 写盘"
/// 这些会失败的步骤集中在一个地方处理，失败时弹窗还能保持打开并显示原因。
#[derive(Debug, Clone)]
pub struct LoginDraft {
    /// 归一化后的实例地址。
    pub base_url: String,
    /// 登录邮箱。
    pub email: String,
    /// 角色（`/auth/me` 成功时取自服务端，失败时为 `Unknown`）。
    pub role: AccountRole,
    /// 明文凭据，交给 `app` 加密后落盘。
    pub credentials: Credentials,
    /// access token 过期时间（RFC3339），后端没给则为 `None`。
    pub expires_at: Option<String>,
}

/// 后台线程回传的结果。
enum WorkerReply {
    /// 登录成功（含 2FA 完成）。
    Success(LoginDraft),
    /// 该账号需要两步验证。
    NeedsTotp {
        temp_token: String,
        masked_email: Option<String>,
    },
    /// 失败，内容已经是可直接展示的中文文案。
    Failed(String),
}

/// 登录弹窗。
pub struct LoginDialog {
    /// 是否正在显示。
    open: bool,
    /// 表单内容。
    form: LoginForm,
    /// 当前阶段。
    stage: Stage,
    /// 当前错误提示（展示后不清空，直到用户再次提交）。
    error: Option<String>,
    /// 2FA 会话令牌，仅在 [`Stage::NeedTotp`] 期间有值。
    temp_token: Option<String>,
    /// 服务端返回的打码邮箱，用于"正在为 xxx 输入验证码"的提示。
    masked_email: Option<String>,
    /// 后台线程的结果通道；没有进行中的请求时为 `None`。
    receiver: Option<Receiver<WorkerReply>>,
    /// egui 上下文，用于后台线程唤醒界面。
    ctx: egui::Context,
}

impl LoginDialog {
    /// 创建一个关闭状态的弹窗。
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            open: false,
            form: LoginForm::default(),
            stage: Stage::Editing,
            error: None,
            temp_token: None,
            masked_email: None,
            receiver: None,
            ctx,
        }
    }

    /// 是否正在显示（`app` 用它决定"失焦是否自动收起面板"与"Esc 关谁"）。
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 打开弹窗。
    ///
    /// 保留上次填的地址与账号（方便连续添加同一实例的多个账号），
    /// 但**清空密码与验证码**：密码不该在弹窗关闭后继续留在内存里。
    pub fn open(&mut self) {
        self.open = true;
        self.stage = Stage::Editing;
        self.error = None;
        self.temp_token = None;
        self.masked_email = None;
        self.form.password.clear();
        self.form.totp_code.clear();
        self.ctx.request_repaint();
    }

    /// 关闭弹窗，并清理敏感输入与进行中的请求。
    pub fn close(&mut self) {
        self.open = false;
        self.stage = Stage::Editing;
        self.form.password.clear();
        self.form.totp_code.clear();
        self.error = None;
        self.temp_token = None;
        self.masked_email = None;
        // 丢弃接收端：后台线程即使跑完也只会往一个已关闭的通道里发，不会 panic
        self.receiver = None;
        self.ctx.request_repaint();
    }

    /// 由 `app` 在落库失败时回填错误（例如"该账号已存在"）。
    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        self.stage = Stage::Editing;
        self.ctx.request_repaint();
    }

    /// 非阻塞地取一次后台结果。
    ///
    /// * 成功 → 返回 [`LoginDraft`]，由 `app` 落库后关闭弹窗；
    /// * 需要 2FA / 失败 → 在内部更新状态，返回 `None`。
    pub fn poll(&mut self) -> Option<LoginDraft> {
        // 没有进行中的请求时直接返回；`?` 在这里等价于 let-else，但更短
        let reply = self.receiver.as_ref()?.try_recv();

        match reply {
            Ok(WorkerReply::Success(draft)) => {
                self.receiver = None;
                self.stage = Stage::Editing;
                // 密码已经进了 draft 的凭据里，这里把界面上的副本清掉
                self.form.password.clear();
                self.form.totp_code.clear();
                Some(draft)
            }
            Ok(WorkerReply::NeedsTotp {
                temp_token,
                masked_email,
            }) => {
                self.receiver = None;
                self.stage = Stage::NeedTotp;
                self.temp_token = Some(temp_token);
                self.masked_email = masked_email;
                self.error = None;
                self.form.totp_code.clear();
                None
            }
            Ok(WorkerReply::Failed(message)) => {
                self.receiver = None;
                // 验证码阶段失败时留在当前阶段，方便直接重输验证码
                self.stage = if self.temp_token.is_some() {
                    Stage::NeedTotp
                } else {
                    Stage::Editing
                };
                self.error = Some(message);
                None
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                // 线程 panic 或被中断：不让界面永远停在"登录中…"
                self.receiver = None;
                self.stage = Stage::Editing;
                self.error = Some("登录线程意外退出，请重试".to_owned());
                None
            }
        }
    }

    /// 绘制弹窗。
    ///
    /// `timeout` 来自 `app.json` 的 `requestTimeoutSeconds`，由 `app` 每帧传进来，
    /// 这样修改配置后只需重启，不必把配置塞进弹窗。
    pub fn show(&mut self, ctx: &egui::Context, timeout: Duration) {
        if !self.open {
            return;
        }

        let submitting = self.stage == Stage::Submitting;
        let need_totp = self.stage == Stage::NeedTotp;

        // 按钮的点击结果先记在局部变量里，等 Modal 返回后再改 `self`：
        // 闭包里同时可变借用 `self.form` 与 `self.error`，再调用 `self.submit()`
        // 会触发借用冲突；把"动作"与"状态变更"分开最省事。
        let mut submit_requested = false;
        let mut cancel_requested = false;

        let modal = egui::Modal::new(egui::Id::unique("login-dialog"))
            .backdrop_color(egui::Color32::from_black_alpha(150))
            .frame(
                egui::Frame::new()
                    .fill(crate::app::PANEL_BG)
                    .corner_radius(egui::CornerRadius::same(12))
                    .stroke(egui::Stroke::new(1.0, crate::app::PANEL_BORDER))
                    .inner_margin(egui::Margin::same(16)),
            )
            .show(ctx, |ui| {
                ui.set_width(DIALOG_WIDTH);

                ui.label(
                    egui::RichText::new("登陆新用户")
                        .size(crate::app::fs(15.0))
                        .strong()
                        .color(crate::app::TEXT_PRIMARY),
                );
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("填写 Sub2API 实例地址与登录信息")
                        .size(crate::app::fs(10.0))
                        .color(crate::app::TEXT_SECONDARY),
                );
                ui.add_space(12.0);

                // ---- 链接 ----
                field_label(ui, "链接");
                let url_edit = egui::TextEdit::singleline(&mut self.form.base_url)
                    .hint_text("https://your-sub2api-host:8080")
                    .desired_width(f32::INFINITY)
                    .interactive(!submitting && !need_totp);
                ui.add_sized([DIALOG_WIDTH, INPUT_HEIGHT], url_edit);

                // ---- 账户名 ----
                ui.add_space(8.0);
                field_label(ui, "账户名（邮箱）");
                let email_edit = egui::TextEdit::singleline(&mut self.form.email)
                    .hint_text("you@example.com")
                    .desired_width(f32::INFINITY)
                    .interactive(!submitting && !need_totp);
                ui.add_sized([DIALOG_WIDTH, INPUT_HEIGHT], email_edit);

                // ---- 密码 ----
                ui.add_space(8.0);
                field_label(ui, "密码");
                let password_edit = egui::TextEdit::singleline(&mut self.form.password)
                    .hint_text("登录密码")
                    .password(true)
                    .desired_width(f32::INFINITY)
                    // 验证码阶段不允许改密码：改了就得重新走第一步
                    .interactive(!submitting && !need_totp);
                ui.add_sized([DIALOG_WIDTH, INPUT_HEIGHT], password_edit);

                // ---- 验证码（仅 2FA 阶段） ----
                if need_totp {
                    ui.add_space(8.0);
                    field_label(ui, "两步验证码");
                    let totp_edit = egui::TextEdit::singleline(&mut self.form.totp_code)
                        .hint_text("6 位数字")
                        .desired_width(f32::INFINITY)
                        .char_limit(6)
                        .interactive(!submitting);
                    ui.add_sized([DIALOG_WIDTH, INPUT_HEIGHT], totp_edit);
                    ui.add_space(4.0);
                    let hint = match &self.masked_email {
                        Some(masked) => format!("该账号已开启两步验证：{masked}"),
                        None => "该账号已开启两步验证，请输入验证器中的 6 位数字".to_owned(),
                    };
                    ui.label(
                        egui::RichText::new(hint)
                            .size(crate::app::fs(10.0))
                            .color(crate::app::TEXT_SECONDARY),
                    );
                }

                // ---- 错误提示 ----
                if let Some(error) = self.error.as_deref() {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(error)
                            .size(crate::app::fs(11.0))
                            .color(egui::Color32::from_rgb(248, 113, 113)),
                    );
                }

                ui.add_space(14.0);

                // ---- 按钮 ----
                // 回车等价于点"确认"：表单填完后不用去找按钮
                if ui.input(|input| input.key_pressed(egui::Key::Enter)) && !submitting {
                    submit_requested = true;
                }

                ui.horizontal(|ui| {
                    if ui
                        .add_sized([96.0, 28.0], egui::Button::new("取消"))
                        .clicked()
                    {
                        cancel_requested = true;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if submitting {
                            ui.spinner();
                            ui.label(
                                egui::RichText::new("登录中…")
                                    .size(crate::app::fs(11.0))
                                    .color(crate::app::TEXT_SECONDARY),
                            );
                        } else {
                            let label = if need_totp {
                                "验证并登录"
                            } else {
                                "确认"
                            };
                            if ui
                                .add_sized([96.0, 28.0], egui::Button::new(label))
                                .clicked()
                            {
                                submit_requested = true;
                            }
                        }
                    });
                });
            });

        // 点击遮罩关闭：提交中不允许关闭，避免"请求还在跑、界面已经没了"
        if modal.backdrop_response.clicked() && !submitting {
            self.close();
            return;
        }
        if cancel_requested && !submitting {
            self.close();
            return;
        }
        if submit_requested {
            self.submit(timeout);
        }
    }

    /// 校验表单并发起请求。
    ///
    /// 校验顺序与用户填表顺序一致（链接 → 账号 → 密码 → 验证码），
    /// 一次只提示一个错误，避免一屏红字。
    fn submit(&mut self, timeout: Duration) {
        // 2FA 阶段：只校验验证码，不再校验链接/密码（它们已经在第一步验证过了）
        if self.stage == Stage::NeedTotp {
            let code = self.form.totp_code.trim();
            if code.len() != 6 || !code.chars().all(|ch| ch.is_ascii_digit()) {
                self.error = Some("请输入 6 位数字验证码".to_owned());
                return;
            }
            let Some(temp_token) = self.temp_token.clone() else {
                self.error = Some("验证码会话已失效，请重新登录".to_owned());
                self.stage = Stage::Editing;
                return;
            };
            self.spawn_worker(timeout, Some(temp_token));
            return;
        }

        // 第一步：链接、账号、密码
        let normalized = match accounts::normalize_base_url(&self.form.base_url) {
            Ok(url) => url,
            Err(err) => {
                self.error = Some(format!("{err}"));
                return;
            }
        };
        if !accounts::is_valid_email(&self.form.email) {
            self.error = Some("请输入有效的邮箱地址".to_owned());
            return;
        }
        if self.form.password.is_empty() {
            self.error = Some("请输入密码".to_owned());
            return;
        }

        // 把归一化结果写回表单：用户能直接看到"实际会用的地址"
        self.form.base_url = normalized;
        self.spawn_worker(timeout, None);
    }

    /// 起一个后台线程执行登录（可选：带 2FA 令牌）。
    fn spawn_worker(&mut self, timeout: Duration, temp_token: Option<String>) {
        // 快照：线程里不再借用 self
        let base_url = self.form.base_url.clone();
        let email = self.form.email.clone();
        let password = self.form.password.clone();
        let totp_code = self.form.totp_code.clone();

        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.stage = Stage::Submitting;
        self.error = None;

        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let reply = run_login(base_url, email, password, temp_token, totp_code, timeout);
            // 通道关闭说明用户已经关掉弹窗：忽略发送错误
            let _ = sender.send(reply);
            // 唤醒界面：否则结果要等到下一次鼠标移动才会被消费
            ctx.request_repaint();
        });
    }
}

/// 输入框上方的字段标签。
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(
        egui::RichText::new(text)
            .size(crate::app::fs(10.0))
            .color(crate::app::TEXT_SECONDARY),
    );
}

/// 在后台线程里执行的完整登录流程。
///
/// 顺序：建客户端 → 登录（或 2FA 登录）→ 拉 `/auth/me` 定角色。
/// `/auth/me` 失败不影响登录成功：角色记为 `Unknown`，账号照样保存，
/// 只是后续 0004 的"按角色分岔展示"会退化成通用视图。
fn run_login(
    base_url: String,
    email: String,
    password: String,
    temp_token: Option<String>,
    totp_code: String,
    timeout: Duration,
) -> WorkerReply {
    let client = match HttpClient::new(&base_url, timeout) {
        Ok(client) => client,
        Err(err) => return WorkerReply::Failed(err.user_message()),
    };

    let outcome = match &temp_token {
        Some(token) => client.login_totp(token, totp_code.trim()),
        None => client.login(&email, &password),
    };

    match outcome {
        Err(err) => {
            // 详细原因（可能含服务端原文）只进日志，界面用统一文案
            log::warn!("登录失败：{}", err.detail());
            WorkerReply::Failed(err.user_message())
        }
        Ok(LoginOutcome::NeedsTotp {
            temp_token,
            masked_email,
        }) => WorkerReply::NeedsTotp {
            temp_token,
            masked_email,
        },
        Ok(LoginOutcome::Tokens(tokens)) => {
            let role = match client.me(&tokens.access_token) {
                Ok(me) => me.role,
                Err(err) => {
                    log::warn!(
                        "登录成功但读取当前用户失败，角色记为未知：{}",
                        err.user_message()
                    );
                    AccountRole::Unknown
                }
            };

            WorkerReply::Success(LoginDraft {
                base_url: client.base_url().to_owned(),
                email,
                role,
                credentials: Credentials {
                    password,
                    access_token: tokens.access_token,
                    refresh_token: tokens.refresh_token,
                },
                expires_at: tokens.expires_at,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ApiError;

    fn dialog() -> LoginDialog {
        // 测试里不真正画界面：`egui::Context::default()` 足够让 request_repaint 生效
        LoginDialog::new(egui::Context::default())
    }

    #[test]
    fn 初始状态是关闭的() {
        let mut dlg = dialog();
        assert!(!dlg.is_open());
        assert!(dlg.poll().is_none());
        dlg.close();
        assert!(!dlg.is_open());
    }

    #[test]
    fn 打开时清空密码但保留地址与账号() {
        let mut dlg = dialog();
        dlg.form.base_url = "https://example.com".to_owned();
        dlg.form.email = "user@example.com".to_owned();
        dlg.form.password = "secret".to_owned();
        dlg.form.totp_code = "123456".to_owned();

        dlg.open();

        assert!(dlg.is_open());
        assert_eq!(dlg.form.base_url, "https://example.com");
        assert_eq!(dlg.form.email, "user@example.com");
        assert!(dlg.form.password.is_empty(), "打开时不应复用上次密码");
        assert!(dlg.form.totp_code.is_empty());
        assert_eq!(dlg.stage, Stage::Editing);
    }

    #[test]
    fn 关闭时清空敏感输入() {
        let mut dlg = dialog();
        dlg.open();
        dlg.form.password = "secret".to_owned();
        dlg.temp_token = Some("temp".to_owned());

        dlg.close();

        assert!(dlg.form.password.is_empty());
        assert!(dlg.temp_token.is_none());
        assert!(!dlg.is_open());
    }

    #[test]
    fn 校验失败不会进入提交状态() {
        let mut dlg = dialog();
        dlg.open();

        // 地址为空
        dlg.submit(Duration::from_secs(1));
        assert_eq!(dlg.stage, Stage::Editing);
        assert!(dlg.error.as_deref().unwrap().contains("链接"));

        // 地址合法但邮箱非法
        dlg.form.base_url = "https://example.com".to_owned();
        dlg.form.email = "not-an-email".to_owned();
        dlg.submit(Duration::from_secs(1));
        assert_eq!(dlg.stage, Stage::Editing);
        assert!(dlg.error.as_deref().unwrap().contains("邮箱"));

        // 邮箱合法但密码为空
        dlg.form.email = "user@example.com".to_owned();
        dlg.form.password.clear();
        dlg.submit(Duration::from_secs(1));
        assert_eq!(dlg.stage, Stage::Editing);
        assert!(dlg.error.as_deref().unwrap().contains("密码"));
    }

    #[test]
    fn 验证码阶段校验格式() {
        let mut dlg = dialog();
        dlg.open();
        dlg.stage = Stage::NeedTotp;
        dlg.temp_token = Some("temp-token".to_owned());

        dlg.form.totp_code = "12ab".to_owned();
        dlg.submit(Duration::from_secs(1));
        assert_eq!(dlg.stage, Stage::NeedTotp, "非法验证码不应进入提交");
        assert!(dlg.error.as_deref().unwrap().contains("6 位"));

        // 没有临时令牌时要求重新登录
        dlg.temp_token = None;
        dlg.form.totp_code = "123456".to_owned();
        dlg.submit(Duration::from_secs(1));
        assert_eq!(dlg.stage, Stage::Editing);
        assert!(dlg.error.as_deref().unwrap().contains("重新登录"));
    }

    #[test]
    fn 落库失败时回填错误并回到编辑态() {
        let mut dlg = dialog();
        dlg.open();
        dlg.stage = Stage::Submitting;

        dlg.set_error("该账号已存在");

        assert_eq!(dlg.stage, Stage::Editing);
        assert_eq!(dlg.error.as_deref(), Some("该账号已存在"));
    }

    #[test]
    fn 错误文案来自api分类() {
        assert_eq!(ApiError::Unauthorized.user_message(), "账号或密码可能有误");
        assert_eq!(
            ApiError::RateLimited.user_message(),
            "请求过于频繁，请稍后再试"
        );
    }

    /// 端到端验证后台登录线程：账号密码 → 需要 2FA → 验证码 → 取角色。
    ///
    /// 直接用本地假实例驱动 [`run_login`]，覆盖真实 HTTP 请求、信封解析与 2FA 分支；
    /// 不经过界面，因此不依赖窗口或鼠标事件。
    #[test]
    #[ignore = "loopback 端到端用例：
需在验收时显式运行 cargo test -- --ignored（本机安全软件会偶发中止 loopback 连接）"]
    fn 后台流程在假实例上完成两步验证登录() {
        // 与其它 loopback 用例串行（见 main.rs 的 test_support 说明）
        let _network_guard = crate::test_support::lock_network();
        use std::io::{Read as _, Write as _};
        use std::net::{TcpListener, TcpStream};

        /// 读取完整请求（头部 + 按 content-length 读完 body）。
        ///
        /// 只读头就关闭连接会让 Windows 因套接字里还有未读数据而发 RST，
        /// 客户端偶发 `os error 10053`——这是本用例曾经 flaky 的真正原因。
        fn read_request(stream: &mut TcpStream) -> String {
            let mut buf = Vec::new();
            let mut chunk = [0_u8; 1024];
            let mut header_end = None;
            while header_end.is_none() {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(position) =
                            buf.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            header_end = Some(position + 4);
                        }
                    }
                    Err(_) => break,
                }
            }
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

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取得端口");
        // 非阻塞 accept + 停止标志：比"固定 accept 三次"稳，
        // 因为客户端可能因为连接池重试而多连一次，固定次数会让服务端线程一直卡在 accept。
        listener.set_nonblocking(true).expect("切换为非阻塞监听");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_in_server = std::sync::Arc::clone(&stop);

        let server = std::thread::spawn(move || {
            while !stop_in_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        // 按路径分发，而不是按到达顺序：顺序分发在并行测试下容易被重试打乱
                        let body = if request.contains("/api/v1/auth/login/2fa") {
                            r#"{"code":0,"message":"success","data":{"access_token":"tok-2","refresh_token":"ref-2","expires_in":7200}}"#
                        } else if request.starts_with("POST /api/v1/auth/login ") {
                            r#"{"code":0,"message":"success","data":{"requires_2fa":true,"temp_token":"temp-1","user_email_masked":"u***@e***.com"}}"#
                        } else {
                            r#"{"code":0,"message":"success","data":{"email":"user@example.com","role":"admin"}}"#
                        };
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        stream.write_all(response.as_bytes()).ok();
                        stream.flush().ok();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let base_url = format!("http://{addr}");

        // 第一步：拿到"需要 2FA"
        // 环境噪声重试：仅对临时网络错误重试（见 main.rs 的 test_support 说明）
        let first = crate::test_support::retry_message(|| {
            match run_login(
                base_url.clone(),
                "user@example.com".to_owned(),
                "password".to_owned(),
                None,
                String::new(),
                Duration::from_secs(5),
            ) {
                WorkerReply::Failed(message) => Err(message),
                other => Ok(other),
            }
        })
        .unwrap_or_else(WorkerReply::Failed);
        let temp_token = match first {
            WorkerReply::NeedsTotp { temp_token, .. } => temp_token,
            WorkerReply::Failed(message) => panic!("第一步不应失败：{message}"),
            WorkerReply::Success(_) => panic!("该账号应当要求 2FA"),
        };
        assert_eq!(temp_token, "temp-1");

        // 第二步：带验证码登录，并读取角色
        let second = crate::test_support::retry_message(|| {
            match run_login(
                base_url.clone(),
                "user@example.com".to_owned(),
                "password".to_owned(),
                Some(temp_token.clone()),
                "123456".to_owned(),
                Duration::from_secs(5),
            ) {
                WorkerReply::Failed(message) => Err(message),
                other => Ok(other),
            }
        })
        .unwrap_or_else(WorkerReply::Failed);
        match second {
            WorkerReply::Success(draft) => {
                assert_eq!(draft.role, AccountRole::Admin);
                assert_eq!(draft.credentials.access_token, "tok-2");
                assert_eq!(draft.credentials.refresh_token, "ref-2");
                assert_eq!(draft.credentials.password, "password");
                assert!(draft.expires_at.is_some(), "应当算出过期时间");
            }
            WorkerReply::Failed(message) => panic!("第二步不应失败：{message}"),
            WorkerReply::NeedsTotp { .. } => panic!("验证码提交后不应再要求 2FA"),
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("假实例线程正常结束");
    }

    /// 手工验收：用**真实**实例跑完整登录线程，并可选地把账号写入指定数据目录。
    ///
    /// 默认忽略，需要时显式执行（凭据只从环境变量读取，不写进代码）：
    ///
    /// ```powershell
    /// $env:SUB2API_ACCEPTANCE_URL = "http://your-host:8080"
    /// $env:SUB2API_ACCEPTANCE_EMAIL = "you@example.com"
    /// $env:SUB2API_ACCEPTANCE_PASSWORD = "******"
    /// # 可选：把账号写进这个目录，方便用真实二进制验收菜单/面板
    /// $env:SUB2API_ACCEPTANCE_DATA_DIR = "target/portable-test"
    /// cargo test 真实实例 -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要真实实例与凭据，见函数文档"]
    fn 真实实例完成登录流程并可落库() {
        let read = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("缺少环境变量 {name}（见用例文档）"))
        };
        let base_url = read("SUB2API_ACCEPTANCE_URL");
        let email = read("SUB2API_ACCEPTANCE_EMAIL");
        let password = read("SUB2API_ACCEPTANCE_PASSWORD");

        let reply = run_login(
            base_url,
            email.clone(),
            password,
            None,
            String::new(),
            Duration::from_secs(15),
        );

        let draft = match reply {
            WorkerReply::Success(draft) => draft,
            WorkerReply::Failed(message) => panic!("真实实例登录失败：{message}"),
            WorkerReply::NeedsTotp { .. } => {
                panic!("该账号要求 2FA，本用例只覆盖无 2FA 的验收流程")
            }
        };

        assert_eq!(draft.email, email);
        assert!(!draft.credentials.access_token.is_empty());
        assert!(draft.expires_at.is_some(), "应当算出 token 过期时间");
        eprintln!(
            "真实实例登录流程通过：role={:?} base_url={} token_len={}",
            draft.role,
            draft.base_url,
            draft.credentials.access_token.len()
        );

        // 可选：落库到指定目录，供真实二进制验收使用
        let Ok(dir) = std::env::var("SUB2API_ACCEPTANCE_DATA_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);

        let (mut store, _) = crate::accounts::load_or_create(&dir).expect("加载账号文件");
        let mut account = crate::accounts::Account::new(
            draft.base_url.clone(),
            draft.email.clone(),
            draft.role,
            String::new(),
        );
        account.last_login_at = Some(crate::accounts::now_rfc3339());
        account.token_expires_at = draft.expires_at.clone();
        let secret_store = crate::secrets::system_store();
        account.secrets = crate::secrets::seal_credentials(
            secret_store.as_ref(),
            &account.id,
            &draft.credentials,
        )
        .expect("加密凭据");

        match store.add(account) {
            Ok(()) => eprintln!("账号已写入：{}", dir.display()),
            // 重复账号属于正常情况（第二次跑验收），不算失败
            Err(err) => eprintln!("账号未写入（可能已存在）：{err:#}"),
        }

        // 重新加载一遍，验证"加密写入 → 读回 → 字段完整"
        let (reloaded, _) = crate::accounts::load_or_create(&dir).expect("重新加载账号文件");
        let saved = reloaded
            .accounts()
            .iter()
            .find(|item| item.email.eq_ignore_ascii_case(&draft.email))
            .expect("落库后应当能重新读到该账号");
        assert!(!saved.secrets.is_empty());
        assert!(
            crate::secrets::open_credentials(secret_store.as_ref(), &saved.id, &saved.secrets)
                .is_ok(),
            "重新加载后的凭据应当能解密"
        );
        eprintln!(
            "落库校验通过：共 {} 个账号，凭据可解密",
            reloaded.accounts().len()
        );
    }
}

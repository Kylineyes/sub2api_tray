//! 账号状态刷新：有界并行请求 + token 失效恢复链。
//!
//! # 为什么要有恢复链
//!
//! 登录拿到的 access token 24 小时就过期。如果每轮刷新遇到 401 就标"离线"，
//! 用户第二天打开面板会看到一片红点，却不知道为什么。恢复链的顺序是：
//!
//! ```text
//! 401 ──▶ 用 refresh_token 续期 ──成功──▶ 重试 /auth/me
//!          │失败
//!          ▼
//!        用加密保存的密码重新登录 ──成功──▶ 重试 /auth/me
//!          │失败
//!          ▼
//!        标记「凭据损坏」，等用户手动重新登录
//! ```
//!
//! 每次续期/重登成功后都会把新凭据回传给主线程落库（`rotated`），
//! 所以重启后不需要再走一遍恢复链。
//!
//! # 为什么有界并行
//!
//! 一轮刷新会打 2N 个请求（每个账号 me + summary）。账号多时串行会让最后一户等待
//! 很久，因此改为并行；但小实例仍可能被瞬间打满，所以并发上限固定为
//! [`MAX_CONCURRENT_ACCOUNT_REFRESHES`]，不按账号数量无限开线程。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::accounts::{Account, AccountProvider};
use crate::api::{ApiError, HttpClient, LoginOutcome, MeInfo, SubscriptionSummary};
use crate::secrets::{self, Credentials, SecretStore};

/// 同一轮刷新最多同时处理的账号数。
///
/// 这是网络 I/O 型任务，并发过高不会更快，却更容易触发实例限流；4 个账号
/// 能在多账号刷新速度和小实例承受能力之间取得平衡。
const MAX_CONCURRENT_ACCOUNT_REFRESHES: usize = 4;

/// 一次刷新的结果。
#[derive(Debug)]
pub enum RefreshOutcome {
    /// 刷新成功；`rotated` 非空表示 token 被续期或账号被重新登录过，需要落库。
    Online {
        /// 最新的用户信息（角色、余额）。
        status: MeInfo,
        /// 订阅摘要；接口失败时为 `None`（不影响在线判定）。
        summary: Option<SubscriptionSummary>,
        /// 续期/重登后的新凭据。
        rotated: Option<Credentials>,
    },
    /// 网络或服务端错误；token 可能仍然有效，下一轮会重试。
    Offline(String),
    /// token 与密码都无法恢复，必须人工重新登录。
    CredentialsInvalid(String),
}

/// 单个账号的刷新结果。
#[derive(Debug)]
pub struct RoundResult {
    /// 账号 id。
    pub id: String,
    /// 结果。
    pub outcome: RefreshOutcome,
}

/// 一轮刷新要处理的账号快照。
///
/// 只带线程需要的最小字段：id（钥匙串条目）、地址、邮箱（重登用）、加密凭据。
#[derive(Debug, Clone)]
pub struct RefreshTarget {
    /// 账号 id。
    pub id: String,
    /// 实例地址（已归一化）。
    pub base_url: String,
    /// 登录邮箱（DeepSeek 官方为脱敏 API Key）。
    pub email: String,
    /// 账号来源。
    pub provider: AccountProvider,
    /// 加密凭据块。
    pub secrets: String,
}

impl RefreshTarget {
    /// 从持久化账号构造刷新目标。
    pub fn from_account(account: &Account) -> Self {
        Self {
            id: account.id.clone(),
            base_url: account.base_url.clone(),
            email: account.email.clone(),
            provider: account.provider,
            secrets: account.secrets.clone(),
        }
    }
}

/// 401 之后该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverAction {
    /// 先用 refresh token 续期。
    Refresh,
    /// 再用保存的密码重新登录。
    Relogin,
    /// 都失败过，标记凭据损坏。
    MarkInvalid,
}

/// 根据"已经尝试过几次恢复"决定下一步动作。
///
/// 抽成纯函数是为了能直接测：恢复链一旦写错，用户会看到账号被误判为"凭据损坏"，
/// 这比"刷新慢一点"严重得多。
pub fn next_recovery_action(attempts_done: u8) -> RecoverAction {
    match attempts_done {
        0 => RecoverAction::Refresh,
        1 => RecoverAction::Relogin,
        _ => RecoverAction::MarkInvalid,
    }
}

/// 是否该开始新一轮刷新。
///
/// * 从未刷新过 → 立刻开始（首刷由调用方延后几秒，等托盘/窗口就绪）；
/// * 距离上一轮已超过间隔 → 开始。
pub fn should_start_round(now: Instant, last_round: Option<Instant>, interval: Duration) -> bool {
    match last_round {
        None => true,
        Some(at) => now.saturating_duration_since(at) >= interval,
    }
}

/// 起一个后台线程跑一轮刷新，返回结果通道。
///
/// 账号之间使用有界并行：最多同时处理 [`MAX_CONCURRENT_ACCOUNT_REFRESHES`]
/// 个账号，结果仍按传入账号顺序回传，调用方无需额外排序。线程全部结束后
/// 一次性发送结果；界面线程用 `try_recv` 取（`Ok` 即整轮完成）。
pub fn spawn_round(
    targets: Vec<RefreshTarget>,
    store: Arc<dyn SecretStore>,
    timeout: Duration,
) -> Receiver<Vec<RoundResult>> {
    let (sender, receiver) = mpsc::channel();

    std::thread::spawn(move || {
        let total = targets.len();
        if total == 0 {
            let _ = sender.send(Vec::new());
            return;
        }

        let next_index = AtomicUsize::new(0);
        let (result_tx, result_rx) = mpsc::channel::<(usize, RoundResult)>();
        let worker_count = refresh_worker_count(total);

        // 用 scoped thread 借 target 与 secret store，避免为每个账号克隆大量凭据。
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let result_tx = result_tx.clone();
                let next_index = &next_index;
                let targets = &targets;
                let store = &store;
                scope.spawn(move || {
                    loop {
                        let index = next_index.fetch_add(1, Ordering::Relaxed);
                        if index >= targets.len() {
                            break;
                        }
                        let result = refresh_one(&targets[index], store.as_ref(), timeout);
                        if result_tx.send((index, result)).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        drop(result_tx);

        // 按原账号顺序放回结果，避免并行完成顺序影响界面展示。
        let mut slots: Vec<Option<RoundResult>> = (0..total).map(|_| None).collect();
        for (index, result) in result_rx {
            if let Some(slot) = slots.get_mut(index) {
                *slot = Some(result);
            }
        }
        let results = slots.into_iter().flatten().collect();
        let _ = sender.send(results);
    });

    receiver
}

/// 计算本轮实际启动的刷新线程数。
///
/// 空列表不会走到这里；非空时至少 1 个，最多不超过并发上限。
fn refresh_worker_count(total: usize) -> usize {
    total.clamp(1, MAX_CONCURRENT_ACCOUNT_REFRESHES)
}

/// 刷新单个账号（在后台线程里执行）。
/// DeepSeek 官方账号刷新：只用 API Key 查余额。
fn refresh_deepseek(id: &str, api_key: &str, timeout: Duration) -> RoundResult {
    match crate::deepseek::fetch_balance(api_key, timeout) {
        Ok(balance) => RoundResult {
            id: id.to_owned(),
            outcome: RefreshOutcome::Online {
                status: MeInfo {
                    role: crate::accounts::AccountRole::Unknown,
                    email: Some(crate::deepseek::mask_api_key(api_key)),
                    balance: Some(balance.total_balance),
                },
                summary: None,
                rotated: None,
            },
        },
        Err(ApiError::Unauthorized) => RoundResult {
            id: id.to_owned(),
            outcome: RefreshOutcome::CredentialsInvalid("DeepSeek API Key 无效或已撤销".to_owned()),
        },
        Err(err) => RoundResult {
            id: id.to_owned(),
            outcome: RefreshOutcome::Offline(err.user_message()),
        },
    }
}

fn refresh_one(target: &RefreshTarget, store: &dyn SecretStore, timeout: Duration) -> RoundResult {
    let id = target.id.clone();

    let mut credentials = match secrets::open_credentials(store, &target.id, &target.secrets) {
        Ok(credentials) => credentials,
        Err(err) => {
            // 解密失败（换了 Windows 用户/机器，或钥匙串条目被删）属于"凭据损坏"。
            // err 链里包含 DPAPI/钥匙串的底层错误码，便于区分"文件坏了"与"跨用户/跨机器"。
            log::warn!("账号 {id} 凭据解密失败（DPAPI/钥匙串错误）：{err:#}");
            return RoundResult {
                id,
                outcome: RefreshOutcome::CredentialsInvalid(
                    "凭据无法解密，请重新登录该账号".to_owned(),
                ),
            };
        }
    };

    if target.provider == AccountProvider::DeepSeekPlatform {
        return refresh_deepseek(&id, &credentials.access_token, timeout);
    }

    let client = match HttpClient::new(&target.base_url, timeout) {
        Ok(client) => client,
        Err(err) => {
            return RoundResult {
                id,
                outcome: RefreshOutcome::Offline(err.user_message()),
            };
        }
    };

    let mut attempts = 0_u8;
    let mut rotated = false;

    loop {
        match client.me(&credentials.access_token) {
            Ok(status) => {
                // 订阅摘要失败不影响"在线"判定：余额/登录态才是主信息
                let summary = match client.subscription_summary(&credentials.access_token) {
                    Ok(summary) => Some(summary),
                    Err(err) => {
                        log::debug!("账号 {id} 订阅摘要获取失败：{}", err.detail());
                        None
                    }
                };
                return RoundResult {
                    id,
                    outcome: RefreshOutcome::Online {
                        status,
                        summary,
                        rotated: rotated.then(|| credentials.clone()),
                    },
                };
            }
            Err(ApiError::Unauthorized) => match next_recovery_action(attempts) {
                RecoverAction::Refresh => {
                    attempts += 1;
                    log::info!("账号 {id} token 失效，尝试续期");
                    match client.refresh(&credentials.refresh_token) {
                        Ok(tokens) => {
                            credentials.access_token = tokens.access_token;
                            if !tokens.refresh_token.is_empty() {
                                credentials.refresh_token = tokens.refresh_token;
                            }
                            rotated = true;
                        }
                        Err(err) => {
                            log::warn!("账号 {id} 续期失败：{}", err.detail());
                        }
                    }
                }
                RecoverAction::Relogin => {
                    attempts += 1;
                    log::info!("账号 {id} 续期失败，尝试用保存的密码重新登录");
                    match client.login(&target.email, &credentials.password) {
                        Ok(LoginOutcome::Tokens(tokens)) => {
                            credentials.access_token = tokens.access_token;
                            if !tokens.refresh_token.is_empty() {
                                credentials.refresh_token = tokens.refresh_token;
                            }
                            rotated = true;
                        }
                        Ok(LoginOutcome::NeedsTotp { .. }) => {
                            return RoundResult {
                                id,
                                outcome: RefreshOutcome::CredentialsInvalid(
                                    "该账号开启了 2FA，自动重登不可用，请手动重新登录".to_owned(),
                                ),
                            };
                        }
                        Err(err) => {
                            return RoundResult {
                                id,
                                outcome: RefreshOutcome::CredentialsInvalid(format!(
                                    "账号密码已失效，请重新登录（{}）",
                                    err.user_message()
                                )),
                            };
                        }
                    }
                }
                RecoverAction::MarkInvalid => {
                    return RoundResult {
                        id,
                        outcome: RefreshOutcome::CredentialsInvalid(
                            "登录状态与密码均无法恢复，请重新登录".to_owned(),
                        ),
                    };
                }
            },
            Err(err) => {
                // 网络/5xx/限流：保持账号记录，标记离线，下一轮重试。
                // 详细原因（含 ureq 原始错误）只进日志，界面统一用文案。
                log::warn!("账号 {id} 刷新失败：{}", err.detail());
                return RoundResult {
                    id,
                    outcome: RefreshOutcome::Offline(err.user_message()),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 多账号刷新线程数受并发上限约束() {
        assert_eq!(refresh_worker_count(1), 1);
        assert_eq!(refresh_worker_count(3), 3);
        assert_eq!(
            refresh_worker_count(10),
            MAX_CONCURRENT_ACCOUNT_REFRESHES,
            "账号再多也不能无限开线程"
        );
    }

    #[test]
    fn 恢复链顺序为续期再重登最后判死() {
        assert_eq!(next_recovery_action(0), RecoverAction::Refresh);
        assert_eq!(next_recovery_action(1), RecoverAction::Relogin);
        assert_eq!(next_recovery_action(2), RecoverAction::MarkInvalid);
        assert_eq!(next_recovery_action(9), RecoverAction::MarkInvalid);
    }

    #[test]
    fn 首轮刷新立即开始() {
        assert!(should_start_round(
            Instant::now(),
            None,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn 未到间隔不刷新() {
        let now = Instant::now();
        let last = now - Duration::from_secs(10);
        assert!(!should_start_round(
            now,
            Some(last),
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn 超过间隔刷新() {
        let now = Instant::now();
        let last = now - Duration::from_secs(400);
        assert!(should_start_round(
            now,
            Some(last),
            Duration::from_secs(300)
        ));
    }

    /// 端到端：假实例先对旧 token 返回 401，续期后接受新 token。
    #[test]
    #[ignore = "loopback 端到端用例：
需在验收时显式运行 cargo test -- --ignored（本机安全软件会偶发中止 loopback 连接）"]
    fn 续期成功后状态在线并回传新凭据() {
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

        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定端口");
        let addr = listener.local_addr().expect("取得端口");
        listener.set_nonblocking(true).expect("非阻塞监听");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_in_server = Arc::clone(&stop);

        let server = std::thread::spawn(move || {
            while !stop_in_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let request = read_request(&mut stream);
                        let lower = request.to_ascii_lowercase();

                        let (status_line, body) = if lower.contains("/api/v1/auth/refresh") {
                            (
                                "200 OK",
                                r#"{"code":0,"message":"success","data":{"access_token":"tok-new","refresh_token":"ref-new","expires_in":3600}}"#.to_owned(),
                            )
                        } else if lower.contains("/api/v1/subscriptions/summary") {
                            (
                                "200 OK",
                                r#"{"code":0,"message":"success","data":{"active_count":1,"total_used_usd":2.5,"subscriptions":[{"group_name":"默认分组"}]}}"#.to_owned(),
                            )
                        } else if lower.contains("authorization: bearer tok-new") {
                            (
                                "200 OK",
                                r#"{"code":0,"message":"success","data":{"email":"user@example.com","role":"admin","balance":5.0}}"#.to_owned(),
                            )
                        } else {
                            (
                                "401 Unauthorized",
                                r#"{"code":401,"message":"expired"}"#.to_owned(),
                            )
                        };

                        let response = format!(
                            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
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

        let store: Arc<dyn SecretStore> = Arc::from(crate::secrets::system_store());
        let account_id = "account-rotated";
        let credentials = Credentials {
            password: "password".to_owned(),
            access_token: "tok-old".to_owned(),
            refresh_token: "ref-old".to_owned(),
        };
        let sealed =
            secrets::seal_credentials(store.as_ref(), account_id, &credentials).expect("加密凭据");

        let target = RefreshTarget {
            id: account_id.to_owned(),
            base_url: format!("http://{addr}"),
            email: "user@example.com".to_owned(),
            provider: AccountProvider::Sub2Api,
            secrets: sealed,
        };

        // 环境噪声重试：仅对临时网络错误重试（见 main.rs 的 test_support 说明）
        let results = crate::test_support::retry_message(|| {
            let receiver = spawn_round(
                vec![target.clone()],
                Arc::clone(&store),
                Duration::from_secs(5),
            );
            let results = receiver
                .recv_timeout(Duration::from_secs(15))
                .map_err(|err| format!("等待刷新结果失败：{err}"))?;
            match &results[0].outcome {
                RefreshOutcome::Offline(message) if crate::test_support::is_transient(message) => {
                    Err(message.clone())
                }
                _ => Ok(results),
            }
        })
        .expect("等待刷新结果");

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().expect("假实例线程结束");

        assert_eq!(results.len(), 1);
        match &results[0].outcome {
            RefreshOutcome::Online {
                status,
                summary,
                rotated,
            } => {
                assert_eq!(status.balance, Some(5.0));
                assert_eq!(
                    summary.as_ref().map(|item| item.active_count),
                    Some(1),
                    "订阅摘要应当被解析"
                );
                let rotated = rotated.as_ref().expect("续期成功后应回传新凭据");
                assert_eq!(rotated.access_token, "tok-new");
                assert_eq!(rotated.refresh_token, "ref-new");
            }
            other => panic!("应当在线，实际：{other:?}"),
        }
    }

    #[test]
    fn 凭据无法解密时标记损坏() {
        let store: Arc<dyn SecretStore> = Arc::from(crate::secrets::system_store());
        let target = RefreshTarget {
            id: "broken".to_owned(),
            base_url: "http://127.0.0.1:9".to_owned(),
            email: "user@example.com".to_owned(),
            provider: AccountProvider::Sub2Api,
            secrets: "enc:dpapi:v1:not-a-real-cipher".to_owned(),
        };

        let receiver = spawn_round(vec![target], store, Duration::from_secs(2));
        let results = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("等待结果");

        match &results[0].outcome {
            RefreshOutcome::CredentialsInvalid(message) => {
                assert!(message.contains("重新登录"));
            }
            other => panic!("应当判定凭据损坏，实际：{other:?}"),
        }
    }

    /// 手工验收：对**真实**实例刷新状态并取回余额/订阅摘要。
    ///
    /// 需要数据目录里已经有账号（先用 `真实实例完成登录流程并可落库` 播种）：
    ///
    /// ```powershell
    /// $env:SUB2API_ACCEPTANCE_DATA_DIR = "target/portable-test"
    /// cargo test 真实实例刷新 -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "需要真实实例与已播种的账号，见函数文档"]
    fn 真实实例刷新状态并取回余额() {
        let dir = std::env::var("SUB2API_ACCEPTANCE_DATA_DIR")
            .expect("缺少环境变量 SUB2API_ACCEPTANCE_DATA_DIR（见用例文档）");
        let dir = std::path::PathBuf::from(dir);

        let (accounts, _) = crate::accounts::load_or_create(&dir).expect("加载账号文件");
        assert!(
            !accounts.accounts().is_empty(),
            "数据目录里还没有账号，请先跑登录验收用例"
        );

        let targets = accounts
            .accounts()
            .iter()
            .map(RefreshTarget::from_account)
            .collect::<Vec<_>>();
        let secret_store: Arc<dyn SecretStore> = Arc::from(crate::secrets::system_store());

        let receiver = spawn_round(targets, secret_store, Duration::from_secs(15));
        let results = receiver
            .recv_timeout(Duration::from_secs(60))
            .expect("等待真实实例刷新结果");

        assert_eq!(results.len(), accounts.accounts().len());
        for result in &results {
            match &result.outcome {
                RefreshOutcome::Online {
                    status,
                    summary,
                    rotated,
                } => {
                    assert!(status.balance.is_some(), "应当取回余额");
                    eprintln!(
                        "真实刷新通过：balance={:?} 订阅数={:?} 续期={}",
                        status.balance,
                        summary.as_ref().map(|item| item.active_count),
                        rotated.is_some()
                    );
                }
                RefreshOutcome::Offline(message) => {
                    panic!("真实实例刷新失败（离线）：{message}")
                }
                RefreshOutcome::CredentialsInvalid(message) => {
                    panic!("真实实例刷新失败（凭据损坏）：{message}")
                }
            }
        }
    }
}

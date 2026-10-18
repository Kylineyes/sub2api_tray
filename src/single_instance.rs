//! 单实例保护。
//!
//! 为什么需要：两个副本同时跑会各自持有内存里的 `accounts.json` 快照，
//! 谁后写盘谁覆盖对方——登录、删除、Key 缓存都可能互相吃掉。
//!
//! 实现方式：在固定的本机端口段上尝试 `bind`（只监听 127.0.0.1）。
//! 端口被占用就说明已有一个实例在运行；拿到监听句柄的实例把句柄一直持有到进程退出，
//! 句柄释放（进程结束）后端口自然可再次绑定。
//!
//! 为什么不用文件锁：`File::try_lock` 需要较新的 std API，且锁文件放在
//! 只读目录时会失败；端口方案在 Windows/macOS 行为一致，也不依赖文件权限。
//! 代价是占一个本机端口（只用回环，不对外），且极端情况下可能被其它程序抢先占用，
//! 因此按端口段依次尝试。

use std::net::TcpListener;

/// 单实例锁占用的本机端口（高位、非常用端口）。
///
/// 必须是**固定**端口：如果允许顺延到下一个端口，第二个实例就会"礼貌地"换一个端口
/// 绑上，单实例保护也就失效了（这正是第一版实现的 bug）。
const LOCK_PORT: u16 = 45917;

/// 单实例守卫：**必须活到进程结束**（`Drop` 时释放端口）。
pub struct InstanceGuard {
    /// 监听句柄，仅用于占位；不 accept 任何连接。
    _listener: TcpListener,
}

/// 申请单实例锁。
///
/// 成功返回守卫（调用方要把它保存在 `main` 的栈上）；已有实例在运行时返回 `Err`，
/// 错误信息可直接展示给用户。
pub fn acquire() -> anyhow::Result<InstanceGuard> {
    match TcpListener::bind(("127.0.0.1", LOCK_PORT)) {
        Ok(listener) => {
            log::debug!("单实例锁已获取：127.0.0.1:{LOCK_PORT}");
            Ok(InstanceGuard {
                _listener: listener,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => Err(anyhow::anyhow!(
            "已有一个 Sub2API 托盘实例在运行（本机端口 {LOCK_PORT} 已被占用）"
        )),
        Err(err) => Err(anyhow::anyhow!(
            "无法建立单实例锁（127.0.0.1:{LOCK_PORT}）：{err}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 第二个实例申请会失败() {
        let first = acquire().expect("第一个实例应当成功");
        let second = acquire();
        assert!(second.is_err(), "第二个实例应当被拒绝");
        drop(first);
        // 释放后应当可以再次获取
        assert!(acquire().is_ok(), "释放后应当能重新获取");
    }
}

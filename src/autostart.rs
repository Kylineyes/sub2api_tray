//! 开机自启（登录时自动运行）。
//!
//! 平台差异：
//!
//! * **Windows**：写当前用户的 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
//!   注册表项。用 `reg.exe` 而不是 windows-sys 的注册表 API —— 只为两行读写引入
//!   `Win32_System_Registry` 特性收益不大，而 `reg` 在所有 Windows 上都存在。
//!   选 HKCU 而不是 HKLM：不需要管理员权限。
//! * **macOS**：写 `~/Library/LaunchAgents/com.sub2api.tray.plist`（RunAtLoad）。
//!   只写文件、不强制 `launchctl load`：下一次登录就会生效，避免在用户没预期时
//!   立刻拉起第二个进程。
//!
//! 未实机验证：macOS 分支（与项目其它 macOS 代码一致，需要在 mac 上人工验收）。

use anyhow::Context as _;

/// 注册表值名 / LaunchAgent 标识。
const ENTRY_NAME: &str = "sub2api_tray";

/// Windows 的 Run 注册表键。
#[cfg(target_os = "windows")]
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

/// 查询当前是否已开启开机自启。
///
/// 实现方式就是"查一下系统里有没有这条记录"，因此不会出现"界面显示已开启、
/// 实际没生效"的不一致。
pub fn is_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("reg")
            .args(["query", RUN_KEY, "/v", ENTRY_NAME])
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    {
        plist_path().map(|path| path.exists()).unwrap_or(false)
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        false
    }
}

/// 开启/关闭开机自启。
///
/// `executable` 由调用方传入（`std::env::current_exe()`），这样便携版被移动位置后
/// 重新保存一次即可指向新路径。
pub fn set_enabled(enabled: bool, executable: &std::path::Path) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    {
        // 路径带空格时必须再加一层引号，注册表里的命令行才能被正确拆分
        let command_line = format!("\"{}\" --hidden", executable.display());
        let status = if enabled {
            std::process::Command::new("reg")
                .args([
                    "add",
                    RUN_KEY,
                    "/v",
                    ENTRY_NAME,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &command_line,
                    "/f",
                ])
                .status()
                .context("调用 reg add 失败")?
        } else {
            std::process::Command::new("reg")
                .args(["delete", RUN_KEY, "/v", ENTRY_NAME, "/f"])
                .status()
                .context("调用 reg delete 失败")?
        };

        // 关闭时"本来就删不掉"（值不存在）也算成功
        if enabled && !status.success() {
            anyhow::bail!("写入注册表失败（reg 退出码 {:?}）", status.code());
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        let path = plist_path().context("无法定位 ~/Library/LaunchAgents")?;
        if enabled {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).context("创建 LaunchAgents 目录失败")?;
            }
            std::fs::write(&path, plist_body(executable)).context("写入 LaunchAgent 失败")?;
        } else if path.exists() {
            std::fs::remove_file(&path).context("删除 LaunchAgent 失败")?;
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        let _ = (enabled, executable);
        anyhow::bail!("当前平台不支持开机自启")
    }
}

/// `~/Library/LaunchAgents/com.sub2api.tray.plist`
#[cfg(target_os = "macos")]
fn plist_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        std::path::PathBuf::from(home)
            .join("Library/LaunchAgents")
            .join("com.sub2api.tray.plist"),
    )
}

/// LaunchAgent 的 plist 内容。
#[cfg(target_os = "macos")]
fn plist_body(executable: &std::path::Path) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.sub2api.tray</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>--hidden</string>
    </array>
    <key>RunAtLoad</key><true/>
    <key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        executable.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 查询状态不会崩溃() {
        // 只验证能安全调用（真实状态取决于本机环境，不做断言）
        let _ = is_enabled();
    }
}

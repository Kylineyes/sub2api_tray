//! 版本更新检查（只"发现"，不自动下载安装）。
//!
//! 数据源：GitHub Releases 的 `latest` 接口（公开仓库，不需要 token）。
//! 只做两件事：判断远端 tag 是否比当前版本新、把下载页链接交给界面。
//! 不替用户下载或执行安装包——静默替换二进制风险太大（签名、权限、正在运行的 exe 被占用）。

use std::time::Duration;

/// 仓库标识（owner/repo），用于拼 GitHub API。
pub const REPO: &str = "Kylineyes/sub2api_tray";

/// 检查结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateInfo {
    /// 远端版本号（已去掉前缀 `v`）。
    pub version: String,
    /// 发布页链接。
    pub url: String,
    /// 发布时间（原样字符串）。
    pub published_at: Option<String>,
}

/// 把版本号切成可比较的数字段：`v0.1.10` → `[0, 1, 10]`。
///
/// 非数字段（例如 `-beta.1`）直接忽略；段数不同时按缺省 0 补齐，所以
/// `0.2` > `0.1.9` 成立。
fn version_parts(version: &str) -> Vec<u64> {
    // 只比较主版本段：`1.0.0-beta.1` 与 `1.0.0` 视为同一版本，
    // 避免用户被"预发布比正式版新"这种误判打扰。
    version
        .trim()
        .trim_start_matches(['v', 'V'])
        .split(['-', '+'])
        .next()
        .unwrap_or_default()
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

/// 判断远端版本是否比当前版本新。
pub fn is_newer(current: &str, latest: &str) -> bool {
    let current = version_parts(current);
    let latest = version_parts(latest);
    let length = current.len().max(latest.len());
    for index in 0..length {
        let left = current.get(index).copied().unwrap_or(0);
        let right = latest.get(index).copied().unwrap_or(0);
        if right != left {
            return right > left;
        }
    }
    false
}

/// 查询 GitHub 上的最新 Release。
///
/// 返回 `Ok(None)` 表示"没有发布记录"（仓库还没打 tag），不是错误。
pub fn check_latest(timeout: Duration) -> Result<Option<UpdateInfo>, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();

    let mut response = agent
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "sub2api_tray")
        .call()
        .map_err(|err| format!("请求 GitHub 失败：{err}"))?;

    let status = response.status().as_u16();
    if status == 404 {
        return Ok(None);
    }
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|err| format!("读取响应失败：{err}"))?;
    if status != 200 {
        return Err(format!("GitHub 返回 HTTP {status}"));
    }

    let value: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("解析响应失败：{err}"))?;
    let tag = value
        .get("tag_name")
        .and_then(|item| item.as_str())
        .unwrap_or_default();
    if tag.is_empty() {
        return Ok(None);
    }

    Ok(Some(UpdateInfo {
        version: tag.trim_start_matches(['v', 'V']).to_owned(),
        url: value
            .get("html_url")
            .and_then(|item| item.as_str())
            .unwrap_or("https://github.com/Kylineyes/sub2api_tray/releases")
            .to_owned(),
        published_at: value
            .get("published_at")
            .and_then(|item| item.as_str())
            .map(ToOwned::to_owned),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 版本比较() {
        assert!(is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.1.0", "v0.1.1"));
        assert!(is_newer("0.1.9", "0.1.10"), "按数字比较而不是字符串");
        assert!(is_newer("0.1", "0.1.1"), "缺省段补 0");
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.3.0", "0.2.9"));
        assert!(!is_newer("1.0.0", "1.0.0-beta.1"));
    }

    #[test]
    fn 版本段解析() {
        assert_eq!(version_parts("v1.2.3"), vec![1, 2, 3]);
        assert_eq!(version_parts("0.1.0-beta.1"), vec![0, 1, 0]);
    }
}

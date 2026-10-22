//! DeepSeek 官方平台适配。
//!
//! 官方控制台是 `platform.deepseek.com`，管理 API Key；真正的数据接口在
//! `api.deepseek.com`。托盘只使用用户填写的 API Key，不保存邮箱和密码。

use crate::api::ApiError;

/// 官方控制台地址，仅用于账号展示。
pub const PLATFORM_URL: &str = "https://platform.deepseek.com";

/// 官方 API 基础地址。
pub const API_BASE: &str = "https://api.deepseek.com";

/// 余额接口路径。
const BALANCE_PATH: &str = "/user/balance";

/// 一次余额查询的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct DeepSeekBalance {
    /// 账户当前是否可用。
    pub is_available: bool,
    /// 余额币种，例如 CNY / USD。
    pub currency: String,
    /// 总余额。
    pub total_balance: f64,
}

/// 返回 API Key 的安全展示文本。
pub fn mask_api_key(api_key: &str) -> String {
    let trimmed = api_key.trim();
    if trimmed.len() <= 8 {
        "sk-...".to_owned()
    } else {
        format!("sk-...{}", &trimmed[trimmed.len() - 4..])
    }
}

/// 解析 DeepSeek 余额响应。
pub fn parse_balance_response(status: u16, body: &str) -> Result<DeepSeekBalance, ApiError> {
    match status {
        401 | 403 => return Err(ApiError::Unauthorized),
        429 => return Err(ApiError::RateLimited),
        200 => {}
        other => return Err(ApiError::Server(format!("HTTP {other}"))),
    }

    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|_| ApiError::Server("DeepSeek 余额响应不是合法 JSON".to_owned()))?;
    let is_available = value
        .get("is_available")
        .and_then(|item| item.as_bool())
        .unwrap_or(false);
    let infos = value
        .get("balance_infos")
        .and_then(|item| item.as_array())
        .ok_or_else(|| ApiError::Server("DeepSeek 余额响应缺少 balance_infos".to_owned()))?;

    let selected = infos
        .iter()
        .find(|item| item.get("currency").and_then(|v| v.as_str()) == Some("CNY"))
        .or_else(|| infos.first())
        .ok_or_else(|| ApiError::Server("DeepSeek 余额列表为空".to_owned()))?;

    let currency = selected
        .get("currency")
        .and_then(|item| item.as_str())
        .unwrap_or("CNY")
        .to_owned();
    let total_balance = selected
        .get("total_balance")
        .and_then(|item| item.as_str())
        .and_then(|text| text.parse::<f64>().ok())
        .ok_or_else(|| ApiError::Server("DeepSeek 余额字段格式错误".to_owned()))?;

    Ok(DeepSeekBalance {
        is_available,
        currency,
        total_balance,
    })
}

/// 用 API Key 查询余额；同时作为登录时的凭据校验。
pub fn fetch_balance(
    api_key: &str,
    timeout: std::time::Duration,
) -> Result<DeepSeekBalance, ApiError> {
    let key = api_key.trim();
    if key.is_empty() {
        return Err(ApiError::InvalidInput(
            "DeepSeek API Key 不能为空".to_owned(),
        ));
    }
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build();
    let agent = config.new_agent();
    let url = format!("{API_BASE}{BALANCE_PATH}");
    let mut response = agent
        .get(&url)
        .header("accept", "application/json")
        .header("authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|err| ApiError::Network(err.to_string()))?;
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(|err| ApiError::Network(err.to_string()))?;
    parse_balance_response(status, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析余额并按人民币优先() {
        let body = r#"{
            "is_available": true,
            "balance_infos": [
                {"currency":"USD","total_balance":"1.00"},
                {"currency":"CNY","total_balance":"110.00"}
            ]
        }"#;
        let got = parse_balance_response(200, body).unwrap();
        assert!(got.is_available);
        assert_eq!(got.currency, "CNY");
        assert_eq!(got.total_balance, 110.0);
    }

    #[test]
    fn 密钥无效时映射为未授权() {
        assert_eq!(
            parse_balance_response(401, r#"{"error":{"message":"invalid key"}}"#).unwrap_err(),
            ApiError::Unauthorized
        );
    }

    #[test]
    fn 密钥展示只保留尾部四位() {
        assert_eq!(mask_api_key("sk-1234567890abcd"), "sk-...abcd");
        assert_eq!(mask_api_key("short"), "sk-...");
    }
}

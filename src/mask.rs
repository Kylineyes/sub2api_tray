//! 账号展示打码。
//!
//! 只影响**展示**（托盘菜单标题、面板文字），不参与存储与请求：
//! `accounts.json` 里始终是完整邮箱，`/auth/login` 也始终用完整邮箱提交。
//!
//! 打码开关来自 `app.json` 的 `maskAccounts`（默认关闭），由调用方决定是否调用
//! [`mask_if`]。之所以把"是否打码"交给调用方而不是在这里读配置，是为了保持本模块
//! 纯函数、可测试，也方便将来在托盘菜单与面板里使用不同的展示策略。

/// 打码后的占位符：输入不合法（空、缺少 `@`、含空白）时统一返回它。
const PLACEHOLDER: &str = "***";

/// 打码时保留的字符数：本地部分与域名第一段各保留前 2 个字符。
const KEEP_CHARS: usize = 2;

/// 按 `maskAccounts` 开关决定是否打码。
///
/// * `enabled == false`：原样返回，便于在关闭打码时零成本展示完整邮箱；
/// * `enabled == true`：调用 [`mask_email`]。
pub fn mask_if(enabled: bool, email: &str) -> String {
    if enabled {
        mask_email(email)
    } else {
        email.to_owned()
    }
}

/// 把邮箱打码成"保留首尾、遮住中间"的形式。
///
/// 规则（与 `docs/plans/0002-data-layer.md` 第 4.4 节一一对应）：
///
/// | 输入 | 输出 |
/// | --- | --- |
/// | `a@b.cn` | `a***@b***.cn` |
/// | `ab@cd.com` | `ab***@cd***.com` |
/// | `zhangwei@example.com` | `zh***@ex***.com` |
/// | `user@sub.example.com` | `us***@su***.example.com` |
/// | 空 / 非法 | `***` |
///
/// 具体做法：
///
/// 1. 用第一个 `@` 切成"本地部分"和"域名"，任一为空或含空白即视为非法；
/// 2. 本地部分保留前两个字符（不足两个就保留一个），后面接 `***`；
/// 3. 域名只打码**第一段**：保留前两个字符后接 `***`，其余层级（如 `.example.com`）原样保留，
///    这样既能看出是哪个域名，又不会暴露具体用户名。
///
/// 注意：返回值只用于界面展示，**不要**把它写回 `accounts.json`。
pub fn mask_email(email: &str) -> String {
    let email = email.trim();
    let Some((local, domain)) = email.split_once('@') else {
        return PLACEHOLDER.to_owned();
    };

    // 去掉奇怪的输入：空段、空格、制表符等都会让展示失去意义
    if local.is_empty() || domain.is_empty() || email.chars().any(char::is_whitespace) {
        return PLACEHOLDER.to_owned();
    }

    // 本地部分：保留首部，遮掉中间
    let local_visible = take_chars(local, KEEP_CHARS);

    // 域名：只处理第一段，其余层级原样展示
    let (first_label, rest) = match domain.split_once('.') {
        Some((label, rest)) => (label, format!(".{rest}")),
        // 没有点号（例如 `localhost`）时整体当作第一段，不再追加后缀
        None => (domain, String::new()),
    };
    let domain_visible = take_chars(first_label, KEEP_CHARS);

    format!("{local_visible}***@{domain_visible}***{rest}")
}

/// 取字符串前 `max` 个字符；不足时取全部。
///
/// 用 `chars()` 而不是字节切片，避免在多字节字符（中文、emoji 邮箱）上 panic。
fn take_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 短邮箱保留首字符() {
        assert_eq!(mask_email("a@b.cn"), "a***@b***.cn");
    }

    #[test]
    fn 两个字符的段完整保留() {
        assert_eq!(mask_email("ab@cd.com"), "ab***@cd***.com");
    }

    #[test]
    fn 常见邮箱只露首两位() {
        assert_eq!(mask_email("zhangwei@example.com"), "zh***@ex***.com");
    }

    #[test]
    fn 多级域名只遮第一段() {
        assert_eq!(
            mask_email("user@sub.example.com"),
            "us***@su***.example.com"
        );
    }

    #[test]
    fn 非法输入返回占位符() {
        assert_eq!(mask_email(""), PLACEHOLDER);
        assert_eq!(mask_email("not-an-email"), PLACEHOLDER);
        assert_eq!(mask_email("@example.com"), PLACEHOLDER);
        assert_eq!(mask_email("user@"), PLACEHOLDER);
        assert_eq!(mask_email("u ser@example.com"), PLACEHOLDER);
    }

    #[test]
    fn 关闭开关时原样返回() {
        assert_eq!(
            mask_if(false, "zhangwei@example.com"),
            "zhangwei@example.com"
        );
        assert_eq!(mask_if(true, "zhangwei@example.com"), "zh***@ex***.com");
    }

    #[test]
    fn 多字节字符不会按字节截断() {
        // 中文邮箱虽然少见，但输入来自用户，必须保证不 panic、不切出半个字符
        let masked = mask_email("张三@例子.公司");
        assert_eq!(masked, "张三***@例子***.公司");
    }
}

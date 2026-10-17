//! 凭据加密存储。
//!
// 0002 交付的是"凭据存储 API"，真正的调用方（0003 登录、0004 删除账号）还没接入，
// 因此这里整体放开 dead_code；接入后应删除本行。
#![allow(dead_code, reason = "凭据存储 API 由 0003/0004 消费，尚未接入")]
//!
//! `accounts.json` 里的 `secrets` 字段是一个**不透明字符串**，由本模块负责生成与解析。
//! 之所以要把加密单独拆一层：
//!
//! 1. 数据层（[`crate::accounts`]）不需要知道任何系统 API，测试可以用假数据；
//! 2. 两个平台的存储形态不同，但调用方只看到 `seal / open / remove`；
//! 3. 将来若要更换算法（例如统一换成"主密钥 + AES-GCM"），只需要改本模块。
//!
//! # 平台实现
//!
//! | 平台 | 落盘内容 | 解密条件 |
//! | --- | --- | --- |
//! | Windows | `enc:dpapi:v1:<base64(DPAPI 密文)>` | 同一台机器 + 同一个 Windows 用户 |
//! | macOS | `enc:keychain:v1:<account-id>`（密文在钥匙串里） | 同一个 macOS 用户（首次访问会弹钥匙串授权） |
//! | 其它 | 不支持，调用即报错 | — |
//!
//! 解密失败一律返回 `Err`，由上层标记"凭据损坏，需要重新登录"，
//! **不会**降级成明文，也不会静默清空账号。
//!
//! 安全约定（与 `AGENTS.md` 第十一节一致）：
//! * 明文只在本模块的入参与返回值里短暂出现，绝不写日志；
//! * Windows 侧使用固定的附加熵，把密文绑定到"本产品 + 本用途"。

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Windows DPAPI 密文的前缀。
pub const PREFIX_DPAPI: &str = "enc:dpapi:v1:";

/// macOS 钥匙串引用的前缀。
pub const PREFIX_KEYCHAIN: &str = "enc:keychain:v1:";

/// 钥匙串里的 service 名称（macOS）。
pub const KEYCHAIN_SERVICE: &str = "sub2api_tray";

/// 一个账号在内存中的明文凭据。
///
/// 不要实现 `Debug` 打印真实字段：这里手工实现 `Debug`，只输出字段长度，
/// 避免任何 `dbg!` / 日志把人带进坑里。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// 登录密码：只在自动重登时使用。
    pub password: String,
    /// 当前 access token。
    pub access_token: String,
    /// refresh token：用于无感续期。
    pub refresh_token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 只暴露长度，不暴露内容
        f.debug_struct("Credentials")
            .field("password_len", &self.password.len())
            .field("access_token_len", &self.access_token.len())
            .field("refresh_token_len", &self.refresh_token.len())
            .finish()
    }
}

impl Credentials {
    /// 序列化成 JSON 字符串，作为加密的明文输入。
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).context("序列化凭据失败")
    }

    /// 从解密后的 JSON 字符串还原。
    pub fn from_json(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("解析凭据失败（密文可能被破坏或来自旧版本）")
    }
}

/// 凭据存储抽象。
///
/// `Send + Sync` 是必须的：后续刷新线程会读取凭据。
pub trait SecretStore: Send + Sync {
    /// 加密（或存入钥匙串），返回要写进 `accounts.json` 的不透明字符串。
    fn seal(&self, account_id: &str, plaintext: &str) -> Result<String>;

    /// 解密（或从钥匙串读取）。
    fn open(&self, account_id: &str, sealed: &str) -> Result<String>;

    /// 删除底层残留（macOS 钥匙串条目；DPAPI 无外部存储，实现为空操作）。
    ///
    /// 删除账号时必须调用，否则钥匙串里会留下孤儿条目。
    fn remove(&self, account_id: &str) -> Result<()>;
}

/// 便捷封装：加密整份凭据。
pub fn seal_credentials(
    store: &dyn SecretStore,
    account_id: &str,
    credentials: &Credentials,
) -> Result<String> {
    store.seal(account_id, &credentials.to_json()?)
}

/// 便捷封装：解密整份凭据。
pub fn open_credentials(
    store: &dyn SecretStore,
    account_id: &str,
    sealed: &str,
) -> Result<Credentials> {
    Credentials::from_json(&store.open(account_id, sealed)?)
}

/// 获取当前平台的凭据存储实现。
pub fn system_store() -> Box<dyn SecretStore> {
    Box::new(platform::SystemSecretStore::new())
}

/// 判断一个 `secrets` 字符串是否属于本模块支持的格式（用于"凭据损坏"判定）。
pub fn is_supported_secret(sealed: &str) -> bool {
    sealed.starts_with(PREFIX_DPAPI) || sealed.starts_with(PREFIX_KEYCHAIN)
}

#[cfg(windows)]
mod platform {
    //! Windows 实现：DPAPI（`CryptProtectData` / `CryptUnprotectData`）。
    //!
    //! 使用"当前用户"作用域（不传 `CRYPTPROTECT_LOCAL_MACHINE`）：
    //! 换一个 Windows 用户或换一台机器都解不开，符合"凭据不出本机"的预期。

    use anyhow::{Context as _, Result, bail};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use windows_sys::Win32::Foundation::{GetLastError, LocalFree};
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    use super::{PREFIX_DPAPI, SecretStore};

    /// 附加熵：把密文绑定到"本产品 + 凭据用途"。
    ///
    /// 有了它，即使别的程序调用 DPAPI 也解不开这份密文；修改本值等于让历史密文全部失效，
    /// 因此除非做版本迁移，不要动它。
    const ENTROPY: &[u8] = b"sub2api_tray/credentials/v1";

    /// Windows 的凭据存储：无状态，直接用 DPAPI。
    pub struct SystemSecretStore;

    impl SystemSecretStore {
        /// 创建一个存储实例。构造函数不会失败，也没有需要初始化的资源。
        pub fn new() -> Self {
            Self
        }
    }

    /// 把可变字节切片包装成 DPAPI 需要的 BLOB。
    fn blob(data: &mut [u8]) -> Result<CRYPT_INTEGER_BLOB> {
        let len = u32::try_from(data.len()).context("凭据过长，超过 DPAPI 的 4GB 上限")?;
        Ok(CRYPT_INTEGER_BLOB {
            cbData: len,
            pbData: data.as_mut_ptr(),
        })
    }

    impl SecretStore for SystemSecretStore {
        fn seal(&self, _account_id: &str, plaintext: &str) -> Result<String> {
            // DPAPI 会原地读取输入，这里用独立缓冲，避免对 &str 做 unsafe 写操作
            let mut plain = plaintext.as_bytes().to_vec();
            let mut entropy = ENTROPY.to_vec();
            let input = blob(&mut plain)?;
            let entropy_blob = blob(&mut entropy)?;
            let mut output = CRYPT_INTEGER_BLOB::default();

            let ok = unsafe {
                CryptProtectData(
                    &input,
                    std::ptr::null(),
                    &entropy_blob,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            };
            if ok == 0 {
                let code = unsafe { GetLastError() };
                bail!("DPAPI 加密失败（Windows 错误码 {code}）");
            }

            // 复制出密文后必须立刻 LocalFree：DPAPI 分配的内存归调用方管理
            let cipher = unsafe {
                std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec()
            };
            unsafe { LocalFree(output.pbData.cast()) };

            Ok(format!("{PREFIX_DPAPI}{}", STANDARD.encode(cipher)))
        }

        fn open(&self, _account_id: &str, sealed: &str) -> Result<String> {
            let Some(encoded) = sealed.strip_prefix(PREFIX_DPAPI) else {
                bail!("凭据格式不受当前平台支持（期望 {PREFIX_DPAPI} 前缀）");
            };
            let mut cipher = STANDARD
                .decode(encoded)
                .context("凭据密文不是合法的 base64")?;
            let mut entropy = ENTROPY.to_vec();
            let input = blob(&mut cipher)?;
            let entropy_blob = blob(&mut entropy)?;
            let mut output = CRYPT_INTEGER_BLOB::default();
            // 描述字符串由 DPAPI 分配，即使成功也要释放
            let mut description: windows_sys::core::PWSTR = std::ptr::null_mut();

            let ok = unsafe {
                CryptUnprotectData(
                    &input,
                    &mut description,
                    &entropy_blob,
                    std::ptr::null(),
                    std::ptr::null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut output,
                )
            };
            if !description.is_null() {
                unsafe { LocalFree(description.cast()) };
            }
            if ok == 0 {
                let code = unsafe { GetLastError() };
                bail!("DPAPI 解密失败（凭据可能来自其它 Windows 用户或其它机器，错误码 {code}）");
            }

            let plain =
                unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }
                    .to_vec();
            unsafe { LocalFree(output.pbData.cast()) };

            String::from_utf8(plain).context("凭据解密结果不是合法的 UTF-8")
        }

        fn remove(&self, _account_id: &str) -> Result<()> {
            // DPAPI 没有外部存储：密文就在 accounts.json 里，删记录即删除密文
            Ok(())
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    //! macOS 实现：系统钥匙串（Keychain）。
    //!
    //! 文件里只保存引用（service + account），真正的密文由钥匙串保管。
    //! 注意：未签名的 app 在首次读写时会弹出"允许访问钥匙串"的系统授权框，
    //! 这属于预期行为；本分支**尚未在 macOS 实机验收**（见 AGENTS.md 第十节）。

    use anyhow::{Context as _, Result, bail};
    use security_framework::passwords::{
        delete_generic_password, get_generic_password, set_generic_password,
    };

    use super::{KEYCHAIN_SERVICE, PREFIX_KEYCHAIN, SecretStore};

    /// macOS 的凭据存储：把整份凭据写进钥匙串的通用密码条目。
    pub struct SystemSecretStore;

    impl SystemSecretStore {
        /// 创建一个存储实例。
        pub fn new() -> Self {
            Self
        }
    }

    impl SecretStore for SystemSecretStore {
        fn seal(&self, account_id: &str, plaintext: &str) -> Result<String> {
            set_generic_password(KEYCHAIN_SERVICE, account_id, plaintext.as_bytes())
                .with_context(|| format!("写入 macOS 钥匙串失败（账号 {account_id}）"))?;
            Ok(format!("{PREFIX_KEYCHAIN}{account_id}"))
        }

        fn open(&self, account_id: &str, sealed: &str) -> Result<String> {
            let Some(reference) = sealed.strip_prefix(PREFIX_KEYCHAIN) else {
                bail!("凭据格式不受当前平台支持（期望 {PREFIX_KEYCHAIN} 前缀）");
            };
            if reference != account_id {
                // 防止手工把 A 的引用抄到 B 上，导致读到别人的凭据
                bail!("凭据引用与账号不匹配（引用 {reference}，账号 {account_id}）");
            }
            let bytes = get_generic_password(KEYCHAIN_SERVICE, account_id)
                .with_context(|| format!("读取 macOS 钥匙串失败（账号 {account_id}）"))?;
            String::from_utf8(bytes).context("钥匙串中的凭据不是合法的 UTF-8")
        }

        fn remove(&self, account_id: &str) -> Result<()> {
            match delete_generic_password(KEYCHAIN_SERVICE, account_id) {
                Ok(()) => Ok(()),
                Err(err) => {
                    // 条目本来就不存在时不当作错误：删除账号应当是幂等的
                    log::debug!("删除钥匙串条目失败（可能本就不存在）：{err}");
                    Ok(())
                }
            }
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    //! 其它平台：明确不支持，所有操作直接报错。
    //!
    //! 保留这个实现是为了让 `cargo check` 在 Linux 上也能过（例如 CI），
    //! 而不是让整个 crate 直接编译失败。

    use anyhow::{Result, bail};

    use super::SecretStore;

    /// 不支持的平台上的占位实现。
    pub struct SystemSecretStore;

    impl SystemSecretStore {
        /// 创建一个存储实例。
        pub fn new() -> Self {
            Self
        }
    }

    impl SecretStore for SystemSecretStore {
        fn seal(&self, _account_id: &str, _plaintext: &str) -> Result<String> {
            bail!("当前平台没有可用的系统凭据存储（仅支持 Windows DPAPI 与 macOS 钥匙串）")
        }

        fn open(&self, _account_id: &str, _sealed: &str) -> Result<String> {
            bail!("当前平台没有可用的系统凭据存储（仅支持 Windows DPAPI 与 macOS 钥匙串）")
        }

        fn remove(&self, _account_id: &str) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            password: "p@ssw0rd-测试".to_owned(),
            access_token: "access-token-value".to_owned(),
            refresh_token: "refresh-token-value".to_owned(),
        }
    }

    #[test]
    fn 凭据序列化往返() {
        let original = credentials();
        let restored = Credentials::from_json(&original.to_json().unwrap()).unwrap();
        assert_eq!(original, restored);
    }

    #[test]
    fn 凭据的debug不泄露内容() {
        let printed = format!("{:?}", credentials());
        assert!(
            !printed.contains("p@ssw0rd"),
            "调试输出不应包含明文：{printed}"
        );
        assert!(!printed.contains("access-token-value"));
        assert!(printed.contains("password_len"));
    }

    #[test]
    fn 前缀识别() {
        assert!(is_supported_secret("enc:dpapi:v1:abc"));
        assert!(is_supported_secret("enc:keychain:v1:uuid"));
        assert!(!is_supported_secret("plain-text"));
        assert!(!is_supported_secret(""));
    }

    #[cfg(windows)]
    #[test]
    fn dpapi加密解密往返() {
        let store = crate::secrets::system_store();
        let sealed = seal_credentials(store.as_ref(), "account-1", &credentials()).unwrap();

        assert!(sealed.starts_with(PREFIX_DPAPI), "实际：{sealed}");
        assert!(!sealed.contains("p@ssw0rd"), "密文里不应出现明文");

        let restored = open_credentials(store.as_ref(), "account-1", &sealed).unwrap();
        assert_eq!(restored, credentials());
    }

    #[cfg(windows)]
    #[test]
    fn 被篡改的密文解密失败() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};

        let store = crate::secrets::system_store();
        let sealed = seal_credentials(store.as_ref(), "account-1", &credentials()).unwrap();

        // 必须改"解码后的字节"再重新编码：直接改 base64 末尾字符可能只落在填充位上，
        // 解码结果完全不变，测试就会变成假阳性。
        let encoded = sealed.strip_prefix(PREFIX_DPAPI).unwrap();
        let mut cipher = STANDARD.decode(encoded).unwrap();
        let middle = cipher.len() / 2;
        cipher[middle] ^= 0xFF;
        let tampered = format!("{PREFIX_DPAPI}{}", STANDARD.encode(&cipher));

        assert!(open_credentials(store.as_ref(), "account-1", &tampered).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn 平台前缀不匹配时报错() {
        let store = crate::secrets::system_store();
        let err = open_credentials(store.as_ref(), "account-1", "enc:keychain:v1:whatever")
            .unwrap_err()
            .to_string();
        assert!(err.contains("不受当前平台支持"), "实际错误：{err}");
    }
}

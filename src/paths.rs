//! 数据目录解析、可写探测与原子写入。
//!
//! # 数据目录
//!
//! 需求方明确要求 `app.json` 与 `accounts.json` 放在"本工程产出的根目录"，
//! 也就是**可执行文件所在目录**（便携式布局：程序拷到哪，数据就在哪）。
//!
//! 这带来一个已知的失败模式：程序若被放到 `Program Files` 等只读目录，
//! 普通用户没有写权限。此时**不做静默回退**（已与需求方确认），
//! 而是由 [`ensure_writable`] 返回明确错误，让上层把"数据目录不可写"直接展示给用户。
//!
//! # 原子写入
//!
//! 所有写文件都走 [`write_atomic`]：先写同目录下的临时文件，`fsync` 后再
//! `rename` 覆盖目标。这样即使写入过程中断电/崩溃，目标文件要么是旧内容、
//! 要么是完整的新内容，不会出现"写了一半的 JSON"。

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};

/// `app.json`：环境参数与默认设置。
pub const APP_CONFIG_FILE_NAME: &str = "app.json";

/// `accounts.json`：账号与（将来的）API Key 的唯一持久化文件。
pub const ACCOUNTS_FILE_NAME: &str = "accounts.json";

/// 解析数据目录：当前可执行文件所在目录。
///
/// 开发期用 `cargo run` 时，这个目录是 `target/debug`，属于预期行为；
/// 发布版就是 exe 所在目录。
pub fn data_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("无法获取当前可执行文件的路径")?;
    let dir = exe
        .parent()
        .with_context(|| format!("可执行文件路径没有父目录：{}", exe.display()))?;
    Ok(dir.to_path_buf())
}

/// 探测目录是否可写，顺便验证"能创建、能删除"。
///
/// 只做一次 `stat` 判断是不够的：Windows 上目录的 ACL 可能在"创建文件"这一步才失败，
/// 所以这里真的写一个探测文件再删掉。探测文件名带进程 id，避免并发时互相踩。
pub fn ensure_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".sub2api-tray-write-probe-{}", std::process::id()));
    let mut file =
        File::create(&probe).with_context(|| format!("数据目录不可写：{}", dir.display()))?;
    file.write_all(b"probe")
        .with_context(|| format!("数据目录不可写：{}", dir.display()))?;
    // 探测文件必须清理掉；删除失败不影响"可写"这个结论，只记录 debug
    if let Err(err) = fs::remove_file(&probe) {
        log::debug!("删除写权限探测文件失败：{err}");
    }
    Ok(())
}

/// 原子地写入文本文件。
///
/// 流程：写 `.<文件名>.tmp-<pid>` → `sync_all` → `rename` 覆盖目标。
/// `rename` 在同一目录内是原子操作（Windows 上对应 `MoveFileEx` + 替换语义）。
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("目标路径没有父目录：{}", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("目标路径没有合法文件名：{}", path.display()))?;

    let temp_path = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let mut temp_file = File::create(&temp_path)
        .with_context(|| format!("创建临时文件失败：{}", temp_path.display()))?;
    temp_file
        .write_all(contents.as_bytes())
        .with_context(|| format!("写入临时文件失败：{}", temp_path.display()))?;
    // 落盘后再改名，尽量保证崩溃后不会留下空文件
    temp_file
        .sync_all()
        .with_context(|| format!("刷新临时文件到磁盘失败：{}", temp_path.display()))?;
    drop(temp_file);

    fs::rename(&temp_path, path).with_context(|| {
        format!(
            "重命名临时文件失败：{} -> {}",
            temp_path.display(),
            path.display()
        )
    })?;
    Ok(())
}

/// 把损坏的文件改名备份，返回备份后的路径。
///
/// 命名规则：`<原名>.corrupt-<unix 秒>`。之所以用时间戳而不是直接删除，
/// 是为了让用户还能手工抢救里面的账号信息（例如 JSON 少了一个逗号）。
pub fn quarantine_corrupt_file(path: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // 不能用 `with_extension`：它会把 `app.json` 的 `.json` 后缀替换掉，
    // 变成 `app.corrupt-…`，用户就分不清原文件是什么了。
    let file_name = path
        .file_name()
        .with_context(|| format!("目标路径没有合法文件名：{}", path.display()))?
        .to_string_lossy()
        .into_owned();
    let backup = path.with_file_name(format!("{file_name}.corrupt-{stamp}"));
    fs::rename(path, &backup).with_context(|| {
        format!(
            "隔离损坏文件失败：{} -> {}",
            path.display(),
            backup.display()
        )
    })?;
    Ok(backup)
}

/// 读取文本文件；文件不存在时返回 `None`。
///
/// 单独包一层是为了让"文件不存在"与"读失败"在调用处容易区分。
pub fn read_to_string_if_exists(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("读取文件失败：{}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 可写目录探测通过() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        ensure_writable(dir.path()).expect("临时目录应当可写");
    }

    #[test]
    fn 不存在的目录探测失败() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let missing = dir.path().join("not-created");
        assert!(ensure_writable(&missing).is_err());
    }

    #[test]
    fn 原子写入后内容完整且没有残留临时文件() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let target = dir.path().join("accounts.json");

        write_atomic(&target, "{\"a\":1}").expect("写入成功");

        assert_eq!(fs::read_to_string(&target).unwrap(), "{\"a\":1}");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "不应残留临时文件");
    }

    #[test]
    fn 原子写入会覆盖旧内容() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let target = dir.path().join("app.json");

        write_atomic(&target, "old").expect("第一次写入");
        write_atomic(&target, "new").expect("第二次写入");

        assert_eq!(fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn 损坏文件被改名备份() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let target = dir.path().join("app.json");
        fs::write(&target, "{broken").unwrap();

        let backup = quarantine_corrupt_file(&target).expect("隔离成功");

        assert!(!target.exists(), "原文件应已被移走");
        assert!(backup.exists(), "备份文件应存在");
        assert!(
            backup
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("corrupt-"),
            "备份文件名应带 corrupt- 前缀"
        );
    }

    #[test]
    fn 读取不存在的文件返回none() {
        let dir = tempfile::tempdir().expect("创建临时目录");
        let missing = dir.path().join("missing.json");
        assert!(read_to_string_if_exists(&missing).unwrap().is_none());
    }
}

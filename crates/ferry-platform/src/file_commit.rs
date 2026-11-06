//! 文件提交、稳定身份与持久化：原子移动且绝不替换已有目标。

use std::io;
use std::path::Path;

/// 把同一文件系统上的文件或目录原子移动到尚不存在的位置。
///
/// 目标存在（包括符号链接）时返回 `AlreadyExists`，跨文件系统时返回
/// `CrossesDevices`。调用方可先复制到目标卷上的临时目录，再重新提交。
pub trait FileCommit: Send + Sync + 'static {
    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// 同一卷内持久文件身份（改名后不变），用于确认崩溃恢复时目标就是原提交对象。
    fn identity(&self, path: &Path) -> io::Result<String>;
    fn identity_open_file(&self, file: &std::fs::File) -> io::Result<String>;

    /// 将已 sync_all 的临时记录原子替换为正式记录。
    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()>;

    /// 尽可能同步目录项。不支持目录同步的平台返回 Unsupported，调用方不得夸大掉电保证。
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
}

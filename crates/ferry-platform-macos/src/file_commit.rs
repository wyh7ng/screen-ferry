//! macOS 原子不覆盖提交，使用 renamex_np(RENAME_EXCL)。

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use ferry_platform::FileCommit;

#[derive(Clone, Copy, Debug, Default)]
pub struct MacFileCommit;

impl FileCommit for MacFileCommit {
    fn identity_open_file(&self, file: &std::fs::File) -> io::Result<String> {
        let meta = file.metadata()?;
        Ok(format!("{}:{}", meta.dev(), meta.ino()))
    }
    fn identity(&self, path: &Path) -> io::Result<String> {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(format!("{}:{}", meta.dev(), meta.ino()))
    }

    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from = CString::new(from.as_os_str().as_bytes())?;
        let to = CString::new(to.as_os_str().as_bytes())?;
        // SAFETY: 路径均以 NUL 结尾，缓冲区在调用期间有效；EXCL 保证不替换目标。
        let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

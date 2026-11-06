//! 场景测试的文件提交器。目录只在受控测试目录中使用，测试进程内串行化提交。
//!
//! 真正的跨进程不覆盖保证由平台实现及平台测试验证。

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use ferry_platform::FileCommit;

static COMMIT: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, Default)]
pub struct MockFileCommit;

impl FileCommit for MockFileCommit {
    fn identity_open_file(&self, file: &fs::File) -> io::Result<String> {
        let meta = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(format!("{}:{}", meta.dev(), meta.ino()))
        }
        #[cfg(not(unix))]
        {
            Ok(format!("{:?}", meta.created()?))
        }
    }
    fn identity(&self, path: &Path) -> io::Result<String> {
        let meta = fs::symlink_metadata(path)?;
        if meta.file_type().is_symlink() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(format!("{}:{}", meta.dev(), meta.ino()))
        }
        #[cfg(not(unix))]
        {
            // 仅用于受控测试。真实 Windows 身份由 WinFileCommit 获取文件 ID。
            Ok(format!("{:?}", meta.created()?))
        }
    }

    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        #[cfg(unix)]
        {
            fs::File::open(path)?.sync_all()
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(io::ErrorKind::Unsupported.into())
        }
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let _guard = COMMIT.lock().unwrap_or_else(|e| e.into_inner());
        match fs::symlink_metadata(to) {
            Ok(_) => return Err(io::Error::from(io::ErrorKind::AlreadyExists)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        if from.is_file() {
            fs::hard_link(from, to)?;
            fs::remove_file(from)
        } else {
            fs::rename(from, to)
        }
    }
}

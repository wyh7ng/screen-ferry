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
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = file.metadata()?;
            Ok(format!("{}:{}", meta.dev(), meta.ino()))
        }
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::AsRawHandle;
            use windows::Win32::Foundation::HANDLE;
            use windows::Win32::Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
            };

            let mut info = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: file 在调用期间持有有效句柄，info 是有效可写输出。
            unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
                .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xffff))?;
            // 创建时间可能相同，不能用它冒充文件身份。与生产提交器一样使用卷和文件 ID。
            Ok(format!(
                "{}:{}:{}",
                info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
            ))
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            let _ = file;
            Err(io::ErrorKind::Unsupported.into())
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
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;

            let file = fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
                .open(path)?;
            self.identity_open_file(&file)
        }
        #[cfg(not(any(unix, target_os = "windows")))]
        {
            Err(io::ErrorKind::Unsupported.into())
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

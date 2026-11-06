//! Windows 原子不覆盖提交，不允许 MoveFileEx 替换已有目标或隐式跨卷复制。

use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use ferry_platform::FileCommit;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, GetFileInformationByHandle,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::HSTRING;

#[derive(Clone, Copy, Debug, Default)]
pub struct WinFileCommit;

impl FileCommit for WinFileCommit {
    fn identity(&self, path: &Path) -> io::Result<String> {
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(path)?;
        self.identity_open_file(&file)
    }
    fn identity_open_file(&self, file: &std::fs::File) -> io::Result<String> {
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: 文件句柄在调用期间有效，info 是可写输出。
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xffff))?;
        Ok(format!(
            "{}:{}:{}",
            info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
        ))
    }

    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()> {
        let (from, to) = (
            HSTRING::from(from.as_os_str()),
            HSTRING::from(to.as_os_str()),
        );
        // SAFETY: 路径有效，允许原子替换旧记录并请求写穿。
        unsafe {
            MoveFileExW(
                &from,
                &to,
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xffff))
    }

    fn sync_directory(&self, _path: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        let from = HSTRING::from(from.as_os_str());
        let to = HSTRING::from(to.as_os_str());
        // SAFETY: HSTRING 在调用期间保持路径有效；没有 REPLACE_EXISTING/COPY_ALLOWED。
        unsafe { MoveFileExW(&from, &to, MOVEFILE_WRITE_THROUGH) }
            .map_err(|e| io::Error::from_raw_os_error(e.code().0 & 0xffff))
    }
}

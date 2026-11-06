//! 后台实例锁。锁由内核维护，进程退出即释放；锁文件本身不删除，避免锁住不同 inode。

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

/// 持有到后台结束的进程锁。
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

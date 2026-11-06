//! 有界后台日志写入。输入线程只做 try_send，磁盘变慢不会堵住输入。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use time::{OffsetDateTime, UtcOffset};

/// 单条日志最大字节数。完整事件在后台整体写入，避免多线程交错。
pub const MAX_LOG_RECORD: usize = 64 * 1024;

#[derive(Clone)]
pub struct LogWriter {
    sender: SyncSender<Vec<u8>>,
    dropped: Arc<AtomicU64>,
    stopping: Arc<AtomicBool>,
}

/// 生命周期必须覆盖全部日志使用者。退出最多等两秒，不因卡住的磁盘永久停机。
pub struct LogGuard {
    stopping: Arc<AtomicBool>,
    finished: mpsc::Receiver<()>,
    thread: Option<JoinHandle<()>>,
}

impl LogWriter {
    pub fn new(
        output: impl Write + Send + 'static,
        capacity: usize,
    ) -> io::Result<(Self, LogGuard)> {
        Self::with_utc_offset(output, capacity, 0)
    }

    /// 使用调用方启动时取得的时区，丢失告警与普通日志采用同一时间格式。
    pub fn with_utc_offset(
        mut output: impl Write + Send + 'static,
        capacity: usize,
        offset_seconds: i32,
    ) -> io::Result<(Self, LogGuard)> {
        let offset = UtcOffset::from_whole_seconds(offset_seconds).unwrap_or(UtcOffset::UTC);
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(capacity.max(1));
        let dropped = Arc::new(AtomicU64::new(0));
        let stopping = Arc::new(AtomicBool::new(false));
        let lost = dropped.clone();
        let stop = stopping.clone();
        let (done, finished) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("ferry-log".into())
            .spawn(move || {
                loop {
                    match receiver.recv_timeout(Duration::from_millis(100)) {
                        Ok(record) => {
                            report_dropped(&mut output, &lost, offset);
                            write_record(&mut output, &record, &lost);
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            report_dropped(&mut output, &lost, offset);
                            let _ = output.flush();
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                        }
                    }
                    if stop.load(Ordering::Acquire) {
                        for record in receiver.try_iter() {
                            write_record(&mut output, &record, &lost);
                        }
                        break;
                    }
                }
                report_dropped(&mut output, &lost, offset);
                let _ = output.flush();
                let _ = done.send(());
            })?;
        Ok((
            Self {
                sender,
                dropped,
                stopping: stopping.clone(),
            },
            LogGuard {
                stopping,
                finished,
                thread: Some(thread),
            },
        ))
    }

    /// 不等待队列空间。过载只丢日志，累计计数由日志线程恢复后报告。
    pub fn record(&self, bytes: &[u8]) -> bool {
        if self.stopping.load(Ordering::Acquire) {
            return false;
        }
        let mut record = bytes[..bytes.len().min(MAX_LOG_RECORD)].to_vec();
        if bytes.len() > MAX_LOG_RECORD {
            record.truncate(MAX_LOG_RECORD - 32);
            record.extend_from_slice(b" [log record truncated]\n");
        }
        if self.sender.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            false
        } else {
            true
        }
    }
}

fn write_record(output: &mut impl Write, record: &[u8], lost: &AtomicU64) {
    if output.write_all(record).is_err() {
        lost.fetch_add(1, Ordering::Relaxed);
    }
}

fn report_dropped(output: &mut impl Write, lost: &AtomicU64, offset: UtcOffset) {
    let count = lost.swap(0, Ordering::AcqRel);
    if count == 0 {
        return;
    }
    let now = OffsetDateTime::now_utc().to_offset(offset);
    let marker = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}  WARN 日志写入未完成，已丢弃 {count} 条日志（队列拥堵或磁盘错误）\n",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
    );
    if output.write_all(marker.as_bytes()).is_err() {
        // 告警本身也写不下时保留数量，磁盘恢复后的下一条仍能说明损失。
        lost.fetch_add(count, Ordering::Relaxed);
    }
}

/// 两卷轮转日志。改名失败时保留原文件，绝不以截断原卷来掩盖旋转失败。
pub struct RotatingLog {
    path: PathBuf,
    previous: PathBuf,
    file: Option<File>,
    len: u64,
    limit: u64,
}

impl RotatingLog {
    pub fn open(path: PathBuf, previous: PathBuf, limit: u64) -> io::Result<Self> {
        let mut log = Self {
            path,
            previous,
            file: None,
            len: 0,
            limit,
        };
        log.reopen()?;
        Ok(log)
    }

    fn reopen(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.len = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file = None;
        if let Err(error) = fs::rename(&self.path, &self.previous) {
            // 原卷仍然有效，恢复句柄供后续重试。失败本身必须上报给异步writer计数。
            let _ = self.reopen();
            return Err(error);
        }
        // append 不会破坏恰好由另一进程创建的同名文件；打开失败也保留上一卷。
        self.reopen()
    }
}

impl Write for RotatingLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.file.is_none() {
            self.reopen()?;
        }
        if self.len > 0 && self.len.saturating_add(bytes.len() as u64) > self.limit {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("运行日志文件不可用"))?;
        let written = file.write(bytes)?;
        self.len = self.len.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::other("运行日志文件不可用"))?
            .flush()
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if self.finished.recv_timeout(Duration::from_secs(2)).is_ok()
            && let Some(thread) = self.thread.take()
        {
            let _ = thread.join();
        }
    }
}

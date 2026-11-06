//! 日志。
//!
//! 常驻运行时总是写运行日志文件（`ferry-store` 指定的位置），从终端运行时同时写到终端
//! （Windows 上常驻运行不接终端）；其他命令只写到终端。
//!
//! - 时间用本机时间（`2026-11-02 06:12:03.104`），提问题时对得上“几点出的问题”。
//!   时区在启动时取一次：之后进程里有了多个线程，有的系统就不让再取了。
//! - 文件写满 2 MB 时改名成上一卷（`screenferry.log.1`）再开一个，只留这一卷。
//! - “详细记录”（DEBUG）可以在运行中打开、关闭，见 [`set_verbose`]。

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::sync::{Mutex, OnceLock};

use ferry_store::{LogGuard, LogWriter, MAX_LOG_RECORD, RotatingLog, Store};
use time::UtcOffset;
use time::macros::format_description;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::fmt::writer::{BoxMakeWriter, MakeWriter};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Registry, reload};

use crate::cli::{Mode, Options};

/// 运行日志一卷的大小上限。
const LOG_LIMIT: u64 = 2 * 1024 * 1024;

/// 运行中改日志级别用。
static LEVEL: OnceLock<reload::Handle<LevelFilter, Registry>> = OnceLock::new();
/// 启动时取到的本机时区。
static OFFSET: OnceLock<UtcOffset> = OnceLock::new();
static LOG_GUARD: OnceLock<Mutex<Option<LogGuard>>> = OnceLock::new();

pub(crate) fn init(options: &Options) {
    let offset =
        *OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    let timer = OffsetTime::new(
        offset,
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second].[subsecond digits:3]"),
    );
    let (level, handle) = reload::Layer::new(if options.verbose {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    });
    let _ = LEVEL.set(handle);

    let terminal = io::stderr().is_terminal();
    let file = matches!(options.mode, Mode::Resident { .. })
        .then(|| log_file(options))
        .flatten()
        .map(Mutex::new);
    // Windows 的控制台窗口不一定认颜色控制码，不加颜色；写进文件时也不加。
    let ansi = terminal && file.is_none() && !cfg!(target_os = "windows");
    let sink: Box<dyn Write + Send> = match file {
        Some(file) if terminal => Box::new(DualOutput(file.into_inner().unwrap(), io::stderr())),
        Some(file) => Box::new(file.into_inner().unwrap()),
        None => Box::new(io::stderr()),
    };
    let writer = match LogWriter::with_utc_offset(sink, 256, offset.whole_seconds()) {
        Ok((writer, guard)) => {
            let _ = LOG_GUARD.set(Mutex::new(Some(guard)));
            BoxMakeWriter::new(AsyncWriter(writer))
        }
        Err(_) => BoxMakeWriter::new(io::stderr),
    };
    let output = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_timer(timer)
        .with_ansi(ansi)
        .with_writer(writer);
    tracing_subscriber::registry()
        .with(level)
        .with(output)
        .init();
}

/// 打开或关闭详细记录。
pub(crate) fn set_verbose(on: bool) {
    if let Some(handle) = LEVEL.get() {
        let level = if on {
            LevelFilter::DEBUG
        } else {
            LevelFilter::INFO
        };
        let _ = handle.reload(level);
    }
}

/// 本机时区与 UTC 相差的秒数。
pub(crate) fn utc_offset() -> i32 {
    OFFSET.get().map_or(0, |o| o.whole_seconds())
}

fn log_file(options: &Options) -> Option<RotatingLog> {
    let store = match &options.data_dir {
        Some(dir) => Store::open(dir),
        None => Store::open_default(),
    }
    .ok()?;
    fs::create_dir_all(store.logs_dir()).ok()?;
    RotatingLog::open(store.log_file(), store.previous_log_file(), LOG_LIMIT).ok()
}

/// 常驻进程退出前释放后台writer，排空已经接受的记录。
pub(crate) fn shutdown() {
    if let Some(guard) = LOG_GUARD.get()
        && let Ok(mut guard) = guard.lock()
    {
        drop(guard.take());
    }
}

struct AsyncWriter(LogWriter);
struct EventWriter {
    sink: LogWriter,
    bytes: Vec<u8>,
}
impl<'a> MakeWriter<'a> for AsyncWriter {
    type Writer = EventWriter;
    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            sink: self.0.clone(),
            bytes: Vec::with_capacity(256),
        }
    }
}
impl Write for EventWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(
            MAX_LOG_RECORD
                .saturating_add(1)
                .saturating_sub(self.bytes.len()),
        );
        self.bytes.extend_from_slice(&buf[..n]);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for EventWriter {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            self.sink.record(&self.bytes);
        }
    }
}
struct DualOutput<A, B>(A, B);
impl<A: Write, B: Write> Write for DualOutput<A, B> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write_all(bytes)?;
        self.1.write_all(bytes)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()?;
        self.1.flush()
    }
}

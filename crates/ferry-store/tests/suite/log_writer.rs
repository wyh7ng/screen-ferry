use ferry_store::LogWriter;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

struct Sink {
    started: mpsc::Sender<()>,
    unblock: mpsc::Receiver<()>,
    data: Arc<Mutex<Vec<u8>>>,
    first: bool,
}
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        if self.first {
            self.first = false;
            self.started.send(()).unwrap();
            self.unblock.recv().unwrap();
        }
        self.data.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn blocked_disk_does_not_block_producer_and_loss_is_reported_after_recovery() {
    let (started, ready) = mpsc::channel();
    let (unblock, wait) = mpsc::channel();
    let data = Arc::new(Mutex::new(Vec::new()));
    let (writer, guard) = LogWriter::new(
        Sink {
            started,
            unblock: wait,
            data: data.clone(),
            first: true,
        },
        1,
    )
    .unwrap();
    assert!(writer.record(b"first\n"));
    ready.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(writer.record(b"second\n"));
    let (done, finished) = mpsc::channel();
    let producer = std::thread::spawn(move || {
        assert!(!writer.record(b"overflow\n"));
        done.send(()).unwrap();
    });
    let result = finished.recv_timeout(Duration::from_secs(1));
    unblock.send(()).unwrap();
    result.expect("the writer must not wait for the blocked disk");
    producer.join().unwrap();
    drop(guard);
    let text = String::from_utf8(data.lock().unwrap().clone()).unwrap();
    assert!(text.contains("first\n"));
    assert!(text.contains("second\n"));
    assert!(text.contains("已丢弃 1 条日志"));
    assert!(!text.contains("overflow"));
}

struct FailingSink {
    data: Arc<Mutex<Vec<u8>>>,
    fail_writes: usize,
    attempted: mpsc::Sender<()>,
}
impl Write for FailingSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.attempted.send(()).unwrap();
        if self.fail_writes > 0 {
            self.fail_writes -= 1;
            return Err(io::Error::other("disk full"));
        }
        self.data.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn failed_record_and_failed_loss_marker_are_reported_after_the_disk_recovers() {
    let data = Arc::new(Mutex::new(Vec::new()));
    let (attempted, attempts) = mpsc::channel();
    let (writer, guard) = LogWriter::with_utc_offset(
        FailingSink {
            data: data.clone(),
            fail_writes: 3,
            attempted,
        },
        8,
        8 * 3600,
    )
    .unwrap();
    assert!(writer.record(b"lost first\n"));
    attempts.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(writer.record(b"lost second\n"));
    // 第二条之前的丢失标记及第二条本身也失败，计数必须保留。
    attempts.recv_timeout(Duration::from_secs(1)).unwrap();
    attempts.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(writer.record(b"recovered\n"));
    drop(guard);
    let text = String::from_utf8(data.lock().unwrap().clone()).unwrap();
    assert!(text.contains("已丢弃 2 条日志"), "{text}");
    assert!(text.contains("recovered\n"));
    let marker = text.lines().find(|line| line.contains("已丢弃")).unwrap();
    assert_eq!(&marker[4..5], "-");
    assert_eq!(&marker[10..11], " ");
    assert_eq!(
        &marker[23..],
        "  WARN 日志写入未完成，已丢弃 2 条日志（队列拥堵或磁盘错误）"
    );
}

#[test]
fn a_failed_rotation_preserves_the_current_file_and_retries_after_recovery() {
    use ferry_store::RotatingLog;
    let dir = crate::support::TempDir::new();
    std::fs::create_dir_all(dir.path()).unwrap();
    let path = dir.path().join("log");
    let previous = dir.path().join("previous");
    std::fs::create_dir(&previous).unwrap();
    let mut log = RotatingLog::open(path.clone(), previous.clone(), 8).unwrap();
    log.write_all(b"original").unwrap();
    assert!(log.write_all(b"new\n").is_err(), "不能把文件改名成目录");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"original",
        "失败后绝不能截断原卷"
    );
    std::fs::remove_dir(&previous).unwrap();
    log.write_all(b"new\n").unwrap();
    log.flush().unwrap();
    assert_eq!(std::fs::read(previous).unwrap(), b"original");
    assert_eq!(std::fs::read(path).unwrap(), b"new\n");
}

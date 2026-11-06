//! 所有文件发送任务共用的 FIFO 节奏器。限制应用层文件内容字节，不保证物理链路隔离。

use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, Instant};

/// 可在线调整的总文件发送速率，单位字节/秒；0 不限速。
///
/// 每次按一个分块排队，Tokio 的 FIFO mutex 让并发任务轮流取得发送额度。
/// 等待 future 可以取消，取消释放队列位置；不阻塞线程或预扣其他任务的额度。
pub struct FileRateLimit {
    rate: watch::Sender<u64>,
    turn: Mutex<()>,
}

impl Default for FileRateLimit {
    fn default() -> Self {
        Self::new(0)
    }
}

impl FileRateLimit {
    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            rate: watch::channel(bytes_per_second).0,
            turn: Mutex::new(()),
        }
    }

    pub fn set(&self, bytes_per_second: u64) {
        self.rate.send_replace(bytes_per_second);
    }

    pub fn get(&self) -> u64 {
        *self.rate.borrow()
    }

    /// 应用层分块不超过约 50ms 的额度，低速时也持续取得进展。
    pub fn chunk_size(&self) -> usize {
        let rate = self.get();
        if rate == 0 {
            ferry_proto::limits::MAX_CHUNK_LEN
        } else {
            (rate / 20).clamp(1, ferry_proto::limits::MAX_CHUNK_LEN as u64) as usize
        }
    }

    /// 等待发送 `bytes` 字节所需的总预算。每个任务只应排队一个分块。
    pub async fn acquire(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let _turn = self.turn.lock().await;
        let mut rate = self.rate.subscribe();
        let mut remaining = bytes as f64;
        loop {
            let per_second = *rate.borrow_and_update();
            if per_second == 0 {
                return;
            }
            let started = Instant::now();
            let wait = Duration::from_secs_f64((remaining / per_second as f64).min(3600.0));
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = rate.changed() => {}
            }
            remaining -= started.elapsed().as_secs_f64() * per_second as f64;
            if remaining <= 0.0 {
                return;
            }
        }
    }

    /// 文件流使用的小额度接口：在线调低速率时缩小尚在等待的分块，避免旧大分块长期占队首。
    pub async fn acquire_chunk(&self, maximum: usize) -> usize {
        if maximum == 0 {
            return 0;
        }
        let _turn = self.turn.lock().await;
        let mut rate = self.rate.subscribe();
        let mut earned = 0.0;
        loop {
            let per_second = *rate.borrow_and_update();
            let quota = if per_second == 0 {
                maximum
            } else {
                maximum.min(
                    (per_second / 20).clamp(1, ferry_proto::limits::MAX_CHUNK_LEN as u64) as usize,
                )
            };
            if per_second == 0 || earned >= quota as f64 {
                return quota;
            }
            let started = Instant::now();
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs_f64((quota as f64 - earned) / per_second as f64)) => {}
                _ = rate.changed() => {}
            }
            earned += started.elapsed().as_secs_f64() * per_second as f64;
        }
    }
}

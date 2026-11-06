//! 输入链路的本机单调时钟统计。固定大小直方图，不记录按键或逐事件日志。

use std::time::{Duration, Instant};

/// 一段时间内的延迟分布。百分位为固定直方图桶上界（约 12.5% 分辨率），最大值精确。
/// 所有时间都是本机测量的微秒，不能用两台设备的时刻相减。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatencySummary {
    pub count: u64,
    pub p50_us: u64,
    pub p95_us: u64,
    pub p99_us: u64,
    pub max_us: u64,
    pub over_100ms: u64,
}

/// 有界队列状态。深度为快照，high_water/coalesced/overflows为该队列生命周期累计值。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueSummary {
    pub depth: usize,
    pub high_water: usize,
    /// 快照中最老一项的等待时间；捕获队列不窥视接收端，无法测量时为 None。
    pub oldest_us: Option<u64>,
    /// 本窗口中已经出队的项目，最长实际等待时间；不能当成当前队首的年龄。
    pub max_wait_us: u64,
    pub coalesced: u64,
    pub overflows: u64,
}

/// 最近统计窗口内的本机输入指标；某条链路不存在的阶段 count 为 0。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InputMetrics {
    /// 本机采样截止的单调时刻，仅进程内使用，不通过IPC传输。
    pub collected_at: Option<Instant>,
    pub window_ms: u64,
    /// 原生捕获回调开始到引擎消费，包括捕获转换和入队后的等待。
    pub capture_queue: LatencySummary,
    pub send_queue: LatencySummary,
    /// 编码、TLS写入与flush等待，不代表对端处理完毕。
    pub send: LatencySummary,
    /// 本机解码得到输入到平台注入调用返回，不含系统合成/屏幕呈现。
    pub inject: LatencySummary,
    pub rtt: LatencySummary,
    /// 定时任务相对于预定唤醒时刻的迟到量。
    pub scheduler: LatencySummary,
    pub capture: QueueSummary,
    pub outbound: QueueSummary,
}

/// 固定内存的近似分位统计，便于网络/输入模块与基准使用。
pub struct LatencyHistogram {
    buckets: [u64; 256],
    count: u64,
    max_us: u64,
    over_100ms: u64,
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; 256],
            count: 0,
            max_us: 0,
            over_100ms: 0,
        }
    }
}

impl LatencyHistogram {
    pub fn record(&mut self, elapsed: Duration) {
        let us = micros(elapsed);
        let index = bucket(us);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.count = self.count.saturating_add(1);
        self.max_us = self.max_us.max(us);
        if elapsed > Duration::from_millis(100) {
            self.over_100ms = self.over_100ms.saturating_add(1);
        }
    }

    pub fn take(&mut self) -> LatencySummary {
        let result = LatencySummary {
            count: self.count,
            p50_us: self.percentile(50),
            p95_us: self.percentile(95),
            p99_us: self.percentile(99),
            max_us: self.max_us,
            over_100ms: self.over_100ms,
        };
        *self = Self::default();
        result
    }

    fn percentile(&self, percent: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (u128::from(self.count) * u128::from(percent)).div_ceil(100) as u64;
        let mut seen = 0u64;
        for (index, &count) in self.buckets.iter().enumerate() {
            seen = seen.saturating_add(count);
            if seen >= target {
                return upper(index).min(self.max_us);
            }
        }
        self.max_us
    }
}

fn bucket(us: u64) -> usize {
    if us < 8 {
        return us as usize;
    }
    let exponent = 63 - us.leading_zeros() as usize;
    let sub = ((us >> (exponent - 3)) & 7) as usize;
    (8 + (exponent - 3) * 8 + sub).min(255)
}

fn upper(index: usize) -> u64 {
    if index < 8 {
        return index as u64;
    }
    if index == 255 {
        return u64::MAX;
    }
    let exponent = 3 + (index - 8) / 8;
    let sub = (index - 8) % 8;
    ((9 + sub as u64) << (exponent - 3)) - 1
}

pub(crate) fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub(crate) const REPORT_EVERY: Duration = Duration::from_secs(5);

pub(crate) struct MetricsWindow {
    since: Instant,
    pub capture: LatencyHistogram,
    pub queued: LatencyHistogram,
    pub send: LatencyHistogram,
    pub inject: LatencyHistogram,
    pub rtt: LatencyHistogram,
    pub scheduler: LatencyHistogram,
}

impl Default for MetricsWindow {
    fn default() -> Self {
        Self {
            since: Instant::now(),
            capture: Default::default(),
            queued: Default::default(),
            send: Default::default(),
            inject: Default::default(),
            rtt: Default::default(),
            scheduler: Default::default(),
        }
    }
}

impl MetricsWindow {
    pub fn take(&mut self, mut capture: QueueSummary, mut outbound: QueueSummary) -> InputMetrics {
        let now = Instant::now();
        let capture_queue = self.capture.take();
        let send_queue = self.queued.take();
        capture.max_wait_us = capture_queue.max_us;
        outbound.max_wait_us = send_queue.max_us;
        let window_ms =
            u64::try_from(now.duration_since(self.since).as_millis()).unwrap_or(u64::MAX);
        self.since = now;
        InputMetrics {
            collected_at: Some(now),
            window_ms,
            capture_queue,
            send_queue,
            send: self.send.take(),
            inject: self.inject.take(),
            rtt: self.rtt.take(),
            scheduler: self.scheduler.take(),
            capture,
            outbound,
        }
    }
}

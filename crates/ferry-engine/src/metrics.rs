//! 输入链路的本机单调时钟统计。固定大小直方图，不记录按键或逐事件日志。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ferry_trust::Fingerprint;

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
    /// 独立写队列入队至编码、传输写入与flush完成，不代表对端处理完毕。
    pub send: LatencySummary,
    /// 本机解码得到输入到平台注入调用返回，含接收队列与平台线程等待，不含屏幕呈现。
    pub inject: LatencySummary,
    pub rtt: LatencySummary,
    /// 本任务定时检查的迟到量，可能包含异步等待，不等于整个运行时被阻塞。
    pub scheduler: LatencySummary,
    /// 后台显示器查询执行耗时，不代表核心运行时被阻塞。
    pub display_query: LatencySummary,
    /// 切屏时同步平台操作（抓取、归还、隐藏、锁定键同步）的耗时。
    pub cursor_switch: LatencySummary,
    /// Ping/Pong 编码、写入及 flush 的等待；不代表对端已收到。
    pub heartbeat_send: LatencySummary,
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
    pub(crate) fn merge(&mut self, other: &mut Self) {
        for (ours, theirs) in self.buckets.iter_mut().zip(other.buckets) {
            *ours = ours.saturating_add(theirs);
        }
        self.count = self.count.saturating_add(other.count);
        self.max_us = self.max_us.max(other.max_us);
        self.over_100ms = self.over_100ms.saturating_add(other.over_100ms);
        *other = Self::default();
    }
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
    pub display_query: LatencyHistogram,
    pub cursor_switch: LatencyHistogram,
    pub heartbeat_send: LatencyHistogram,
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
            display_query: Default::default(),
            cursor_switch: Default::default(),
            heartbeat_send: Default::default(),
        }
    }
}

impl MetricsWindow {
    pub fn record_display_query(&mut self, elapsed: Duration) {
        self.display_query.record(elapsed);
    }

    pub fn record_cursor_switch(&mut self, elapsed: Duration) {
        self.cursor_switch.record(elapsed);
    }

    pub fn record_heartbeat_send(&mut self, elapsed: Duration) {
        self.heartbeat_send.record(elapsed);
    }

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
            display_query: self.display_query.take(),
            cursor_switch: self.cursor_switch.take(),
            heartbeat_send: self.heartbeat_send.take(),
            capture,
            outbound,
        }
    }
}

/// 一个已收集的性能窗口；只含时长、计数和来源，不含输入内容。
#[derive(Clone, Debug)]
pub struct RecordedMetrics {
    pub peer: Option<Fingerprint>,
    pub metrics: InputMetrics,
}

/// 一次 >100ms 窗口及其前后上下文。最多前2、触发1、后2个同源窗口。
#[derive(Clone, Debug)]
pub struct MetricIncident {
    pub peer: Option<Fingerprint>,
    pub triggered_at: Instant,
    stages: Vec<&'static str>,
    windows: Vec<InputMetrics>,
    following: usize,
    related: Vec<RecordedMetrics>,
}

impl MetricIncident {
    /// 触发时间前后15秒内的其他本机来源，最多16窗；不表示各阶段属于同一操作。
    pub fn related(&self) -> &[RecordedMetrics] {
        &self.related
    }
    pub fn windows(&self) -> &[InputMetrics] {
        &self.windows
    }

    /// 触发窗口里超过阈值的阶段名，只描述观测，不推断原因。
    pub fn stages(&self) -> &[&'static str] {
        &self.stages
    }
}

/// 当前来源生命周期内已接收窗口的累计值，不从近似分位数推算平均或全局分位数。
#[derive(Clone, Debug)]
pub struct MetricTotals {
    pub peer: Option<Fingerprint>,
    pub windows: u64,
    pub last_collected: Instant,
    pub stages: std::collections::BTreeMap<&'static str, StageTotals>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct StageTotals {
    pub count: u64,
    pub max_us: u64,
    pub over_100ms: u64,
}

/// 有界的诊断上下文。常规窗口保留60秒，每来源最多12个，全局最多128个；
/// 另外保留最近4次尖峰，每次最多5个窗口，直到来源移除、清空或被新尖峰挤出。
/// 同源尖峰的后续2个窗口内再次触发只并入当前上下文，不延长容量。
/// 调用者应在引擎换代时clear、连接换代时remove_peer，不能跨会话拼接。
#[derive(Clone, Default)]
pub struct MetricsHistory {
    recent: VecDeque<RecordedMetrics>,
    incidents: VecDeque<MetricIncident>,
    totals: VecDeque<MetricTotals>,
}

impl MetricsHistory {
    const AGE: Duration = Duration::from_secs(60);
    const PER_SOURCE: usize = 12;
    const TOTAL: usize = 128;
    const INCIDENTS: usize = 4;

    /// 收集时间缺失、未来或同来源逆序/重复的窗口不进入诊断历史。
    pub fn record(&mut self, peer: Option<Fingerprint>, metrics: InputMetrics) {
        let now = Instant::now();
        self.recent.retain(|entry| fresh(&entry.metrics, now));
        let Some(at) = metrics.collected_at.filter(|at| *at <= now) else {
            return;
        };
        if now.duration_since(at) > Self::AGE
            || self
                .totals
                .iter()
                .any(|entry| entry.peer == peer && entry.last_collected >= at)
        {
            return;
        }
        if !self.totals.iter().any(|entry| entry.peer == peer) {
            if self.totals.len() >= Self::TOTAL {
                self.totals.pop_front();
            }
            self.totals.push_back(MetricTotals {
                peer,
                windows: 0,
                last_collected: at,
                stages: Default::default(),
            });
        }
        if let Some(total) = self.totals.iter_mut().find(|entry| entry.peer == peer) {
            total.windows = total.windows.saturating_add(1);
            total.last_collected = at;
            for (name, stage) in stages(&metrics) {
                let sum = total.stages.entry(name).or_default();
                sum.count = sum.count.saturating_add(stage.count);
                sum.over_100ms = sum.over_100ms.saturating_add(stage.over_100ms);
                sum.max_us = sum.max_us.max(stage.max_us);
            }
        }
        let mut continuing = false;
        for incident in &mut self.incidents {
            if related_source(incident.peer, peer)
                && related_time(at, incident.triggered_at)
                && incident.related.len() < 16
            {
                incident.related.push(RecordedMetrics {
                    peer,
                    metrics: metrics.clone(),
                });
            }
            if incident.peer == peer && incident.following > 0 {
                incident.windows.push(metrics.clone());
                incident.following -= 1;
                continuing = true;
            }
        }
        let stages = spike_stages(&metrics);
        if !continuing && !stages.is_empty() {
            let mut before: Vec<_> = self
                .recent
                .iter()
                .rev()
                .filter(|entry| entry.peer == peer)
                .take(2)
                .map(|entry| entry.metrics.clone())
                .collect();
            before.reverse();
            before.push(metrics.clone());
            let related = self
                .recent
                .iter()
                .rev()
                .filter(|entry| {
                    related_source(peer, entry.peer)
                        && entry
                            .metrics
                            .collected_at
                            .is_some_and(|when| related_time(when, at))
                })
                .take(8)
                .cloned()
                .collect();
            self.incidents.push_back(MetricIncident {
                peer,
                triggered_at: at,
                stages,
                windows: before,
                following: 2,
                related,
            });
            while self.incidents.len() > Self::INCIDENTS {
                self.incidents.pop_front();
            }
        }
        while self
            .recent
            .iter()
            .filter(|entry| entry.peer == peer)
            .count()
            >= Self::PER_SOURCE
        {
            if let Some(index) = self.recent.iter().position(|entry| entry.peer == peer) {
                self.recent.remove(index);
            }
        }
        self.recent.push_back(RecordedMetrics { peer, metrics });
        while self.recent.len() > Self::TOTAL {
            self.recent.pop_front();
        }
    }

    /// 使用采样截止的单调时刻筛选；导出时不因长时间没有新事件而显示过期常规窗口。
    pub fn recent(&self, now: Instant) -> impl Iterator<Item = &RecordedMetrics> {
        self.recent
            .iter()
            .filter(move |entry| fresh(&entry.metrics, now))
    }

    pub fn incidents(&self) -> impl Iterator<Item = &MetricIncident> {
        self.incidents.iter()
    }

    pub fn totals(&self) -> impl Iterator<Item = &MetricTotals> {
        self.totals.iter()
    }

    pub fn remove_peer(&mut self, peer: Option<Fingerprint>) {
        self.recent.retain(|entry| entry.peer != peer);
        self.incidents.retain(|entry| entry.peer != peer);
        for incident in &mut self.incidents {
            incident.related.retain(|entry| entry.peer != peer);
        }
        self.totals.retain(|entry| entry.peer != peer);
    }

    pub fn clear(&mut self) {
        self.recent.clear();
        self.incidents.clear();
        self.totals.clear();
    }
}

fn fresh(metrics: &InputMetrics, now: Instant) -> bool {
    metrics.collected_at.is_some_and(|at| {
        now.checked_duration_since(at)
            .is_some_and(|age| age <= MetricsHistory::AGE)
    })
}

fn spike_stages(metrics: &InputMetrics) -> Vec<&'static str> {
    stages(metrics)
        .into_iter()
        .filter_map(|(name, stage)| (stage.over_100ms > 0).then_some(name))
        .collect()
}

fn related_time(at: Instant, trigger: Instant) -> bool {
    at.max(trigger).duration_since(at.min(trigger)) <= Duration::from_secs(15)
}

fn related_source(trigger: Option<Fingerprint>, other: Option<Fingerprint>) -> bool {
    // 连接尖峰优先关联公共引擎；引擎尖峰关联连接，避免无关其他peer挤走本机阶段。
    trigger.is_none() != other.is_none()
}

fn stages(metrics: &InputMetrics) -> [(&'static str, &LatencySummary); 9] {
    [
        ("capture_queue", &metrics.capture_queue),
        ("send_queue", &metrics.send_queue),
        ("send", &metrics.send),
        ("inject", &metrics.inject),
        ("rtt", &metrics.rtt),
        ("scheduler", &metrics.scheduler),
        ("display_query", &metrics.display_query),
        ("cursor_switch", &metrics.cursor_switch),
        ("heartbeat_send", &metrics.heartbeat_send),
    ]
}

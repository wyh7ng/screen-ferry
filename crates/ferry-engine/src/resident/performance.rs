//! 引擎采样到本机 IPC 的投影；不推断跨机器单程耗时。
use ferry_trust::Fingerprint;

pub(super) fn project(
    peer: Option<Fingerprint>,
    metrics: crate::metrics::InputMetrics,
) -> ferry_ipc::PeerPerformance {
    ferry_ipc::PeerPerformance {
        fingerprint: peer.map(|p| p.to_hex()),
        measured_at: super::record::now_ms().saturating_sub(metrics.collected_at.map_or(0, |at| {
            i64::try_from(at.elapsed().as_millis()).unwrap_or(i64::MAX)
        })),
        metrics: ferry_ipc::InputMetrics {
            window_ms: metrics.window_ms,
            capture_queue: latency(metrics.capture_queue),
            send_queue: latency(metrics.send_queue),
            send: latency(metrics.send),
            inject: latency(metrics.inject),
            rtt: latency(metrics.rtt),
            scheduler: latency(metrics.scheduler),
            capture: queue(metrics.capture),
            outbound: queue(metrics.outbound),
        },
    }
}

fn latency(s: crate::metrics::LatencySummary) -> ferry_ipc::LatencySummary {
    ferry_ipc::LatencySummary {
        count: s.count,
        p50_us: s.p50_us,
        p95_us: s.p95_us,
        p99_us: s.p99_us,
        max_us: s.max_us,
        over_100ms: s.over_100ms,
    }
}
fn queue(s: crate::metrics::QueueSummary) -> ferry_ipc::QueueSummary {
    ferry_ipc::QueueSummary {
        depth: s.depth,
        high_water: s.high_water,
        oldest_us: s.oldest_us,
        max_wait_us: s.max_wait_us,
        coalesced: s.coalesced,
        overflows: s.overflows,
    }
}

use std::time::{Duration, Instant};

use ferry_engine::{InputMetrics, LatencyHistogram, LatencySummary, MetricsHistory};
use ferry_trust::Fingerprint;

#[test]
fn quantiles_are_bounded_upper_estimates_and_maximum_is_exact() {
    let mut histogram = LatencyHistogram::default();
    for us in 1..=1000 {
        histogram.record(Duration::from_micros(us));
    }
    let stats = histogram.take();
    assert_eq!(stats.count, 1000);
    assert!((500..=562).contains(&stats.p50_us));
    assert!((950..=1000).contains(&stats.p95_us));
    assert!((990..=1000).contains(&stats.p99_us));
    assert_eq!(stats.max_us, 1000);
    assert_eq!(stats.over_100ms, 0);
    assert_eq!(histogram.take(), LatencySummary::default());
}

#[test]
fn zero_long_tail_and_duration_overflow_are_counted_without_panicking() {
    let mut histogram = LatencyHistogram::default();
    histogram.record(Duration::ZERO);
    histogram.record(Duration::from_millis(100));
    histogram.record(Duration::from_micros(100_001));
    histogram.record(Duration::from_secs(u64::MAX));
    let stats = histogram.take();
    assert_eq!(stats.count, 4);
    assert_eq!(stats.over_100ms, 2);
    assert_eq!(stats.max_us, u64::MAX);
    assert_eq!(stats.p99_us, u64::MAX);
}

#[test]
fn a_single_sample_never_rounds_above_its_exact_maximum() {
    for us in [
        0,
        1,
        2,
        7,
        8,
        9,
        63,
        127,
        513,
        999,
        100_001,
        1_000_000,
        u64::MAX,
    ] {
        let mut histogram = LatencyHistogram::default();
        histogram.record(Duration::from_micros(us));
        let stats = histogram.take();
        assert_eq!(
            (stats.p50_us, stats.p95_us, stats.p99_us, stats.max_us),
            (us, us, us, us)
        );
    }
}

fn window(at: Instant, rtt_us: u64) -> InputMetrics {
    let mut histogram = LatencyHistogram::default();
    histogram.record(Duration::from_micros(rtt_us));
    InputMetrics {
        collected_at: Some(at),
        window_ms: 5000,
        rtt: histogram.take(),
        ..Default::default()
    }
}

#[test]
fn diagnostic_history_limits_sources_and_total_and_expires_without_new_samples() {
    let now = Instant::now();
    let mut history = MetricsHistory::default();
    for i in 0..20 {
        history.record(None, window(now - Duration::from_secs(30 - i), 1000));
    }
    assert_eq!(history.recent(now).count(), 12);
    assert_eq!(history.recent(now + Duration::from_secs(61)).count(), 0);
    for peer in 0..140 {
        history.record(Some(Fingerprint::from_bytes([peer; 32])), window(now, 1000));
    }
    assert_eq!(history.recent(now).count(), 128);
    assert!(!history.recent(now).any(|entry| entry.peer.is_none()));
}

#[test]
fn spike_context_keeps_before_and_after_windows_and_outlives_rolling_history() {
    let now = Instant::now();
    let mut history = MetricsHistory::default();
    for (i, us) in [1000, 2000, 120_000, 130_000, 3000, 4000]
        .into_iter()
        .enumerate()
    {
        history.record(None, window(now - Duration::from_secs(6 - i as u64), us));
    }
    let incidents: Vec<_> = history.incidents().collect();
    assert_eq!(incidents[0].stages(), ["rtt"]);
    assert_eq!(incidents.len(), 1, "重叠尖峰合并，不扩展上下文容量");
    assert_eq!(
        incidents[0]
            .windows()
            .iter()
            .map(|m| m.rtt.max_us)
            .collect::<Vec<_>>(),
        [1000, 2000, 120_000, 130_000, 3000]
    );
    assert_eq!(history.recent(now + Duration::from_secs(61)).count(), 0);
    assert_eq!(history.incidents().count(), 1, "常规历史过期后尖峰仍可导出");
}

#[test]
fn spike_history_is_bounded_and_source_lifecycle_cannot_mix_sessions() {
    let now = Instant::now();
    let mut history = MetricsHistory::default();
    for peer in 1..=6 {
        let peer = Some(Fingerprint::from_bytes([peer; 32]));
        let mut metrics = window(now, 1000);
        metrics.display_query = LatencySummary {
            count: 1,
            max_us: 120_000,
            over_100ms: 1,
            ..Default::default()
        };
        history.record(peer, metrics);
    }
    assert_eq!(history.incidents().count(), 4);
    assert!(
        history
            .incidents()
            .all(|incident| incident.stages() == ["display_query"])
    );
    let last = Some(Fingerprint::from_bytes([6; 32]));
    history.remove_peer(last);
    assert!(history.recent(now).all(|entry| entry.peer != last));
    assert!(history.incidents().all(|entry| entry.peer != last));
    history.record(last, window(now, 2000));
    assert_eq!(
        history
            .recent(now)
            .filter(|entry| entry.peer == last)
            .count(),
        1
    );
    assert!(history.incidents().all(|entry| entry.peer != last));
    history.clear();
    assert_eq!(history.recent(now).count(), 0);
    assert_eq!(history.incidents().count(), 0);
}

#[test]
fn invalid_duplicate_and_reversed_windows_do_not_contaminate_context() {
    let now = Instant::now();
    let mut history = MetricsHistory::default();
    history.record(None, InputMetrics::default());
    history.record(None, window(now + Duration::from_secs(60), 120_000));
    history.record(None, window(now - Duration::from_secs(61), 120_000));
    history.record(None, window(now, 1000));
    history.record(None, window(now, 120_000));
    history.record(None, window(now - Duration::from_secs(1), 120_000));
    assert_eq!(history.recent(now).count(), 1);
    assert_eq!(history.incidents().count(), 0);
}

use std::time::Duration;

use ferry_engine::{LatencyHistogram, LatencySummary};

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

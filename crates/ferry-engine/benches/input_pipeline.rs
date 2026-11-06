//! 本机CPU开销实验，不是网络或输入到显示延迟。cargo bench -p ferry-engine --bench input_pipeline
use ferry_engine::{InputSettings, LatencyHistogram, input_queue};
use ferry_proto::Message;
use ferry_types::{InputEvent, Point};
use std::hint::black_box;
use std::time::{Duration, Instant};

fn main() {
    const EVENTS: usize = 100_000;
    let mut histogram = LatencyHistogram::default();
    let started = Instant::now();
    for n in 0..EVENTS {
        histogram.record(black_box(Duration::from_micros((n % 100_000) as u64)));
    }
    let elapsed = started.elapsed();
    assert_eq!(histogram.take().count, EVENTS as u64);
    println!(
        "local histogram: {:.1} ns/sample",
        elapsed.as_nanos() as f64 / EVENTS as f64
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for coalesce_motion in [false, true] {
        let (tx, mut rx) = input_queue(128, InputSettings { coalesce_motion });
        let started = Instant::now();
        runtime.block_on(async {
            for chunk in 0..EVENTS / 64 {
                for i in 0..64 {
                    tx.try_send(
                        Message::Input(InputEvent::PointerMotion {
                            position: Point::new((chunk * 64 + i) as i32, 100),
                        }),
                        true,
                    )
                    .unwrap();
                }
                let count = if coalesce_motion { 1 } else { 64 };
                for _ in 0..count {
                    black_box(rx.recv().await.unwrap());
                }
            }
        });
        let elapsed = started.elapsed();
        let stats = tx.summary();
        println!(
            "local queue coalesce={coalesce_motion}: {:.1} ns/input, high_water={}, merged={}",
            elapsed.as_nanos() as f64 / (EVENTS / 64 * 64) as f64,
            stats.high_water,
            stats.coalesced
        );
        assert_eq!(stats.depth, 0);
        assert!(stats.high_water <= 64);
    }
}

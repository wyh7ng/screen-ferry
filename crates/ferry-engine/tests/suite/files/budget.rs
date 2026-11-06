use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_engine::FileRateLimit;

#[tokio::test(start_paused = true)]
async fn concurrent_senders_share_one_fifo_budget() {
    let budget = Arc::new(FileRateLimit::new(100));
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for id in 0..3 {
        let (budget, order) = (budget.clone(), order.clone());
        tasks.push(tokio::spawn(async move {
            budget.acquire(100).await;
            order.lock().unwrap().push(id);
        }));
        tokio::task::yield_now().await;
    }
    tokio::time::advance(Duration::from_millis(999)).await;
    tokio::task::yield_now().await;
    assert!(order.lock().unwrap().is_empty());
    for id in 0..3 {
        tokio::time::advance(if id == 0 {
            Duration::from_millis(1)
        } else {
            Duration::from_secs(1)
        })
        .await;
        tokio::task::yield_now().await;
        assert_eq!(*order.lock().unwrap(), (0..=id).collect::<Vec<_>>());
    }
    for task in tasks {
        task.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn live_rate_changes_and_cancelled_waiters_take_effect_immediately() {
    let budget = Arc::new(FileRateLimit::new(1));
    let first = tokio::spawn({
        let budget = budget.clone();
        async move {
            budget.acquire(1000).await;
        }
    });
    tokio::task::yield_now().await;
    let second = tokio::spawn({
        let budget = budget.clone();
        async move {
            budget.acquire(1).await;
        }
    });
    tokio::task::yield_now().await;
    first.abort();
    first.await.unwrap_err();
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    second.await.unwrap();
    let pending = tokio::spawn({
        let budget = budget.clone();
        async move {
            budget.acquire(1000).await;
        }
    });
    tokio::task::yield_now().await;
    budget.set(0);
    tokio::task::yield_now().await;
    assert!(pending.is_finished());
    pending.await.unwrap();
    assert_eq!(budget.get(), 0);
}

#[tokio::test(start_paused = true)]
async fn increasing_the_rate_accounts_for_bytes_already_waited() {
    let budget = Arc::new(FileRateLimit::new(100));
    let pending = tokio::spawn({
        let budget = budget.clone();
        async move {
            budget.acquire(100).await;
        }
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(500)).await;
    budget.set(200);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_millis(249)).await;
    tokio::task::yield_now().await;
    assert!(!pending.is_finished());
    tokio::time::advance(Duration::from_millis(1)).await;
    pending.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn dropping_to_one_byte_per_second_does_not_strand_a_large_chunk() {
    let budget = Arc::new(FileRateLimit::new(1_000_000));
    let pending = tokio::spawn({
        let budget = budget.clone();
        async move { budget.acquire_chunk(65536).await }
    });
    tokio::task::yield_now().await;
    budget.set(1);
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(pending.await.unwrap(), 1);
}

#[tokio::test]
async fn cancelling_a_rate_limited_file_does_not_wait_for_its_byte_budget() {
    use super::{collect, reached, send_when_ready, write};
    use crate::support::{Device, Rig, Setup, TempDir, eventually};
    use ferry_engine::{Files, TransferState};
    use ferry_platform_mock::MockFileCommit;
    let pc = Device::new();
    let (events, seen) = collect();
    let files = Files::new(&pc.store, events.clone(), Arc::new(MockFileCommit));
    files.set_rate_limit(1);
    let rig = Rig::with(Setup {
        pc,
        pc_files: Some(files),
        pc_events: events,
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let path = source.path().join("slow.txt");
    write(&path, &[1; 1000]);
    let id = send_when_ready(&rig.pc_files, vec![path]).await;
    eventually("开始等待发送额度", || {
        reached(&seen, |state| *state == TransferState::Running)
    })
    .await;
    rig.pc_files.cancel(id);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if reached(&seen, |state| {
                matches!(state, TransferState::Cancelled { .. })
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("取消不能等待1000秒的文件额度");
    assert!(!rig.mac.store.downloads_dir().join("slow.txt").exists());
}

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ferry_engine::{Events, Files, TransferState};
use ferry_platform::FileCommit;
use ferry_platform_mock::MockFileCommit;
use ferry_proto::Message;
use ferry_types::TransferId;

use super::{WirePeer, collect, reached, received};
use crate::support::{Device, Rig, Setup, eventually};

/// 改名已经成功，但紧接着的目录同步失败；意图仍为 pending，模拟掉电窗口。
struct InterruptCommit {
    downloads: PathBuf,
    armed: AtomicBool,
    cross_device: bool,
    before_publish: bool,
}

impl FileCommit for InterruptCommit {
    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        if self.cross_device && !from.starts_with(&self.downloads) {
            return Err(io::ErrorKind::CrossesDevices.into());
        }
        MockFileCommit.rename_no_replace(from, to)
    }
    fn identity(&self, path: &Path) -> io::Result<String> {
        MockFileCommit.identity(path)
    }
    fn identity_open_file(&self, file: &fs::File) -> io::Result<String> {
        MockFileCommit.identity_open_file(file)
    }
    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()> {
        MockFileCommit.replace_record(from, to)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        let at_fault = if self.before_publish {
            path.parent() == Some(self.downloads.as_path()) && path.join("item").exists()
        } else {
            path == self.downloads && self.downloads.join("wire.txt").exists()
        };
        if at_fault && self.armed.swap(false, Ordering::SeqCst) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        MockFileCommit.sync_directory(path)
    }
}

async fn interrupted(cross_device: bool, before_publish: bool) -> (Rig, WirePeer, TransferId) {
    let mac = Device::new();
    let (events, seen) = collect();
    let files = Files::new(
        &mac.store,
        events.clone(),
        Arc::new(InterruptCommit {
            downloads: mac.store.downloads_dir(),
            armed: AtomicBool::new(true),
            cross_device,
            before_publish,
        }),
    );
    let rig = Rig::with(Setup {
        mac,
        start_client: false,
        mac_files: Some(files),
        mac_events: events,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([91; 16]);
    peer.offer(id, 4).await;
    let (mut file, _) = peer.file(id, 4).await;
    file.send(&Message::FileData {
        data: b"data".to_vec(),
    })
    .await
    .unwrap();
    let sha256 = ring::digest::digest(&ring::digest::SHA256, b"data")
        .as_ref()
        .try_into()
        .unwrap();
    file.send(&Message::FileEnd(ferry_proto::FileEnd { index: 0, sha256 }))
        .await
        .unwrap();
    eventually("改名后目录同步失败", || {
        reached(&seen, |s| matches!(s, TransferState::Failed { .. }))
    })
    .await;
    // 清掉明确失败通知，下一轮模拟发送方未收到失败，按同一任务恢复。
    assert!(matches!(
        peer.data.recv().await.unwrap(),
        Some(Message::FileCancel(_))
    ));
    if !before_publish {
        assert_eq!(
            fs::read(received(&rig.mac).join("wire.txt")).unwrap(),
            b"data"
        );
    }
    (rig, peer, id)
}

#[tokio::test]
async fn an_unrecorded_rename_is_recovered_without_publishing_a_second_copy() {
    for cross in [false, true] {
        let (rig, peer, id) = interrupted(cross, false).await;
        let work = rig
            .mac
            .store
            .partial_dir()
            .join(peer.fingerprint.to_hex())
            .join(id.to_string());
        assert!(work.join("commit-intent.json").exists());
        let (tls, fingerprint) = (peer.tls.clone(), peer.fingerprint);
        drop(peer);
        let (mac, pc) = rig.stop();
        // 新引擎、新 Files 实例，只保留磁盘目录和配对；不能依赖原任务的内存状态。
        let rig = Rig::with(Setup {
            mac,
            pc,
            start_client: false,
            ..Setup::default()
        })
        .await;
        let mut peer = WirePeer::using(tls, fingerprint, &rig).await;
        peer.offer(id, 4).await;
        let (mut file, resume) = peer.file(id, 4).await;
        assert_eq!(resume.index, 1);
        assert!(matches!(
            file.recv().await.unwrap(),
            Some(Message::FileDone)
        ));
        assert!(!received(&rig.mac).join("wire (2).txt").exists());
        assert!(work.join("committed.json").exists());
        assert!(!work.join("commit-intent.json").exists());
    }
}

#[tokio::test]
async fn recovery_never_overwrites_or_deletes_a_user_modified_target() {
    let (rig, mut peer, id) = interrupted(false, false).await;
    fs::write(received(&rig.mac).join("wire.txt"), b"USER").unwrap();
    peer.offer(id, 4).await;
    use ferry_proto::{Capabilities, Channel, Hello};
    let (mut file, _) = peer.tls.connect(peer.addr, Channel::File).await.unwrap();
    ferry_net::initiate(
        &mut file,
        Hello::new(
            Channel::File,
            Capabilities::FILES,
            peer.session,
            "wire-peer",
        ),
    )
    .await
    .unwrap();
    file.send(&Message::FileOpen { id }).await.unwrap();
    file.send(&Message::FileList(ferry_proto::FileList {
        entries: vec![ferry_proto::FileEntry {
            path: "wire.txt".into(),
            kind: ferry_proto::EntryKind::File,
            size: 4,
            modified: 0,
        }],
        last: true,
    }))
    .await
    .unwrap();
    match file.recv().await {
        Ok(None) => {}
        Err(ferry_net::Error::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof => {}
        other => panic!("冲突不能要求重新发送内容：{other:?}"),
    }
    assert_eq!(
        fs::read(received(&rig.mac).join("wire.txt")).unwrap(),
        b"USER"
    );
    assert!(!received(&rig.mac).join("wire (2).txt").exists());
}

#[tokio::test]
async fn interrupted_cross_volume_copy_reuses_its_recorded_private_container() {
    let (rig, peer, id) = interrupted(true, true).await;
    let downloads = rig.mac.store.downloads_dir();
    assert!(!downloads.join("wire.txt").exists());
    let (tls, fingerprint) = (peer.tls.clone(), peer.fingerprint);
    drop(peer);
    let (mac, pc) = rig.stop();
    let files = Files::new(
        &mac.store,
        Events::none(),
        Arc::new(InterruptCommit {
            downloads: downloads.clone(),
            armed: AtomicBool::new(false),
            cross_device: true,
            before_publish: false,
        }),
    );
    let rig = Rig::with(Setup {
        mac,
        pc,
        start_client: false,
        mac_files: Some(files),
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::using(tls, fingerprint, &rig).await;
    peer.offer(id, 4).await;
    let (mut file, resume) = peer.file(id, 4).await;
    assert_eq!(resume.index, 1);
    assert!(matches!(
        file.recv().await.unwrap(),
        Some(Message::FileDone)
    ));
    assert_eq!(fs::read(downloads.join("wire.txt")).unwrap(), b"data");
    assert_eq!(
        fs::read_dir(downloads).unwrap().count(),
        1,
        "不得遗留复制中的私有目录"
    );
}

#[test]
fn receipt_cleanup_is_bounded_and_preserves_partial_and_unresolved_intents() {
    let device = Device::new();
    let files = Files::new(&device.store, Events::none(), Arc::new(MockFileCommit));
    // 在进入运行时前构造，确保没有启动清理和这次显式清理争抢人工缓存。
    let peer = "ab".repeat(32);
    let root = device.store.partial_dir().join(&peer);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    for n in 0..1030u32 {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        let work = root.join(TransferId(bytes).to_string());
        fs::create_dir_all(&work).unwrap();
        let completed = if n == 0 { now - 8 * 24 * 3600 } else { now };
        fs::write(work.join("committed.json"), serde_json::to_vec(&serde_json::json!({"version":1,"manifest":"test","saved":[],"completed":completed})).unwrap()).unwrap();
        if n == 1 {
            fs::create_dir(work.join("data")).unwrap();
        }
        if n == 2 {
            fs::write(work.join("commit-intent.json"), b"pending").unwrap();
        }
    }
    // 1028可清理候选含一份过期项，只留1024份；另两份有partial/intent，均不能误删。
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let removed = runtime.block_on(files.prune_receipts()).unwrap();
    assert_eq!(removed, 4);
    for n in [1u32, 2] {
        let mut bytes = [0u8; 16];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        assert!(
            root.join(TransferId(bytes).to_string())
                .join("committed.json")
                .exists()
        );
    }
    assert_eq!(fs::read_dir(root).unwrap().count(), 1026);
}

#[tokio::test]
async fn receipt_cleanup_never_removes_an_active_transfer_record() {
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([93; 16]);
    peer.offer(id, 4).await;
    let work = rig
        .mac
        .store
        .partial_dir()
        .join(peer.fingerprint.to_hex())
        .join(id.to_string());
    fs::create_dir_all(&work).unwrap();
    let receipt = work.join("committed.json");
    fs::write(
        &receipt,
        br#"{"version":1,"manifest":"test","saved":[],"completed":0}"#,
    )
    .unwrap();
    assert_eq!(rig.mac_files.prune_receipts().await.unwrap(), 0);
    assert!(receipt.exists());
}

#[derive(Clone, Copy)]
enum CleanupOutcome {
    Delete,
    Fail,
    Panic,
    Replaced,
}

struct CleanupGate {
    target: Option<PathBuf>,
    calls: usize,
    blocked: bool,
    released: bool,
    timed_out: bool,
}

struct SlowCleanup {
    state: Mutex<CleanupGate>,
    changed: Condvar,
    outcome: CleanupOutcome,
}

impl SlowCleanup {
    fn new(outcome: CleanupOutcome) -> Self {
        Self {
            state: Mutex::new(CleanupGate {
                target: None,
                calls: 0,
                blocked: false,
                released: false,
                timed_out: false,
            }),
            changed: Condvar::new(),
            outcome,
        }
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_all();
    }

    fn blocked(&self) -> bool {
        self.state.lock().unwrap().blocked
    }
}

impl FileCommit for SlowCleanup {
    fn identity(&self, path: &Path) -> io::Result<String> {
        let mut state = self.state.lock().unwrap();
        if state.target.as_deref() == Some(path) {
            state.calls += 1;
            if state.calls == 2 {
                // 第一次扫描候选；第二次已保留该任务，在磁盘重查期间阻塞。
                state.blocked = true;
                let (mut state, wait) = self
                    .changed
                    .wait_timeout_while(state, Duration::from_secs(8), |s| !s.released)
                    .unwrap();
                state.timed_out = wait.timed_out();
                state.blocked = false;
                drop(state);
                match self.outcome {
                    CleanupOutcome::Delete | CleanupOutcome::Replaced => {}
                    CleanupOutcome::Fail => return Err(io::ErrorKind::PermissionDenied.into()),
                    CleanupOutcome::Panic => panic!("injected cleanup worker panic"),
                }
                return MockFileCommit.identity(path);
            }
        }
        drop(state);
        MockFileCommit.identity(path)
    }

    fn identity_open_file(&self, file: &fs::File) -> io::Result<String> {
        MockFileCommit.identity_open_file(file)
    }
    fn rename_no_replace(&self, from: &Path, to: &Path) -> io::Result<()> {
        MockFileCommit.rename_no_replace(from, to)
    }
    fn replace_record(&self, from: &Path, to: &Path) -> io::Result<()> {
        MockFileCommit.replace_record(from, to)
    }
    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        MockFileCommit.sync_directory(path)
    }
}

struct ReleaseCleanup(Arc<SlowCleanup>);
impl Drop for ReleaseCleanup {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn cleanup_overlap(outcome: CleanupOutcome, abort_caller: bool) {
    let mac = Device::new();
    let (events, mut incoming) = Events::channel();
    let platform = Arc::new(SlowCleanup::new(outcome));
    // 不调度隐式启动清理：本测试控制何时进入磁盘阶段。
    let files = Files::new(&mac.store, events.clone(), platform.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _release = ReleaseCleanup(platform.clone());
        let seen = Arc::new(Mutex::new(Vec::new()));
        tokio::spawn({
            let seen = seen.clone();
            async move {
                while let Some(event) = incoming.recv().await {
                    seen.lock().unwrap().push(event);
                }
            }
        });
        let rig = Rig::with(Setup {
            mac,
            start_client: false,
            mac_files: Some(files.clone()),
            mac_events: events,
            ..Setup::default()
        }).await;
        let mut peer = WirePeer::connect(&rig).await;
        let id = TransferId([94; 16]);
        let work = rig.mac.store.partial_dir().join(peer.fingerprint.to_hex()).join(id.to_string());
        fs::create_dir_all(&work).unwrap();
        let receipt = work.join("committed.json");
        fs::write(&receipt, br#"{"version":1,"manifest":"expired","saved":[],"completed":0}"#).unwrap();
        platform.state.lock().unwrap().target = Some(work.clone());
        let cleanup = tokio::spawn({
            let files = files.clone();
            async move { files.prune_receipts().await }
        });
        eventually("清理进入锁外慢磁盘操作", || platform.blocked()).await;
        assert!(files.connected().contains(&peer.fingerprint), "共享状态锁不等待磁盘");
        peer.data.send(&Message::FileOffer(ferry_proto::FileOffer {
            id,
            name: "wire.txt".into(),
            items: 1,
            files: 1,
            bytes: 4,
        })).await.unwrap();
        eventually("同一任务只登记等待清理", || seen.lock().unwrap().iter().any(|event| {
            matches!(event, ferry_engine::Event::Transfer(update) if update.id == id && update.state == TransferState::Waiting)
        })).await;
        // 即使对端在Accept前抢先打开内容连接，也不能越过目录保留开始恢复。
        let (mut premature, _) = peer.tls.connect(peer.addr, ferry_proto::Channel::File).await.unwrap();
        ferry_net::initiate(&mut premature, ferry_proto::Hello::new(
            ferry_proto::Channel::File,
            ferry_proto::Capabilities::FILES,
            peer.session,
            "wire-peer",
        )).await.unwrap();
        premature.send(&Message::FileOpen { id }).await.unwrap();
        match tokio::time::timeout(Duration::from_secs(1), premature.recv()).await.unwrap() {
            Ok(None) | Err(ferry_net::Error::Closed) => {}
            Err(ferry_net::Error::Io(error)) if matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset) => {}
            other => panic!("清理期间提前打开的文件连接必须关闭：{other:?}"),
        }
        // 不相关任务仍可获取同一State锁并立即接受。
        let other = TransferId([95; 16]);
        assert!(matches!(peer.offer(other, 1).await, Message::FileAccept { id: got } if got == other));
        assert!(receipt.exists(), "等待中的任务不能读取或覆盖待清理回执");

        let before = seen.lock().unwrap().iter().filter(|event| matches!(event, ferry_engine::Event::Rtt { .. })).count();
        eventually("慢清理期间心跳持续响应", || seen.lock().unwrap().iter().filter(|event| matches!(event, ferry_engine::Event::Rtt { .. })).count() > before).await;
        assert!(platform.blocked(), "回归测试必须在磁盘仍阻塞时验证进展");
        assert!(!platform.state.lock().unwrap().timed_out);
        assert!(tokio::time::timeout(Duration::from_millis(100), peer.data.recv()).await.is_err(), "清理中不得提前接受同一任务");

        if matches!(outcome, CleanupOutcome::Replaced) {
            // 扫描后原子替换回执；即使完成时刻仍为同一秒，也不能清理新对象。
            let temporary = work.join("replacement.tmp");
            fs::write(&temporary, br#"{"version":1,"manifest":"refreshed","saved":[],"completed":0}"#).unwrap();
            MockFileCommit.replace_record(&temporary, &receipt).unwrap();
        }

        if abort_caller {
            cleanup.abort();
            assert!(cleanup.await.unwrap_err().is_cancelled());
            assert_eq!(files.prune_receipts().await.unwrap(), 0, "调用方取消不提前释放真实工作线程名额");
            assert!(platform.blocked());
            platform.release();
        } else {
            platform.release();
            let result = cleanup.await.unwrap();
            match outcome {
                CleanupOutcome::Delete => assert_eq!(result.unwrap(), 1),
                CleanupOutcome::Fail | CleanupOutcome::Replaced => assert_eq!(result.unwrap(), 0),
                CleanupOutcome::Panic => assert!(result.is_err()),
            }
        }
        let response = tokio::time::timeout(Duration::from_secs(3), peer.data.recv()).await.unwrap().unwrap();
        assert!(matches!(response, Some(Message::FileAccept { id: got }) if got == id), "清理结束或失败后原等待任务自动继续");

        if matches!(outcome, CleanupOutcome::Delete) {
            assert!(!receipt.exists());
            let (mut file, resume) = peer.file(id, 4).await;
            assert_eq!((resume.index, resume.offset), (0, 0));
            file.send(&Message::FileData { data: b"done".to_vec() }).await.unwrap();
            let sha256 = ring::digest::digest(&ring::digest::SHA256, b"done").as_ref().try_into().unwrap();
            file.send(&Message::FileEnd(ferry_proto::FileEnd { index: 0, sha256 })).await.unwrap();
            assert!(matches!(file.recv().await.unwrap(), Some(Message::FileDone)));
            assert_eq!(fs::read(received(&rig.mac).join("wire.txt")).unwrap(), b"done");
        } else {
            assert!(receipt.exists(), "清理失败保留原回执");
        }
    });
}

#[test]
fn slow_receipt_cleanup_does_not_hold_runtime_state_and_cancel_keeps_reservation() {
    cleanup_overlap(CleanupOutcome::Delete, true);
}

#[test]
fn failed_receipt_cleanup_releases_waiting_offer_without_deleting_receipt() {
    cleanup_overlap(CleanupOutcome::Fail, false);
}

#[test]
fn panicked_receipt_cleanup_releases_waiting_offer_without_poisoning_state() {
    cleanup_overlap(CleanupOutcome::Panic, false);
}

#[test]
fn receipt_replaced_after_scan_is_preserved_even_with_the_same_completion_second() {
    cleanup_overlap(CleanupOutcome::Replaced, false);
}

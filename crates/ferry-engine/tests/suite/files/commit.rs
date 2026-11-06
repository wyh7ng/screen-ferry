use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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

#[tokio::test]
async fn receipt_cleanup_is_bounded_and_preserves_partial_and_unresolved_intents() {
    let device = Device::new();
    let files = Files::new(&device.store, Events::none(), Arc::new(MockFileCommit));
    // 等启动清理结束后准备人工缓存，避免两次清理互相抢占。
    tokio::task::yield_now().await;
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
    let mut removed = 0;
    for _ in 0..20 {
        removed += files.prune_receipts().await.unwrap();
        if removed != 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
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

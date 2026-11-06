use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_engine::{Error, Event, Events, Files, TransferState, TransferUpdate};
use ferry_types::TransferId;
use tokio::time::sleep;

use crate::support::{Device, Rig, Setup, TempDir, eventually};

mod budget;
mod commit;
mod manifest;

/// 把文件传输事件收集到一个列表里。
fn collect() -> (Events, Arc<Mutex<Vec<TransferUpdate>>>) {
    let (events, mut rx) = Events::channel();
    let seen = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn({
        let seen = seen.clone();
        async move {
            while let Some(event) = rx.recv().await {
                if let Event::Transfer(update) = event {
                    seen.lock().unwrap().push(update);
                }
            }
        }
    });
    (events, seen)
}

fn reached(seen: &Mutex<Vec<TransferUpdate>>, pred: impl Fn(&TransferState) -> bool) -> bool {
    seen.lock().unwrap().iter().any(|u| pred(&u.state))
}

/// 连上之后再发：刚启动时数据连接可能还没建好。
async fn send_when_ready(files: &Files, paths: Vec<PathBuf>) -> TransferId {
    for _ in 0..300 {
        let Some(peer) = files.connected().first().copied() else {
            sleep(Duration::from_millis(10)).await;
            continue;
        };
        match files.send(peer, paths.clone()).await {
            Ok(id) => return id,
            Err(Error::NotConnected) => sleep(Duration::from_millis(10)).await,
            Err(e) => panic!("发送失败：{e}"),
        }
    }
    panic!("等待超时：数据连接没有建好");
}

fn received(device: &Device) -> PathBuf {
    device.store.downloads_dir()
}

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// 每个字节都不一样的内容，错位、截断都能看出来。
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

#[tokio::test]
async fn a_file_goes_from_the_client_to_the_server() {
    let (mac_events, mac) = collect();
    let (pc_events, pc) = collect();
    let rig = Rig::with(Setup {
        mac_events,
        pc_events,
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let file = source.path().join("季度报告.pdf");
    // 跨过好几个分块，最后一块不满。
    let content = pattern(200_000);
    write(&file, &content);

    send_when_ready(&rig.pc_files, vec![file]).await;

    let target = received(&rig.mac).join("季度报告.pdf");
    eventually("服务端收到文件", || target.is_file()).await;
    assert_eq!(fs::read(&target).unwrap(), content);
    eventually("两边都报告完成", || {
        reached(&pc, |s| matches!(s, TransferState::Done { .. }))
            && reached(
                &mac,
                |s| matches!(s, TransferState::Done { saved } if saved == &vec![target.clone()]),
            )
    })
    .await;
    let partial = rig.mac.store.partial_dir();
    eventually("临时文件已清理", || !has_payload(&partial)).await;
}

#[tokio::test]
async fn a_folder_goes_from_the_server_to_the_client() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let root = source.path().join("设计稿");
    write(&root.join("首页.png"), &pattern(70_000));
    write(&root.join("空文件.txt"), b"");
    // Windows 本身不允许文件名里有冒号，那边造不出这个源文件；清洗规则另有单元测试。
    let colon = std::env::consts::OS != "windows";
    if colon {
        write(&root.join("图标/a:b.svg"), b"<svg/>");
    }
    write(&root.join("图标/深/层/说明.md"), "层层叠叠".as_bytes());
    write(&root.join(".DS_Store"), b"junk");
    fs::create_dir_all(root.join("空文件夹")).unwrap();
    let lone = source.path().join("readme.txt");
    write(&lone, b"hi");

    send_when_ready(&rig.mac_files, vec![root, lone]).await;

    let got = received(&rig.pc);
    eventually("客户端收到所有项目", || {
        got.join("readme.txt").is_file() && got.join("设计稿/首页.png").is_file()
    })
    .await;
    let folder = got.join("设计稿");
    assert_eq!(fs::read(folder.join("首页.png")).unwrap(), pattern(70_000));
    assert_eq!(fs::read(folder.join("空文件.txt")).unwrap(), b"");
    if colon {
        assert_eq!(
            fs::read(folder.join("图标/a_b.svg")).unwrap(),
            b"<svg/>",
            "冒号在 Windows 上不能用"
        );
    }
    assert_eq!(
        fs::read_to_string(folder.join("图标/深/层/说明.md")).unwrap(),
        "层层叠叠"
    );
    assert!(folder.join("空文件夹").is_dir(), "空文件夹也带过去");
    assert!(!folder.join(".DS_Store").exists(), "系统生成的文件不发送");
}

#[tokio::test]
async fn the_same_name_twice_gets_a_number() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let file = source.path().join("证书.p12");
    write(&file, b"first");

    send_when_ready(&rig.pc_files, vec![file.clone()]).await;
    let got = received(&rig.mac);
    eventually("第一份", || got.join("证书.p12").is_file()).await;

    write(&file, b"second");
    send_when_ready(&rig.pc_files, vec![file]).await;
    eventually("第二份自动编号", || {
        got.join("证书 (2).p12").is_file()
    })
    .await;
    assert_eq!(fs::read(got.join("证书.p12")).unwrap(), b"first");
    assert_eq!(fs::read(got.join("证书 (2).p12")).unwrap(), b"second");
}

#[tokio::test]
async fn cancelling_stops_both_sides_and_leaves_nothing_behind() {
    let (mac_events, mac) = collect();
    let (pc_events, pc) = collect();
    let rig = Rig::with(Setup {
        mac_events,
        pc_events,
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let file = source.path().join("发布录屏.mov");
    write(&file, &pattern(8 << 20));

    let id = send_when_ready(&rig.pc_files, vec![file]).await;
    rig.pc_files.cancel(id);

    eventually("发送方报告已取消", || {
        reached(&pc, |s| {
            matches!(s, TransferState::Cancelled { by_peer: false })
        })
    })
    .await;
    // 对方可能还没开始收，也可能收到一半；都不能留下文件。
    sleep(Duration::from_millis(300)).await;
    assert!(!received(&rig.mac).join("发布录屏.mov").exists());
    assert!(!reached(&mac, |s| matches!(s, TransferState::Done { .. })));
    let partial = rig
        .mac
        .store
        .partial_dir()
        .join(rig.pc.store.identity("pc").unwrap().fingerprint().to_hex())
        .join(id.to_string());
    eventually("接收方删掉了收到一半的部分", || {
        !partial.exists()
    })
    .await;
}

#[tokio::test]
async fn sending_without_a_connection_says_so() {
    let device = Device::new();
    let files = Files::new(
        &device.store,
        Events::none(),
        Arc::new(ferry_platform_mock::MockFileCommit),
    );
    let source = TempDir::new();
    let file = source.path().join("a.txt");
    write(&file, b"a");
    assert!(matches!(
        files
            .send("00".repeat(32).parse().unwrap(), vec![file])
            .await,
        Err(Error::NotConnected)
    ));
}

#[tokio::test]
async fn an_interrupted_transfer_resumes_where_it_stopped() {
    use ferry_proto::Message;
    let (events, seen) = collect();
    let rig = Rig::with(Setup {
        start_client: false,
        mac_events: events,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([85; 16]);
    let content = pattern(200_000);
    peer.offer(id, content.len() as u64).await;
    let (mut file, resume) = peer.file(id, content.len() as u64).await;
    assert_eq!((resume.index, resume.offset), (0, 0));
    file.send(&Message::FileData {
        data: content[..40_000].to_vec(),
    })
    .await
    .unwrap();
    drop(file);
    eventually("接收方处理文件连接中断", || {
        reached(&seen, |s| *s == TransferState::Interrupted)
    })
    .await;
    let tls = peer.tls.clone();
    let fingerprint = peer.fingerprint;
    drop(peer); // 连输入、数据连接一起关闭，再用同一证书建立新会话。
    let mut peer = WirePeer::using(tls, fingerprint, &rig).await;
    peer.offer(id, content.len() as u64).await;
    let (mut file, resume) = peer.file(id, content.len() as u64).await;
    assert_eq!((resume.index, resume.offset), (0, 40_000));
    for part in content[40_000..].chunks(ferry_proto::limits::MAX_CHUNK_LEN) {
        file.send(&Message::FileData {
            data: part.to_vec(),
        })
        .await
        .unwrap();
    }
    let sha256 = ring::digest::digest(&ring::digest::SHA256, &content)
        .as_ref()
        .try_into()
        .unwrap();
    file.send(&Message::FileEnd(ferry_proto::FileEnd { index: 0, sha256 }))
        .await
        .unwrap();
    assert!(matches!(
        file.recv().await.unwrap(),
        Some(Message::FileDone)
    ));
    assert_eq!(
        fs::read(received(&rig.mac).join("wire.txt")).unwrap(),
        content
    );
}

fn has_payload(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry.file_name() == "data" || (entry.path().is_dir() && has_payload(&entry.path()))
        })
    })
}

#[tokio::test]
async fn simultaneous_same_names_keep_every_file() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let a = source.path().join("a/report.txt");
    let b = source.path().join("b/report.txt");
    write(&a, b"first");
    write(&b, b"second");
    let (first, second) = tokio::join!(
        send_when_ready(&rig.pc_files, vec![a]),
        send_when_ready(&rig.pc_files, vec![b])
    );
    assert_ne!(first, second);
    eventually("两份同名文件都留下", || {
        received(&rig.mac).join("report (2).txt").exists()
    })
    .await;
    let mut contents = [
        fs::read(received(&rig.mac).join("report.txt")).unwrap(),
        fs::read(received(&rig.mac).join("report (2).txt")).unwrap(),
    ];
    contents.sort();
    assert_eq!(contents, [b"first".to_vec(), b"second".to_vec()]);
}

#[tokio::test]
async fn scanning_failure_does_not_expose_the_source_path() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let seed = source.path().join("seed");
    write(&seed, b"ok");
    send_when_ready(&rig.pc_files, vec![seed]).await;
    let missing = source.path().join("private-personal-name.txt");
    let peer = rig.pc_files.connected()[0];
    let error = rig
        .pc_files
        .send(peer, vec![missing])
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("private-personal-name"));
    assert!(!error.contains(&source.path().to_string_lossy().to_string()));
}

/// 手写的已配对客户端：在真实 TLS 线上控制消息时序，复现普通客户端难以稳定制造的断线。
struct WirePeer {
    tls: ferry_net::Tls,
    addr: std::net::SocketAddr,
    session: u64,
    data: ferry_net::Framed<ferry_net::ClientStream>,
    input: tokio::task::JoinHandle<()>,
    fingerprint: ferry_trust::Fingerprint,
}

impl Drop for WirePeer {
    fn drop(&mut self) {
        self.input.abort();
    }
}

impl WirePeer {
    async fn connect(rig: &Rig) -> Self {
        let identity = ferry_trust::Identity::generate("wire-peer").unwrap();
        let fingerprint = identity.fingerprint();
        let mut peers = rig.mac.store.peers().unwrap();
        peers.add(ferry_trust::Peer::new("wire-peer", fingerprint));
        rig.mac.store.save_peers(&peers).unwrap();
        let tls = ferry_net::Tls::new(&identity).unwrap();
        Self::using(tls, fingerprint, rig).await
    }

    async fn using(tls: ferry_net::Tls, fingerprint: ferry_trust::Fingerprint, rig: &Rig) -> Self {
        use ferry_proto::{Capabilities, Channel, Hello, Message};
        let (mut input, _) = tls.connect(rig.addr, Channel::Input).await.unwrap();
        let welcome = ferry_net::initiate(
            &mut input,
            Hello::new(Channel::Input, Capabilities::FILES, 0, "wire-peer"),
        )
        .await
        .unwrap();
        assert!(matches!(
            input.recv().await.unwrap(),
            Some(Message::Displays(_))
        ));
        input
            .send(&Message::Displays(vec![crate::support::display(
                100, 100, 100,
            )]))
            .await
            .unwrap();
        let input = tokio::spawn(async move {
            while let Ok(Some(message)) = input.recv().await {
                if let Message::Ping { nonce } = message {
                    let _ = input.send(&Message::Pong { nonce }).await;
                }
            }
        });
        let data = loop {
            let (mut data, _) = tls.connect(rig.addr, Channel::Data).await.unwrap();
            if ferry_net::initiate(
                &mut data,
                Hello::new(
                    Channel::Data,
                    Capabilities::FILES,
                    welcome.session,
                    "wire-peer",
                ),
            )
            .await
            .is_ok()
            {
                break data;
            }
            sleep(Duration::from_millis(10)).await;
        };
        Self {
            tls,
            addr: rig.addr,
            session: welcome.session,
            data,
            input,
            fingerprint,
        }
    }

    async fn offer(&mut self, id: TransferId, size: u64) -> ferry_proto::Message {
        self.data
            .send(&ferry_proto::Message::FileOffer(ferry_proto::FileOffer {
                id,
                name: "wire.txt".into(),
                items: 1,
                files: 1,
                bytes: size,
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), self.data.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    }

    async fn file(
        &self,
        id: TransferId,
        size: u64,
    ) -> (
        ferry_net::Framed<ferry_net::ClientStream>,
        ferry_proto::FileResume,
    ) {
        use ferry_proto::{Capabilities, Channel, Hello, Message};
        let (mut file, _) = self.tls.connect(self.addr, Channel::File).await.unwrap();
        ferry_net::initiate(
            &mut file,
            Hello::new(
                Channel::File,
                Capabilities::FILES,
                self.session,
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
                size,
                modified: 0,
            }],
            last: true,
        }))
        .await
        .unwrap();
        let Some(Message::FileResume(resume)) = file.recv().await.unwrap() else {
            panic!("expected resume")
        };
        (file, resume)
    }
}

#[tokio::test]
async fn duplicate_offer_repeats_accept_without_starting_another_job() {
    use ferry_proto::Message;
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([81; 16]);
    assert!(matches!(peer.offer(id, 4).await, Message::FileAccept { id: got } if got == id));
    assert!(matches!(peer.offer(id, 4).await, Message::FileAccept { id: got } if got == id));
    let (_, resume) = peer.file(id, 4).await;
    assert_eq!(resume.offset, 0);
}

#[tokio::test]
async fn a_completed_transfer_only_replays_confirmation_after_the_ack_is_lost() {
    use ferry_proto::Message;
    let (events, seen) = collect();
    let rig = Rig::with(Setup {
        start_client: false,
        mac_events: events,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([82; 16]);
    peer.offer(id, 4).await;
    let (mut file, resume) = peer.file(id, 4).await;
    assert_eq!(resume.index, 0);
    file.send(&Message::FileData {
        data: b"data".to_vec(),
    })
    .await
    .unwrap();
    let hash: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, b"data")
        .as_ref()
        .try_into()
        .unwrap();
    file.send(&Message::FileEnd(ferry_proto::FileEnd {
        index: 0,
        sha256: hash,
    }))
    .await
    .unwrap();
    eventually("接收方完成提交", || {
        reached(&seen, |s| matches!(s, TransferState::Done { .. }))
    })
    .await;
    // 不读取 FileDone，模拟确认没有到达发送方。
    drop(file);
    peer.offer(id, 4).await;
    let (mut replay, resume) = peer.file(id, 4).await;
    assert_eq!(resume.index, 1, "重放完成不能重新接收内容");
    assert!(matches!(
        replay.recv().await.unwrap(),
        Some(Message::FileDone)
    ));
    assert_eq!(
        fs::read(received(&rig.mac).join("wire.txt")).unwrap(),
        b"data"
    );
    assert!(!received(&rig.mac).join("wire (2).txt").exists());
}

#[tokio::test]
async fn cancellation_after_file_eof_still_removes_partial_data() {
    use ferry_proto::{CancelReason, FileCancel, Message};
    let (events, seen) = collect();
    let rig = Rig::with(Setup {
        start_client: false,
        mac_events: events,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    let id = TransferId([83; 16]);
    peer.offer(id, 100_000).await;
    let (mut file, _) = peer.file(id, 100_000).await;
    file.send(&Message::FileData {
        data: vec![1; 1000],
    })
    .await
    .unwrap();
    drop(file);
    eventually("先收到文件连接中断", || {
        reached(&seen, |s| *s == TransferState::Interrupted)
    })
    .await;
    let partial = rig
        .mac
        .store
        .partial_dir()
        .join(peer.fingerprint.to_hex())
        .join(id.to_string());
    assert!(partial.exists());
    peer.data
        .send(&Message::FileCancel(FileCancel {
            id,
            reason: CancelReason::CANCELLED,
        }))
        .await
        .unwrap();
    eventually("迟到的取消删除临时内容", || !partial.exists()).await;
    assert!(!received(&rig.mac).join("wire.txt").exists());
}

struct CrossDeviceCommit {
    downloads: PathBuf,
}

impl ferry_platform::FileCommit for CrossDeviceCommit {
    fn identity(&self, path: &Path) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity(&ferry_platform_mock::MockFileCommit, path)
    }
    fn identity_open_file(&self, file: &fs::File) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity_open_file(&ferry_platform_mock::MockFileCommit, file)
    }
    fn replace_record(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::replace_record(&ferry_platform_mock::MockFileCommit, from, to)
    }
    fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::sync_directory(&ferry_platform_mock::MockFileCommit, path)
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if !from.starts_with(&self.downloads) {
            return Err(std::io::Error::from(std::io::ErrorKind::CrossesDevices));
        }
        ferry_platform::FileCommit::rename_no_replace(
            &ferry_platform_mock::MockFileCommit,
            from,
            to,
        )
    }
}

#[tokio::test]
async fn cross_device_staging_preserves_existing_files_and_cleans_up() {
    let mac = Device::new();
    let (events, seen) = collect();
    let downloads = mac.store.downloads_dir();
    write(&downloads.join("report.txt"), b"existing");
    let files = Files::new(
        &mac.store,
        events.clone(),
        Arc::new(CrossDeviceCommit {
            downloads: downloads.clone(),
        }),
    );
    let rig = Rig::with(Setup {
        mac,
        mac_events: events,
        mac_files: Some(files),
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let path = source.path().join("report.txt");
    write(&path, b"new-content");
    send_when_ready(&rig.pc_files, vec![path]).await;
    eventually("跨盘提交完成", || {
        reached(&seen, |s| matches!(s, TransferState::Done { .. }))
    })
    .await;
    assert_eq!(fs::read(downloads.join("report.txt")).unwrap(), b"existing");
    assert_eq!(
        fs::read(downloads.join("report (2).txt")).unwrap(),
        b"new-content"
    );
    assert!(!fs::read_dir(downloads).unwrap().flatten().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .starts_with(".screenferry-")
    }));
}

struct PausedCommit {
    entered: Arc<std::sync::atomic::AtomicBool>,
    released: Arc<std::sync::atomic::AtomicBool>,
}

impl ferry_platform::FileCommit for PausedCommit {
    fn identity(&self, path: &Path) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity(&ferry_platform_mock::MockFileCommit, path)
    }
    fn identity_open_file(&self, file: &fs::File) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity_open_file(&ferry_platform_mock::MockFileCommit, file)
    }
    fn replace_record(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::replace_record(&ferry_platform_mock::MockFileCommit, from, to)
    }
    fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::sync_directory(&ferry_platform_mock::MockFileCommit, path)
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        self.entered.store(true, Ordering::Release);
        while !self.released.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        ferry_platform::FileCommit::rename_no_replace(
            &ferry_platform_mock::MockFileCommit,
            from,
            to,
        )
    }
}

#[tokio::test]
async fn cancellation_cannot_undo_an_atomic_commit_that_has_already_succeeded() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mac = Device::new();
    let (events, seen) = collect();
    let entered = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let files = Files::new(
        &mac.store,
        events.clone(),
        Arc::new(PausedCommit {
            entered: entered.clone(),
            released: released.clone(),
        }),
    );
    let rig = Rig::with(Setup {
        mac,
        mac_events: events,
        mac_files: Some(files),
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let path = source.path().join("report.txt");
    write(&path, b"will-be-cancelled");
    let id = send_when_ready(&rig.pc_files, vec![path]).await;
    eventually("工作线程进入提交", || {
        entered.load(Ordering::Acquire)
    })
    .await;
    rig.mac_files.cancel(id);
    assert!(
        !reached(&seen, |s| matches!(s, TransferState::Cancelled { .. })),
        "提交线程还没退出，不能先声称临时文件已删除"
    );
    released.store(true, Ordering::Release);
    eventually("取消后的工作线程结束并清理", || {
        !has_payload(&rig.mac.store.partial_dir())
    })
    .await;
    eventually("原子提交成功后报告完成", || {
        reached(&seen, |s| matches!(s, TransferState::Done { .. }))
    })
    .await;
    assert_eq!(
        fs::read(received(&rig.mac).join("report.txt")).unwrap(),
        b"will-be-cancelled"
    );
    assert!(!reached(&seen, |s| matches!(
        s,
        TransferState::Cancelled { .. }
    )));
}

#[tokio::test]
async fn a_failed_file_connection_retries_while_input_and_data_stay_connected() {
    use ferry_engine::{ClientConfig, ClipLimit, EdgeFeed, ServerAddr};
    use ferry_platform_mock::{MockClipboard, MockDisplays, MockFileCommit, MockInject};
    use ferry_proto::{Capabilities, Channel, FileEntry, FileList, FileOffer, Hello, Message};
    use ferry_trust::{Identity, Peer, PeerList};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = Identity::generate("retry-server").unwrap();
    let tls = ferry_net::Tls::new(&server).unwrap();
    let device = Device::new();
    device
        .store
        .save_peers(&PeerList::from_iter([Peer::new(
            "retry-server",
            server.fingerprint(),
        )]))
        .unwrap();
    let (events, seen) = collect();
    let files = Files::new(&device.store, events.clone(), Arc::new(MockFileCommit));
    let client = tokio::spawn(ferry_engine::connect(
        ClientConfig {
            server: ServerAddr::Fixed(addr),
            name: "retry-client".into(),
            clipboard: false,
            clipboard_limit: ClipLimit::default(),
            command_key: false,
            store: device.store.clone(),
            events,
            files,
            edge: EdgeFeed::none(),
            devices: Vec::new(),
            commands: None,
            invite_port: None,
        },
        MockInject::new(),
        MockDisplays::new(vec![crate::support::display(100, 100, 100)]),
        MockClipboard::new(),
        crate::support::AutoConfirm::answering(true),
    ));
    let (stream, _) = listener.accept().await.unwrap();
    let (mut input, _) = tls.accept(stream).await.unwrap();
    let (hello, _) = ferry_net::accept(&mut input, Capabilities::FILES, "retry-server", |_| Ok(7))
        .await
        .unwrap();
    assert_eq!(hello.channel, Channel::Input);
    input
        .send(&Message::Displays(vec![crate::support::display(
            100, 100, 100,
        )]))
        .await
        .unwrap();
    assert!(matches!(
        input.recv().await.unwrap(),
        Some(Message::Displays(_))
    ));
    let input_task = tokio::spawn(async move {
        while let Ok(Some(message)) = input.recv().await {
            if let Message::Ping { nonce } = message {
                let _ = input.send(&Message::Pong { nonce }).await;
            }
        }
    });
    let (stream, _) = listener.accept().await.unwrap();
    let (mut data, _) = tls.accept(stream).await.unwrap();
    let (hello, _) = ferry_net::accept(&mut data, Capabilities::FILES, "retry-server", |_| Ok(7))
        .await
        .unwrap();
    assert_eq!(hello.channel, Channel::Data);
    let id = TransferId([84; 16]);
    data.send(&Message::FileOffer(FileOffer {
        id,
        name: "retry.txt".into(),
        items: 1,
        files: 1,
        bytes: 4,
    }))
    .await
    .unwrap();
    assert!(matches!(
        data.recv().await.unwrap(),
        Some(Message::FileAccept { .. })
    ));
    for attempt in 0..2 {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        if attempt == 0 {
            // TLS 尚未建立就断开：输入和数据连接仍然保持。
            drop(stream);
            continue;
        }
        let (mut file, _) = tls.accept(stream).await.unwrap();
        let (hello, _) = ferry_net::accept(
            &mut file,
            Capabilities::FILES,
            "retry-server",
            |_: &Hello| Ok(7),
        )
        .await
        .unwrap();
        assert_eq!(hello.channel, Channel::File, "不能靠重建输入连接恢复");
        assert!(matches!(
            file.recv().await.unwrap(),
            Some(Message::FileOpen { .. })
        ));
        file.send(&Message::FileList(FileList {
            entries: vec![FileEntry {
                path: "retry.txt".into(),
                kind: ferry_proto::EntryKind::File,
                size: 4,
                modified: 0,
            }],
            last: true,
        }))
        .await
        .unwrap();
        assert!(matches!(
            file.recv().await.unwrap(),
            Some(Message::FileResume(_))
        ));
        file.send(&Message::FileData {
            data: b"done".to_vec(),
        })
        .await
        .unwrap();
        let hash: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, b"done")
            .as_ref()
            .try_into()
            .unwrap();
        file.send(&Message::FileEnd(ferry_proto::FileEnd {
            index: 0,
            sha256: hash,
        }))
        .await
        .unwrap();
        assert!(matches!(
            file.recv().await.unwrap(),
            Some(Message::FileDone)
        ));
    }
    eventually("文件自动重试完成", || {
        reached(&seen, |s| matches!(s, TransferState::Done { .. }))
    })
    .await;
    assert_eq!(
        fs::read(device.store.downloads_dir().join("retry.txt")).unwrap(),
        b"done"
    );
    client.abort();
    input_task.abort();
}

#[tokio::test]
async fn incoming_transfer_count_is_bounded_per_peer() {
    use ferry_proto::Message;
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    let mut peer = WirePeer::connect(&rig).await;
    for n in 1..=8 {
        let id = TransferId([n; 16]);
        assert!(matches!(
            peer.offer(id, 1).await,
            Message::FileAccept { .. }
        ));
    }
    assert!(matches!(
        peer.offer(TransferId([9; 16]), 1).await,
        Message::FileCancel(_)
    ));
}

struct FailAfterFirstCommit {
    calls: std::sync::atomic::AtomicUsize,
    first: PathBuf,
}

impl ferry_platform::FileCommit for FailAfterFirstCommit {
    fn identity(&self, path: &Path) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity(&ferry_platform_mock::MockFileCommit, path)
    }
    fn identity_open_file(&self, file: &fs::File) -> std::io::Result<String> {
        ferry_platform::FileCommit::identity_open_file(&ferry_platform_mock::MockFileCommit, file)
    }
    fn replace_record(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::replace_record(&ferry_platform_mock::MockFileCommit, from, to)
    }
    fn sync_directory(&self, path: &Path) -> std::io::Result<()> {
        ferry_platform::FileCommit::sync_directory(&ferry_platform_mock::MockFileCommit, path)
    }

    fn rename_no_replace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
            // 第一个已公开文件可能已被用户编辑，失败回滚不得删除它。
            fs::write(&self.first, b"user-edited")?;
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        ferry_platform::FileCommit::rename_no_replace(
            &ferry_platform_mock::MockFileCommit,
            from,
            to,
        )
    }
}

#[tokio::test]
async fn a_later_commit_failure_never_removes_an_already_published_file() {
    let mac = Device::new();
    let (events, seen) = collect();
    let first = mac.store.downloads_dir().join("first.txt");
    let files = Files::new(
        &mac.store,
        events.clone(),
        Arc::new(FailAfterFirstCommit {
            calls: Default::default(),
            first: first.clone(),
        }),
    );
    let rig = Rig::with(Setup {
        mac,
        mac_events: events,
        mac_files: Some(files),
        ..Setup::default()
    })
    .await;
    let source = TempDir::new();
    let a = source.path().join("first.txt");
    let b = source.path().join("second.txt");
    write(&a, b"original");
    write(&b, b"second");
    send_when_ready(&rig.pc_files, vec![a, b]).await;
    eventually("明确报告部分保存失败", || {
        reached(
            &seen,
            |s| matches!(s, TransferState::Failed { reason } if reason.contains("部分文件已保存")),
        )
    })
    .await;
    assert_eq!(fs::read(first).unwrap(), b"user-edited");
    assert!(!received(&rig.mac).join("second.txt").exists());
}

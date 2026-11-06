use std::time::Duration;

use ferry_engine::{ClipLimit, Event};
use ferry_platform_mock::MockClipboard;
use ferry_types::{ClipboardContent, ClipboardFormat};
use tokio::time::sleep;

use crate::support::{Rig, Setup, collect, eventually, fake_png, has};

/// 比轮询间隔（250 ms）长得多，足够让不该发生的同步发生。
const QUIET: Duration = Duration::from_millis(700);

fn text(t: &str) -> ClipboardContent {
    ClipboardContent::text(t)
}

#[tokio::test]
async fn copying_on_the_mac_reaches_the_pc_when_the_cursor_goes_there() {
    let rig = Rig::start().await;
    rig.mac_clipboard.copy(text("来自 Mac"));

    sleep(QUIET).await;
    assert_eq!(
        rig.pc_clipboard.writes(),
        0,
        "光标没过去，不该写入对方剪贴板"
    );

    rig.enter().await;
    eventually("PC 收到文本", || {
        rig.pc_clipboard.content() == text("来自 Mac")
    })
    .await;
}

#[tokio::test]
async fn copying_on_the_pc_reaches_the_mac_when_the_cursor_returns() {
    let rig = Rig::start().await;
    rig.enter().await;
    rig.pc_clipboard.copy(text("来自 PC"));

    rig.leave().await;
    eventually("Mac 收到文本", || {
        rig.mac_clipboard.content() == text("来自 PC")
    })
    .await;
}

#[tokio::test]
async fn only_the_latest_copy_is_transferred() {
    let rig = Rig::start().await;
    for i in 0..3 {
        rig.mac_clipboard.copy(text(&format!("第 {i} 次")));
        sleep(Duration::from_millis(300)).await;
    }

    rig.enter().await;
    eventually("PC 收到最后一次", || {
        rig.pc_clipboard.content() == text("第 2 次")
    })
    .await;
    sleep(QUIET).await;
    assert_eq!(rig.pc_clipboard.writes(), 1);
}

#[tokio::test]
async fn synced_content_is_not_echoed_back() {
    let rig = Rig::start().await;
    rig.mac_clipboard.copy(text("回声"));
    rig.enter().await;
    eventually("PC 收到文本", || rig.pc_clipboard.writes() == 1).await;

    rig.leave().await;
    sleep(QUIET).await;
    rig.enter().await;
    sleep(QUIET).await;

    assert_eq!(rig.mac_clipboard.writes(), 0, "Mac 不该收到自己的内容");
    assert_eq!(rig.pc_clipboard.writes(), 1, "同一份内容只写一次");
}

#[tokio::test]
async fn connecting_does_not_overwrite_the_mac_with_old_pc_content() {
    let mac = MockClipboard::new();
    let pc = MockClipboard::new();
    mac.copy(text("Mac 上的"));
    pc.copy(text("PC 上旧的"));
    let rig = Rig::with_clipboards(mac, pc).await;

    sleep(QUIET).await;
    rig.enter().await;
    eventually("PC 收到 Mac 的内容", || {
        rig.pc_clipboard.content() == text("Mac 上的")
    })
    .await;
    rig.leave().await;
    sleep(QUIET).await;

    assert_eq!(rig.mac_clipboard.content(), text("Mac 上的"));
    assert_eq!(rig.mac_clipboard.writes(), 0);
}

#[tokio::test]
async fn crossing_right_after_connecting_still_keeps_each_side_right() {
    // 数据连接比输入连接晚一点建立，这期间光标就已经切过去了。
    let mac = MockClipboard::new();
    let pc = MockClipboard::new();
    mac.copy(text("Mac 上的"));
    pc.copy(text("PC 上旧的"));
    let rig = Rig::with_clipboards(mac, pc).await;

    rig.enter().await;
    eventually("PC 收到 Mac 的内容", || {
        rig.pc_clipboard.content() == text("Mac 上的")
    })
    .await;
    rig.leave().await;
    sleep(QUIET).await;
    assert_eq!(rig.mac_clipboard.writes(), 0, "PC 的旧内容不能覆盖 Mac");
}

#[tokio::test]
async fn large_text_arrives_in_many_chunks() {
    let rig = Rig::start().await;
    // 约 600 KB，需要十来个 64 KiB 分块。
    let big = "剪贴板分块测试。\n".repeat(25_000);
    rig.mac_clipboard.copy(text(&big));

    rig.enter().await;
    eventually("PC 收到完整文本", || rig.pc_clipboard.writes() == 1).await;
    assert_eq!(rig.pc_clipboard.content().as_text(), Some(big.as_str()));
}

#[tokio::test]
async fn copying_something_unsupported_clears_the_pending_offer() {
    let rig = Rig::start().await;
    rig.mac_clipboard.copy(text("旧文本"));
    sleep(Duration::from_millis(300)).await;
    // 比如复制了一张图片：本机读不出任何支持的格式。
    rig.mac_clipboard.copy(ClipboardContent::new());
    sleep(Duration::from_millis(300)).await;

    rig.enter().await;
    sleep(QUIET).await;
    assert_eq!(rig.pc_clipboard.writes(), 0, "不该粘贴出之前的旧文本");
}

/// 一份带格式、带图片的内容，像从网页或文档里复制出来的。
fn rich(png_padding: usize) -> ClipboardContent {
    let mut content = text("会议纪要");
    content.insert(ClipboardFormat::HTML, "<b>会议纪要</b>".as_bytes().to_vec());
    content.insert(ClipboardFormat::RTF, br"{\rtf1 x}".to_vec());
    content.insert(ClipboardFormat::PNG, fake_png(640, 480, png_padding));
    content
}

#[tokio::test]
async fn images_and_rich_text_reach_the_other_side() {
    let (mac_events, mac) = collect();
    let rig = Rig::with(Setup {
        mac_events,
        ..Setup::default()
    })
    .await;
    // 约 300 KB 的图片，要分好几块。
    rig.mac_clipboard.copy(rich(300_000));

    rig.enter().await;
    eventually("PC 收到全部格式", || {
        rig.pc_clipboard.content() == rich(300_000)
    })
    .await;
    eventually("Mac 报告图片被拉走了", || {
        has(&mac, |e| {
            matches!(e, Event::Clipboard { to_peer: true, summary, .. }
                if summary.rich && summary.image.as_ref().is_some_and(|i| i.width == 640))
        })
    })
    .await;
}

#[tokio::test]
async fn a_format_over_the_senders_limit_is_skipped_and_reported() {
    let (mac_events, mac) = collect();
    let rig = Rig::with(Setup {
        mac_events,
        mac_clip_limit: ClipLimit::new(100 * 1024),
        ..Setup::default()
    })
    .await;
    rig.mac_clipboard.copy(rich(300_000));

    rig.enter().await;
    eventually("PC 收到文字和格式", || {
        rig.pc_clipboard.writes() == 1
    })
    .await;
    let got = rig.pc_clipboard.content();
    assert_eq!(got.as_text(), Some("会议纪要"));
    assert!(got.get(ClipboardFormat::HTML).is_some());
    assert_eq!(
        got.get(ClipboardFormat::PNG),
        None,
        "图片超过上限，没有发出去"
    );
    assert!(has(&mac, |e| matches!(
        e,
        Event::ClipboardSkipped {
            to_peer: true,
            format: ClipboardFormat::PNG,
            limit: 102_400,
            ..
        }
    )));
}

#[tokio::test]
async fn the_receiver_skips_what_is_over_its_own_limit() {
    let (pc_events, pc) = collect();
    let rig = Rig::with(Setup {
        pc_events,
        pc_clip_limit: ClipLimit::new(100 * 1024),
        ..Setup::default()
    })
    .await;
    rig.mac_clipboard.copy(rich(300_000));

    rig.enter().await;
    eventually("PC 收到文字", || rig.pc_clipboard.writes() == 1).await;
    assert_eq!(rig.pc_clipboard.content().get(ClipboardFormat::PNG), None);
    assert!(has(&pc, |e| matches!(
        e,
        Event::ClipboardSkipped {
            to_peer: false,
            format: ClipboardFormat::PNG,
            ..
        }
    )));
}

#[tokio::test]
async fn raising_the_limit_takes_effect_without_reconnecting() {
    let limit = ClipLimit::new(100 * 1024);
    let rig = Rig::with(Setup {
        mac_clip_limit: limit.clone(),
        ..Setup::default()
    })
    .await;
    rig.mac_clipboard.copy(rich(300_000));
    rig.enter().await;
    eventually("先收到没有图片的那份", || {
        rig.pc_clipboard.writes() == 1
    })
    .await;
    rig.leave().await;

    limit.set(1024 * 1024);
    rig.mac_clipboard.copy(rich(300_001));
    rig.enter().await;
    eventually("调大上限后图片也过来了", || {
        rig.pc_clipboard.content() == rich(300_001)
    })
    .await;
}

struct PausedWriteClipboard {
    inner: MockClipboard,
    started: Option<tokio::sync::oneshot::Sender<()>>,
    resume: std::sync::mpsc::Receiver<()>,
    timed_out: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ferry_platform::Clipboard for PausedWriteClipboard {
    fn formats(&self) -> &'static [ClipboardFormat] {
        ferry_platform::Clipboard::formats(&self.inner)
    }
    fn change_count(&mut self) -> u64 {
        ferry_platform::Clipboard::change_count(&mut self.inner)
    }
    fn read(&mut self) -> ferry_platform::Result<ClipboardContent> {
        ferry_platform::Clipboard::read(&mut self.inner)
    }
    fn write(&mut self, content: &ClipboardContent) -> ferry_platform::Result<u64> {
        if let Some(started) = self.started.take() {
            let _ = started.send(());
            if self.resume.recv_timeout(Duration::from_secs(3)).is_err() {
                self.timed_out
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        ferry_platform::Clipboard::write(&mut self.inner, content)
    }
}

#[tokio::test]
async fn a_blocked_native_clipboard_write_does_not_block_input_or_timers() {
    use ferry_engine::connect;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let rig = Rig::with(Setup {
        start_client: false,
        ..Setup::default()
    })
    .await;
    rig.mac_clipboard.copy(text("native write is slow"));
    let config = crate::client::paired_config(&rig, rig.addr);
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (resume, blocked) = std::sync::mpsc::channel();
    let timed_out = Arc::new(AtomicBool::new(false));
    let clipboard = PausedWriteClipboard {
        inner: rig.pc_clipboard.clone(),
        started: Some(started),
        resume: blocked,
        timed_out: timed_out.clone(),
    };
    let client = tokio::spawn(connect(
        config,
        rig.inject.clone(),
        rig.pc_displays.clone(),
        clipboard,
        rig.pc_confirm.clone(),
    ));
    rig.enter().await;
    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !timed_out.load(Ordering::SeqCst),
        "系统剪贴板阻塞期间运行时仍能被调度"
    );
    let before = rig.inject.events().len();
    rig.push(10, 0);
    eventually("剪贴板调用未结束时输入继续注入", || {
        rig.inject.events().len() > before
    })
    .await;
    sleep(Duration::from_millis(20)).await;
    assert!(!timed_out.load(Ordering::SeqCst));
    resume.send(()).unwrap();
    eventually("工作线程完成后写入剪贴板", || {
        rig.pc_clipboard.writes() > 0
    })
    .await;
    client.abort();
    rig.server.abort();
}

#[tokio::test]
async fn clipboard_messages_without_negotiated_permission_close_the_data_channel() {
    use ferry_engine::Live;
    use ferry_net::Tls;
    use ferry_proto::{Capabilities, Channel, ClipboardItem, ClipboardOffer, Hello, Message};
    use ferry_types::{DeviceSettings, Side};
    let live = Live {
        side: Side::Right,
        layout: Vec::new(),
        devices: Vec::new(),
        mouse_dpi: 1600,
        input: Default::default(),
        fallback: DeviceSettings {
            clipboard: false,
            ..DeviceSettings::default()
        },
    };
    let rig = Rig::with(Setup {
        start_client: false,
        live: Some(live.fixed()),
        ..Setup::default()
    })
    .await;
    let _ = crate::client::paired_config(&rig, rig.addr);
    let tls = Tls::new(&rig.pc.store.identity("pc").unwrap()).unwrap();
    let (mut input, _) = tls.connect(rig.addr, Channel::Input).await.unwrap();
    let caps = Capabilities::CLIPBOARD | Capabilities::FILES;
    let welcome = ferry_net::initiate(&mut input, Hello::new(Channel::Input, caps, 0, "pc"))
        .await
        .unwrap();
    assert!(!welcome.capabilities.contains(Capabilities::CLIPBOARD));
    input
        .send(&Message::Displays(vec![crate::support::display(
            1920, 1080, 100,
        )]))
        .await
        .unwrap();
    assert!(matches!(
        input.recv().await.unwrap(),
        Some(Message::Displays(_))
    ));
    sleep(Duration::from_millis(30)).await;
    let (mut data, _) = tls.connect(rig.addr, Channel::Data).await.unwrap();
    ferry_net::initiate(
        &mut data,
        Hello::new(Channel::Data, caps, welcome.session, "pc"),
    )
    .await
    .unwrap();
    data.send(&Message::ClipboardOffer(ClipboardOffer {
        seq: 1,
        items: vec![ClipboardItem {
            format: ClipboardFormat::TEXT,
            size: 5,
        }],
    }))
    .await
    .unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), data.recv())
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(None) | Err(_)),
        "未授权通告不能得到拉取请求：{closed:?}"
    );
    assert_eq!(rig.mac_clipboard.writes(), 0);
    rig.server.abort();
}

/// 真实 TLS 服务端，剪贴板可替换为故障注入实现。
struct ClipboardServer {
    device: crate::support::Device,
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<ferry_engine::Result<std::convert::Infallible>>,
}

impl ClipboardServer {
    async fn start<C: ferry_platform::Clipboard>(clipboard: C, limit: ClipLimit) -> Self {
        use ferry_engine::{EdgeFeed, Events, Files, Live, ServerConfig, serve};
        use ferry_platform_mock::{MockCapture, MockDisplays, MockFileCommit};
        use std::sync::Arc;
        let device = crate::support::Device::new();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let config = ServerConfig {
            live: Live {
                side: ferry_types::Side::Right,
                layout: vec![],
                devices: vec![],
                fallback: Default::default(),
                mouse_dpi: 1600,
                input: Default::default(),
            }
            .fixed(),
            name: "clipboard server".into(),
            clipboard: true,
            clipboard_limit: limit,
            command_key: true,
            store: device.store.clone(),
            announce: false,
            events: Events::none(),
            files: Files::new(&device.store, Events::none(), Arc::new(MockFileCommit)),
            edge: EdgeFeed::none(),
        };
        let task = tokio::spawn(serve(
            listener,
            config,
            MockCapture::new(),
            MockDisplays::new(vec![crate::support::display(1000, 800, 100)]),
            clipboard,
            crate::support::AutoConfirm::answering(true),
        ));
        Self { device, addr, task }
    }
}

impl Drop for ClipboardServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct ClipboardPeer {
    data: ferry_net::Framed<ferry_net::ClientStream>,
    input: tokio::task::JoinHandle<()>,
}

impl ClipboardPeer {
    async fn connect(server: &ClipboardServer) -> Self {
        use ferry_net::Tls;
        use ferry_proto::{Capabilities, Channel, Hello, Message};
        use ferry_trust::{Identity, Peer};
        let identity = Identity::generate("clipboard peer").unwrap();
        let mut peers = server.device.store.peers().unwrap();
        peers.add(Peer::new("clipboard peer", identity.fingerprint()));
        server.device.store.save_peers(&peers).unwrap();
        let tls = Tls::new(&identity).unwrap();
        let caps = Capabilities::CLIPBOARD | Capabilities::EDGE_DROP;
        let (mut input, _) = tls.connect(server.addr, Channel::Input).await.unwrap();
        let welcome = ferry_net::initiate(&mut input, Hello::new(Channel::Input, caps, 0, "peer"))
            .await
            .unwrap();
        input
            .send(&Message::Displays(vec![crate::support::display(
                1920, 1080, 100,
            )]))
            .await
            .unwrap();
        // PeerSide 在服务端注册会话之后发送，数据连接无需靠sleep猜时序。
        loop {
            match input.recv().await.unwrap().unwrap() {
                Message::PeerSide { .. } => break,
                Message::Ping { nonce } => input.send(&Message::Pong { nonce }).await.unwrap(),
                _ => {}
            }
        }
        let input = tokio::spawn(async move {
            while let Ok(Some(message)) = input.recv().await {
                if let Message::Ping { nonce } = message
                    && input.send(&Message::Pong { nonce }).await.is_err()
                {
                    break;
                }
            }
        });
        let (mut data, _) = tls.connect(server.addr, Channel::Data).await.unwrap();
        ferry_net::initiate(
            &mut data,
            Hello::new(Channel::Data, caps, welcome.session, "peer"),
        )
        .await
        .unwrap();
        Self { data, input }
    }

    async fn offer(&mut self) -> ferry_proto::ClipboardOffer {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(ferry_proto::Message::ClipboardOffer(offer)) =
                    self.data.recv().await.unwrap()
                {
                    return offer;
                }
            }
        })
        .await
        .unwrap()
    }

    async fn publish_text(&mut self, value: &str) {
        use ferry_proto::{Chunk, ClipboardItem, ClipboardOffer, Message};
        self.data
            .send(&Message::ClipboardOffer(ClipboardOffer {
                seq: 77,
                items: vec![ClipboardItem {
                    format: ClipboardFormat::TEXT,
                    size: value.len() as u64,
                }],
            }))
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Message::ClipboardRequest(request)) = self.data.recv().await.unwrap() {
                    return request;
                }
            }
        })
        .await
        .unwrap();
        self.data
            .send(&Message::Chunk(Chunk {
                transfer: request.transfer,
                last: true,
                data: value.as_bytes().to_vec(),
            }))
            .await
            .unwrap();
    }

    async fn fetch(
        &mut self,
        offer: &ferry_proto::ClipboardOffer,
        format: ClipboardFormat,
    ) -> Vec<u8> {
        use ferry_proto::{ClipboardRequest, Message};
        self.data
            .send(&Message::ClipboardRequest(ClipboardRequest {
                seq: offer.seq,
                format,
                transfer: 1,
            }))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            let mut bytes = Vec::new();
            loop {
                match self.data.recv().await.unwrap().unwrap() {
                    Message::Chunk(chunk) if chunk.transfer == 1 => {
                        bytes.extend_from_slice(&chunk.data);
                        if chunk.last {
                            return bytes;
                        }
                    }
                    Message::Cancel(_) => panic!("有效的剪贴板通告不应被取消"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap()
    }
}

impl Drop for ClipboardPeer {
    fn drop(&mut self) {
        self.input.abort();
    }
}

#[tokio::test]
async fn multiple_peers_read_each_clipboard_version_only_once() {
    let native = MockClipboard::new();
    native.copy(rich(300_000));
    let server = ClipboardServer::start(native.clone(), ClipLimit::default()).await;
    let mut first = ClipboardPeer::connect(&server).await;
    let a = first.offer().await;
    let mut second = ClipboardPeer::connect(&server).await;
    let b = second.offer().await;
    assert_eq!(native.reads(), 1, "两个TLS会话共享一次原生读取/转换");
    assert_eq!(
        first.fetch(&a, ClipboardFormat::PNG).await,
        fake_png(640, 480, 300_000)
    );
    assert_eq!(
        second.fetch(&b, ClipboardFormat::PNG).await,
        fake_png(640, 480, 300_000)
    );
    native.copy(text("next version"));
    let a = first.offer().await;
    let b = second.offer().await;
    assert_eq!(native.reads(), 2, "计数改变后只重新读取一次");
    assert_eq!(
        first.fetch(&a, ClipboardFormat::TEXT).await,
        b"next version"
    );
    assert_eq!(
        second.fetch(&b, ClipboardFormat::TEXT).await,
        b"next version"
    );
}

#[tokio::test]
async fn filtering_one_offer_does_not_remove_formats_from_the_shared_cache() {
    let native = MockClipboard::new();
    native.copy(rich(300_000));
    let limit = ClipLimit::new(100 * 1024);
    let server = ClipboardServer::start(native.clone(), limit.clone()).await;
    let mut first = ClipboardPeer::connect(&server).await;
    let a = first.offer().await;
    assert!(!a.items.iter().any(|i| i.format == ClipboardFormat::PNG));
    assert_eq!(
        first.fetch(&a, ClipboardFormat::HTML).await,
        "<b>会议纪要</b>".as_bytes()
    );
    limit.set(1024 * 1024);
    let mut second = ClipboardPeer::connect(&server).await;
    let b = second.offer().await;
    assert!(b.items.iter().any(|i| i.format == ClipboardFormat::PNG));
    assert_eq!(native.reads(), 1, "限额过滤不破坏原缓存，也不强制重新转换");
    assert_eq!(
        second.fetch(&b, ClipboardFormat::PNG).await,
        fake_png(640, 480, 300_000)
    );
    assert!(native.content().get(ClipboardFormat::PNG).is_some());
}

struct ChangingReadClipboard {
    inner: MockClipboard,
    change_once: bool,
    fail_once: bool,
}

impl ferry_platform::Clipboard for ChangingReadClipboard {
    fn formats(&self) -> &'static [ClipboardFormat] {
        ferry_platform::Clipboard::formats(&self.inner)
    }
    fn change_count(&mut self) -> u64 {
        ferry_platform::Clipboard::change_count(&mut self.inner)
    }
    fn read(&mut self) -> ferry_platform::Result<ClipboardContent> {
        let content = ferry_platform::Clipboard::read(&mut self.inner)?;
        if std::mem::take(&mut self.fail_once) {
            return Err(ferry_platform::Error::Failed(
                "temporary clipboard lock".into(),
            ));
        }
        if std::mem::take(&mut self.change_once) {
            self.inner.copy(text("stable version"));
        }
        Ok(content)
    }
    fn write(&mut self, content: &ClipboardContent) -> ferry_platform::Result<u64> {
        ferry_platform::Clipboard::write(&mut self.inner, content)
    }
}

#[tokio::test]
async fn a_clipboard_change_during_conversion_is_retried_before_caching() {
    let native = MockClipboard::new();
    native.copy(text("inconsistent old version"));
    let server = ClipboardServer::start(
        ChangingReadClipboard {
            inner: native.clone(),
            change_once: true,
            fail_once: false,
        },
        ClipLimit::default(),
    )
    .await;
    let mut first = ClipboardPeer::connect(&server).await;
    let a = first.offer().await;
    assert_eq!(
        first.fetch(&a, ClipboardFormat::TEXT).await,
        b"stable version"
    );
    let mut second = ClipboardPeer::connect(&server).await;
    let b = second.offer().await;
    assert_eq!(
        second.fetch(&b, ClipboardFormat::TEXT).await,
        b"stable version"
    );
    assert_eq!(native.reads(), 2, "读前后计数改变需要重读；稳定版本可复用");
}

#[tokio::test]
async fn a_failed_first_read_is_retried_without_publishing_empty_content() {
    let native = MockClipboard::new();
    native.copy(text("preserve this"));
    let server = ClipboardServer::start(
        ChangingReadClipboard {
            inner: native.clone(),
            change_once: false,
            fail_once: true,
        },
        ClipLimit::default(),
    )
    .await;
    let mut first = ClipboardPeer::connect(&server).await;
    let a = first.offer().await;
    assert!(!a.items.is_empty(), "读取失败不冒充空剪贴板通告");
    assert_eq!(
        first.fetch(&a, ClipboardFormat::TEXT).await,
        b"preserve this"
    );
    let mut second = ClipboardPeer::connect(&server).await;
    second.offer().await;
    assert_eq!(native.reads(), 2, "错误不缓存，成功后才共享");
}

#[tokio::test]
async fn an_unused_image_cache_releases_its_reference_without_another_copy() {
    use std::sync::Arc;
    let native = MockClipboard::new();
    let mut content = ClipboardContent::new();
    content.insert(ClipboardFormat::PNG, fake_png(64, 64, 1024));
    let image = content.get_shared(ClipboardFormat::PNG).unwrap();
    native.copy(content);
    let server = ClipboardServer::start(native.clone(), ClipLimit::default()).await;
    let mut peer = ClipboardPeer::connect(&server).await;
    let offer = peer.offer().await;
    peer.fetch(&offer, ClipboardFormat::PNG).await;
    drop(peer);
    eventually("仅原生内容和共享缓存还持有图片", || {
        Arc::strong_count(&image) == 3
    })
    .await;
    sleep(Duration::from_millis(6100)).await;
    eventually("五秒没有读取后释放缓存引用", || {
        Arc::strong_count(&image) == 2
    })
    .await;
}

#[tokio::test]
async fn aggregate_cache_size_is_bounded_even_for_large_content() {
    let native = MockClipboard::new();
    let mut content = ClipboardContent::new();
    content.insert(ClipboardFormat::TEXT, vec![b'x'; 64 * 1024 * 1024 + 1]);
    native.copy(content);
    let server = ClipboardServer::start(native.clone(), ClipLimit::default()).await;
    let mut first = ClipboardPeer::connect(&server).await;
    first.offer().await;
    let mut second = ClipboardPeer::connect(&server).await;
    second.offer().await;
    assert_eq!(native.reads(), 2, "超过64MiB的聚合内容不能留在共享缓存");
}

#[tokio::test]
async fn a_remote_write_invalidates_the_cached_native_content_for_other_peers() {
    let native = MockClipboard::new();
    native.copy(text("before remote write"));
    let server = ClipboardServer::start(native.clone(), ClipLimit::default()).await;
    let mut first = ClipboardPeer::connect(&server).await;
    first.offer().await;
    let mut second = ClipboardPeer::connect(&server).await;
    second.offer().await;
    assert_eq!(native.reads(), 1);
    first.publish_text("written remotely").await;
    eventually("收到的内容写入本机", || native.writes() == 1).await;
    let updated = second.offer().await;
    assert_eq!(
        second.fetch(&updated, ClipboardFormat::TEXT).await,
        b"written remotely"
    );
    assert_eq!(native.reads(), 2, "写入后新计数只读取一次，不沿用旧缓存");
}

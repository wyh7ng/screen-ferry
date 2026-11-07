use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ferry_engine::{EdgeDrop, Event, Files};
use ferry_platform::{DragReader, Overlay, OverlayEvent, OverlaySink};
use ferry_platform_mock::MockOverlay;
use ferry_types::{Frame, Lang, Scene};

use super::{ACCENT, left_button, pointer, source_file, texts};
use crate::support::{Device, Rig, Setup, TempDir, collect, eventually};

#[derive(Default)]
struct Gate {
    block: AtomicBool,
    entered: AtomicBool,
    timed_out: AtomicBool,
    counts: AtomicUsize,
    reads: AtomicUsize,
    empty_reads: AtomicUsize,
    released: Mutex<bool>,
    ready: Condvar,
}

impl Gate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.ready.notify_all();
    }
}

struct HeldGate(Arc<Gate>);
impl Drop for HeldGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct ControlledOverlay {
    inner: MockOverlay,
    gate: Arc<Gate>,
}

struct ControlledReader {
    inner: MockOverlay,
    gate: Arc<Gate>,
}

impl DragReader for ControlledReader {
    fn change_count(&mut self) -> i64 {
        self.gate.counts.fetch_add(1, Ordering::SeqCst);
        self.inner.change_count()
    }

    fn files(&mut self) -> Vec<PathBuf> {
        self.gate.reads.fetch_add(1, Ordering::SeqCst);
        if self
            .gate
            .empty_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Vec::new();
        }
        let result = self.inner.files();
        if self.gate.block.swap(false, Ordering::SeqCst) {
            self.gate.entered.store(true, Ordering::SeqCst);
            let (_released, timeout) = self
                .gate
                .ready
                .wait_timeout_while(
                    self.gate.released.lock().unwrap(),
                    Duration::from_secs(6),
                    |released| !*released,
                )
                .unwrap();
            self.gate
                .timed_out
                .store(timeout.timed_out(), Ordering::SeqCst);
        }
        result
    }
}

impl Overlay for ControlledOverlay {
    fn drag_reader(&self) -> Box<dyn DragReader> {
        Box::new(ControlledReader {
            inner: self.inner.clone(),
            gate: self.gate.clone(),
        })
    }
    fn start(&mut self, sink: OverlaySink) -> ferry_platform::Result<()> {
        self.inner.start(sink)
    }
    fn show(&mut self, frame: Frame, scale: f32, scene: Scene) {
        self.inner.show(frame, scale, scene);
    }
    fn hide(&mut self) {
        self.inner.hide();
    }
    fn drag_count(&self) -> i64 {
        panic!("核心线程不得同步查询拖放剪贴板")
    }
    fn dragged_files(&self) -> Vec<PathBuf> {
        panic!("核心线程不得同步读取拖放剪贴板")
    }
    fn offer(&mut self, files: Vec<PathBuf>) {
        self.inner.offer(files);
    }
    fn watch_edge(&mut self, strip: Option<Frame>) {
        self.inner.watch_edge(strip);
    }
    fn open(&self, path: &Path) {
        self.inner.open(path);
    }
    fn reveal(&self, path: &Path) {
        self.inner.reveal(path);
    }
}

struct DragRig {
    rig: Rig,
    overlay: MockOverlay,
    gate: Arc<Gate>,
    _release: HeldGate,
    _edge: EdgeDrop,
    events: Arc<Mutex<Vec<Event>>>,
}

impl DragRig {
    async fn start() -> Self {
        let device = Device::new();
        let (events, observed) = collect();
        let files = Files::new(
            &device.store,
            events.clone(),
            Arc::new(ferry_platform_mock::MockFileCommit),
        );
        let overlay = MockOverlay::new();
        let gate = Arc::new(Gate::default());
        let edge = EdgeDrop::spawn(
            ControlledOverlay {
                inner: overlay.clone(),
                gate: gate.clone(),
            },
            files.clone(),
            ACCENT,
            Lang::Zh,
        );
        let rig = Rig::with(Setup {
            mac: device,
            mac_files: Some(files),
            mac_events: events,
            edge: edge.feed(),
            ..Default::default()
        })
        .await;
        rig.enter().await;
        rig.leave().await;
        Self {
            rig,
            overlay,
            gate: gate.clone(),
            _release: HeldGate(gate),
            _edge: edge,
            events: observed,
        }
    }

    async fn begin(&self, files: Vec<PathBuf>, blocked: bool) {
        pointer(&self.rig, 500, 400);
        let previous = self.gate.counts.load(Ordering::SeqCst);
        left_button(&self.rig, true);
        eventually("异步记录拖动基线", || {
            self.gate.counts.load(Ordering::SeqCst) > previous
        })
        .await;
        self.gate.block.store(blocked, Ordering::SeqCst);
        self.overlay.start_drag(files);
        pointer(&self.rig, 900, 400);
        if blocked {
            eventually("读取线程被测试门闩阻塞", || {
                self.gate.entered.load(Ordering::SeqCst)
            })
            .await;
        }
    }

    fn rtts(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, Event::Rtt { .. }))
            .count()
    }
}

#[tokio::test]
async fn blocked_drag_preview_does_not_block_heartbeat_or_actual_drop() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let path = source_file(&temp, "slow-preview.txt", b"drop survives slow preview");
    test.begin(vec![path.clone()], true).await;
    let count = test.gate.counts.load(Ordering::SeqCst);
    let rtts = test.rtts();
    for i in 0..200 {
        pointer(&test.rig, 850 + i % 50, 400);
    }
    eventually("预览被阻塞时仍回应心跳", || test.rtts() > rtts).await;
    assert_eq!(
        test.gate.counts.load(Ordering::SeqCst),
        count,
        "在飞查询不再排队或并发读取"
    );
    test.overlay
        .emit(OverlayEvent::Dropped { paths: vec![path] });
    left_button(&test.rig, false);
    let saved = test.rig.pc.store.downloads_dir().join("slow-preview.txt");
    eventually("实际放下不用等待预览读取", || saved.is_file()).await;
    assert_eq!(std::fs::read(saved).unwrap(), b"drop survives slow preview");
    assert!(
        !test.gate.timed_out.load(Ordering::SeqCst),
        "读取仍由门闩阻塞，不能依靠超时放行"
    );
    test.gate.release();
}

#[tokio::test]
async fn old_drag_reply_cannot_replace_a_new_drag_or_resurrect_after_release() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let old = source_file(&temp, "old-drag.txt", b"old");
    let new = source_file(&temp, "new-drag.txt", b"new");
    test.begin(vec![old], true).await;
    left_button(&test.rig, false);
    eventually("旧拖动已结束", || test.overlay.watching().is_none()).await;
    left_button(&test.rig, true);
    eventually("新拖动已建立", || test.overlay.watching().is_some()).await;
    test.overlay.start_drag(vec![new]);
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    test.gate.release();
    eventually("只显示新拖动内容", || {
        texts(&test.overlay).contains(&"new-drag.txt".into())
    })
    .await;
    assert!(!texts(&test.overlay).contains(&"old-drag.txt".into()));
    left_button(&test.rig, false);
    eventually("松开后收起，不重放迟到结果", || {
        test.overlay.shown().is_none()
    })
    .await;
}

#[tokio::test]
async fn unchanged_drag_clipboard_is_polled_at_a_bounded_rate_and_stops_when_released() {
    let test = DragRig::start().await;
    pointer(&test.rig, 900, 400);
    left_button(&test.rig, true);
    eventually("建立异步基线", || {
        test.gate.counts.load(Ordering::SeqCst) >= 2
    })
    .await;
    let count = test.gate.counts.load(Ordering::SeqCst);
    for _ in 0..1000 {
        pointer(&test.rig, 901, 400);
    }
    tokio::time::sleep(Duration::from_millis(240)).await;
    let after = test.gate.counts.load(Ordering::SeqCst);
    assert!(after - count <= 8, "高频移动不应变成每事件剪贴板查询");
    assert_eq!(
        test.gate.reads.load(Ordering::SeqCst),
        0,
        "变化计数不变时不读取旧拖放内容"
    );
    left_button(&test.rig, false);
    eventually("结束拖动", || test.overlay.watching().is_none()).await;
    tokio::time::sleep(Duration::from_millis(70)).await;
    let idle = test.gate.counts.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(140)).await;
    assert_eq!(test.gate.counts.load(Ordering::SeqCst), idle, "空闲不轮询");
}

#[tokio::test]
async fn native_enter_recovers_a_pasteboard_already_changed_before_async_baseline() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let path = source_file(&temp, "early-drag.txt", b"first gesture");
    pointer(&test.rig, 900, 400);
    // 与旧测试相反：系统先准备拖放数据，后台才消费按下和异步基线。
    test.overlay.start_drag(vec![path.clone()]);
    left_button(&test.rig, true);
    eventually("异步基线与当前拖放计数一致", || {
        test.gate.counts.load(Ordering::SeqCst) >= 2
    })
    .await;
    assert_eq!(
        test.gate.reads.load(Ordering::SeqCst),
        0,
        "不凭旧计数猜测正在拖的内容"
    );
    assert!(
        test.overlay.watching().is_some(),
        "预览未知时仍存在原生接收条"
    );
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    eventually("原生进入强制取得当前文件", || {
        texts(&test.overlay).contains(&"early-drag.txt".into())
    })
    .await;
    assert!(
        test.overlay.watching().is_some(),
        "已知文件后接收条也不在动画前消失"
    );
    test.overlay
        .emit(OverlayEvent::Dropped { paths: vec![path] });
    left_button(&test.rig, false);
    eventually("第一轮拖放就发送", || {
        test.rig
            .pc
            .store
            .downloads_dir()
            .join("early-drag.txt")
            .is_file()
    })
    .await;
}

#[tokio::test]
async fn an_empty_drag_read_retries_without_waiting_for_another_gesture() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let path = source_file(&temp, "late-ready.txt", b"ready after empty");
    test.gate.empty_reads.store(1, Ordering::SeqCst);
    test.begin(vec![path], false).await;
    eventually("瞬时空读取后仍继续识别", || {
        texts(&test.overlay).contains(&"late-ready.txt".into())
            || test.gate.reads.load(Ordering::SeqCst) >= 2
    })
    .await;
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    eventually("同一次拖动显示文件", || {
        texts(&test.overlay).contains(&"late-ready.txt".into())
    })
    .await;
    assert!(test.gate.reads.load(Ordering::SeqCst) >= 2);
    left_button(&test.rig, false);
}

#[tokio::test]
async fn native_enter_before_pointer_watch_creates_current_drag() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let path = source_file(&temp, "native-first.txt", b"native enter is authoritative");
    pointer(&test.rig, 900, 400);
    test.overlay.start_drag(vec![path.clone()]);
    // 原生拖放事件不应要求watch通道已经观察到ButtonDown。
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    eventually("未消费按下也识别原生拖入", || {
        texts(&test.overlay).contains(&"native-first.txt".into())
    })
    .await;
    test.overlay
        .emit(OverlayEvent::Dropped { paths: vec![path] });
    eventually("原生路径仍发送成功", || {
        test.rig
            .pc
            .store
            .downloads_dir()
            .join("native-first.txt")
            .is_file()
    })
    .await;
}

#[tokio::test]
async fn button_up_keeps_native_drop_target_until_callback_but_never_sends_cached_files() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let cached = source_file(&temp, "cached.txt", b"preview only");
    let dropped = source_file(&temp, "actual.txt", b"native drop data");
    test.begin(vec![cached], false).await;
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    eventually("接收胶囊已显示", || {
        texts(&test.overlay).contains(&"松手发送到".into())
    })
    .await;
    left_button(&test.rig, false);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        test.overlay.watching().is_some(),
        "Up不能在原生Drop前撤掉接收条"
    );
    assert!(test.overlay.shown().is_some(), "Up不能在原生Drop前收起胶囊");
    assert!(
        !test
            .rig
            .pc
            .store
            .downloads_dir()
            .join("cached.txt")
            .exists(),
        "Up本身绝不发送预览缓存"
    );
    test.overlay.emit(OverlayEvent::DragExited);
    test.overlay.emit(OverlayEvent::Dropped {
        paths: vec![dropped],
    });
    eventually("迟于Up的Drop仍发送权威路径", || {
        test.rig
            .pc
            .store
            .downloads_dir()
            .join("actual.txt")
            .is_file()
    })
    .await;
    assert!(
        !test
            .rig
            .pc
            .store
            .downloads_dir()
            .join("cached.txt")
            .exists()
    );
}

#[tokio::test]
async fn release_without_native_drop_expires_without_sending_preview() {
    let test = DragRig::start().await;
    let temp = TempDir::new();
    let path = source_file(&temp, "cancelled-drag.txt", b"must not send");
    test.begin(vec![path], false).await;
    test.overlay.emit(OverlayEvent::DragEntered { files: true });
    eventually("当前文件已识别", || {
        texts(&test.overlay).contains(&"cancelled-drag.txt".into())
    })
    .await;
    left_button(&test.rig, false);
    eventually("没有Drop时宽限后收起", || {
        test.overlay.watching().is_none() && test.overlay.shown().is_none()
    })
    .await;
    assert!(
        !test
            .rig
            .pc
            .store
            .downloads_dir()
            .join("cancelled-drag.txt")
            .exists()
    );
}

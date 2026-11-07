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

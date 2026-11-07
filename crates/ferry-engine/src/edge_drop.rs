//! 边缘投递：拖着文件靠近朝向对方的屏幕边缘时，贴边滑出胶囊，放上去就发送；
//! 收发过程中胶囊显示进度，发送方完成后自动收起，接收方收完变成卡片。见 ADR 0005。
//!
//! 引擎经 [`EdgeFeed`] 报告光标和朝向对方的那条边；文件传输的进展由常驻后台转来。
//! 这里决定显示哪种样子，[`Capsule`] 算出动画中的每一帧，交给平台的 [`Overlay`] 画出来。
//!
//! 收到的卡片上可以直接把文件拖到任意文件夹；拖出去的过程中卡片一直留着，放下后收起。
//!
//! 服务端可能同时连着几台设备，每台占朝向它的那一段边（ADR 0006）：拖到哪一段就发给哪台，
//! 进度、提示和卡片也出现在那台设备那一段。
//!
//! 两端联动：本机拖着文件靠近边缘时，经引擎把提示（`DropHint`）发给对方；对方在相邻边上
//! 同一位置提前滑出接收胶囊，放下后接收进度也出现在那里。

mod drag_query;

use std::path::PathBuf;
use std::time::Duration;

use ferry_core::Side;
use ferry_core::capsule::{Action, Avatar, Capsule, Glyph, Look, Tile, hit, place};
use ferry_platform::{Overlay, OverlayEvent};
use ferry_proto::{DropHint, HintState};
use ferry_types::{Frame, Lang, Rgba};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, sleep_until};
use tracing::{info, warn};

use ferry_trust::Fingerprint;

use crate::events::{TransferState, TransferUpdate};
use crate::files::Files;
use drag_query::{DragQuery, Reply as QueryReply, Request as QueryRequest};

/// 离边缘多近（点）开始出现胶囊。
const NEAR: f32 = 210.0;
/// 离边缘多近（点）胶囊长到最大。
const CLOSE: f32 = 80.0;
/// 留意拖动时贴边接收条的宽度（点）。
const STRIP: f32 = 6.0;
/// 动画的帧间隔。
const FRAME: Duration = Duration::from_millis(16);
/// “已发送”“已取消”这类提示停留多久。
const BRIEF: Duration = Duration::from_millis(1800);
/// 出错提示停留多久。
const NOTICE: Duration = Duration::from_secs(4);
/// 收到后的卡片停留多久。
const CARD: Duration = Duration::from_secs(15);
/// 对方说“离开了”之后，接收胶囊再停留多久。
const GRACE: Duration = Duration::from_millis(300);
/// 对方直接从胶囊上“离开”，多半是放下了文件：提示和传输走不同的连接，先到的是“离开”，
/// 多留一会儿等传输开始，胶囊就不会收起又弹出来。
const DROP_GRACE: Duration = Duration::from_millis(2000);
/// 两条拖动提示之间至少隔多久；状态变化（靠近、到胶囊上、离开）不受限制。
const HINT_GAP: Duration = Duration::from_millis(50);
/// 剪贴板只在实际拖动且靠近边缘时轮询，至多每秒20次；没有活动拖动时不轮询。
const QUERY_GAP: Duration = Duration::from_millis(50);
/// 鼠标钩子的Up可能先于原生Drop回调到达；接收区域短暂保留，但绝不凭Up发送缓存内容。
const LOCAL_DROP_GRACE: Duration = Duration::from_millis(250);
/// 提示里沿边位置的满量程。
const ALONG_FULL: f32 = 10_000.0;

/// 本机光标的现状。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pointer {
    /// 桌面坐标。
    pub x: f32,
    pub y: f32,
    /// 是否按着鼠标键（拖着东西）。
    pub held: bool,
    /// 光标是否在本机。
    pub here: bool,
}

/// 朝向一台对方设备的那一段边。
#[derive(Clone, Debug, PartialEq)]
pub struct Edge {
    /// 对方。
    pub id: Fingerprint,
    /// 对方在本机的哪一侧。
    pub side: Side,
    /// 本机朝向对方的那一段（桌面坐标）：贴着 `side` 那条边、与本机屏幕同宽或同高的矩形。
    /// 只有一台设备在这一侧时就是整个屏幕。
    pub screen: Frame,
    /// 每个点对应多少桌面单位。
    pub scale: f32,
    /// 对方的设备名。
    pub peer: String,
    /// 是否连着对方。一台都没连着时保留最后那些边，拖文件过来时显示“未连接”。
    pub online: bool,
}

/// 对方发来的拖动提示：哪台设备，提示内容；内容为 `None` 表示它的会话结束了。
type Incoming = Option<(Fingerprint, Option<DropHint>)>;

/// 引擎往边缘投递报告光标和边的地方。克隆后共用；没有边缘投递时什么也不做。
#[derive(Clone)]
pub struct EdgeFeed {
    pointer: watch::Sender<Pointer>,
    edges: watch::Sender<Vec<Edge>>,
    /// 本机拖动中的提示和要发给哪台设备，由引擎发出。`None` 表示没有在拖（对方应收起）。
    outgoing: watch::Sender<Option<(Fingerprint, DropHint)>>,
    /// 对方发来的拖动提示。
    incoming: watch::Sender<Incoming>,
}

impl EdgeFeed {
    /// 不需要边缘投递（命令行模式、测试）。
    pub fn none() -> Self {
        Self {
            pointer: watch::channel(Pointer::default()).0,
            edges: watch::channel(Vec::new()).0,
            outgoing: watch::channel(None).0,
            incoming: watch::channel(None).0,
        }
    }

    /// 本机要发给对方的拖动提示，变了就发。
    pub(crate) fn hints(&self) -> watch::Receiver<Option<(Fingerprint, DropHint)>> {
        self.outgoing.subscribe()
    }

    /// `peer` 发来一条拖动提示；`None` 表示和它的会话结束了。
    pub(crate) fn remote(&self, peer: Fingerprint, hint: Option<DropHint>) {
        self.incoming.send_replace(Some((peer, hint)));
    }

    /// 引擎停下了（暂停共享、重启）：忘掉那些边、对方的提示和光标，胶囊随之收起。
    pub(crate) fn reset(&self) {
        self.edges.send_replace(Vec::new());
        self.incoming.send_replace(None);
        self.pointer.send_replace(Pointer::default());
    }

    /// 光标或按键变了。只在真的变了时唤醒边缘投递。
    pub(crate) fn pointer(&self, update: impl FnOnce(&mut Pointer)) {
        self.pointer.send_if_modified(|p| {
            let before = *p;
            update(p);
            *p != before
        });
    }

    /// 朝向各台对方设备的边变了；没有能收文件的对方时为空。
    pub(crate) fn edges(&self, edges: Vec<Edge>) {
        self.edges.send_if_modified(|e| {
            let changed = *e != edges;
            *e = edges;
            changed
        });
    }
}

/// 正在运行的边缘投递。丢弃即停止，胶囊随之收起。
pub struct EdgeDrop {
    feed: EdgeFeed,
    transfers: mpsc::UnboundedSender<TransferUpdate>,
    accent: watch::Sender<Rgba>,
    lang: watch::Sender<Lang>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for EdgeDrop {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl EdgeDrop {
    /// 在后台启动边缘投递。放下的文件交给 `files` 发送。
    pub fn spawn<O: Overlay + 'static>(
        mut overlay: O,
        files: Files,
        accent: Rgba,
        lang: Lang,
    ) -> Self {
        let (pointer, pointer_rx) = watch::channel(Pointer::default());
        let (edges, edges_rx) = watch::channel(Vec::new());
        let (outgoing, _) = watch::channel(None);
        let (incoming, incoming_rx) = watch::channel(None);
        let (transfers, transfers_rx) = mpsc::unbounded_channel();
        let (accent_tx, accent_rx) = watch::channel(accent);
        let (lang_tx, lang_rx) = watch::channel(lang);
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let started = overlay.start(Box::new(move |event| {
            let _ = events_tx.send(event);
        }));
        if let Err(e) = started {
            warn!("屏幕边缘的浮层用不了，拖文件到边缘不会有反应：{e}");
        }
        let worker = Worker {
            overlay,
            files,
            pointer: pointer_rx,
            edges: edges_rx,
            outgoing: outgoing.clone(),
            incoming: incoming_rx,
            transfers: transfers_rx,
            events: events_rx,
            accent: accent_rx,
            lang: lang_rx,
            capsule: Capsule::new(),
            drag: None,
            job: None,
            brief: None,
            card: None,
            target: None,
            side: Side::Left,
            scale: 1.0,
            drag_target: None,
            pending_along: None,
            watching: None,
            remote: None,
            offered: Vec::new(),
            dragging_out: false,
            sent: None,
            sent_at: Instant::now(),
            hint_due: None,
            shown: false,
            moving: false,
            dirty: false,
            drag_generation: 0,
            card_generation: 0,
            card_query: None,
            query_due: None,
            pointer_held: false,
            pending_drop: None,
        };
        Self {
            feed: EdgeFeed {
                pointer,
                edges,
                outgoing,
                incoming,
            },
            transfers,
            accent: accent_tx,
            lang: lang_tx,
            task: tokio::spawn(worker.run()),
        }
    }

    /// 交给引擎的报告入口。
    pub fn feed(&self) -> EdgeFeed {
        self.feed.clone()
    }

    /// 文件传输有了进展。
    pub fn transfer(&self, update: TransferUpdate) {
        let _ = self.transfers.send(update);
    }

    /// 强调色变了。
    pub fn set_accent(&self, accent: Rgba) {
        self.accent.send_replace(accent);
    }

    /// 界面语言变了。
    pub fn set_lang(&self, lang: Lang) {
        self.lang.send_replace(lang);
    }
}

/// 一次正在进行的拖动。
struct Drag {
    /// 按下鼠标那一刻拖放剪贴板的变化计数；之后变了，说明开始拖新的东西。
    count: Option<i64>,
    generation: u64,
    /// 原生 DragEntered 证明是当前拖动，不必等待异步取得的基线发生变化。
    force_read: bool,
    /// 拖的文件：还没判断时为 `None`，判断出不是文件时为空。
    files: Option<Vec<PathBuf>>,
    /// 拖着的东西在胶囊上。
    over: bool,
    /// 拖的是什么：第一项的名字、几项、总大小（有文件夹时不算，记为 0）。
    summary: Option<(String, u32, u64)>,
}

struct PendingDrop {
    until: Instant,
    peer: Option<Fingerprint>,
    along: f32,
}

/// 对方正在拖过来的东西。
struct Remote {
    peer: Fingerprint,
    hint: DropHint,
    /// 对方说“离开了”之后，接收胶囊留到什么时候。
    until: Option<Instant>,
}

/// 正在显示进度的传输。
struct Job {
    update: TransferUpdate,
    /// 沿边的位置：发送时是放下的位置，接收时在屏幕中间。
    along: f32,
}

/// 一闪而过的提示。
struct Brief {
    look: Look,
    along: f32,
    until: Instant,
    /// 关于哪台设备，决定贴在哪一段边上。
    peer: Option<Fingerprint>,
}

/// 收到后的卡片。
struct Card {
    peer: Fingerprint,
    saved: Vec<PathBuf>,
    look: Look,
    along: f32,
    until: Instant,
}

struct Worker<O> {
    overlay: O,
    files: Files,
    pointer: watch::Receiver<Pointer>,
    edges: watch::Receiver<Vec<Edge>>,
    outgoing: watch::Sender<Option<(Fingerprint, DropHint)>>,
    incoming: watch::Receiver<Incoming>,
    transfers: mpsc::UnboundedReceiver<TransferUpdate>,
    events: mpsc::UnboundedReceiver<OverlayEvent>,
    accent: watch::Receiver<Rgba>,
    lang: watch::Receiver<Lang>,
    capsule: Capsule,
    drag: Option<Drag>,
    job: Option<Job>,
    brief: Option<Brief>,
    card: Option<Card>,
    /// 上一次要求胶囊显示成的样子和位置。
    target: Option<(Look, Frame)>,
    /// 胶囊现在贴着的那一段边在哪一侧、每点多少桌面单位。
    side: Side,
    scale: f32,
    /// 本机拖动正朝着哪台设备：放下时发给它。
    drag_target: Option<Fingerprint>,
    /// 刚放下文件、进度还没来时，胶囊的位置。
    pending_along: Option<f32>,
    /// 正在请平台留意的贴边区域。
    watching: Option<Frame>,
    /// 对方正在拖过来的东西。
    remote: Option<Remote>,
    /// 交给浮层、可以从卡片拖出去的文件。
    offered: Vec<PathBuf>,
    /// 正在把卡片里的文件拖出去。
    dragging_out: bool,
    /// 最近一次发给对方的提示，和发出的时刻。
    sent: Option<(Fingerprint, DropHint)>,
    sent_at: Instant,
    /// 有一点小变化还没发，等到这个时刻再发。
    hint_due: Option<Instant>,
    /// 浮层窗口是否在屏幕上。
    shown: bool,
    /// 动画还在动：继续按帧画。
    moving: bool,
    /// 内容变了：下一帧要重画，即使动画已经停下。
    dirty: bool,
    drag_generation: u64,
    card_generation: u64,
    card_query: Option<PathBuf>,
    query_due: Option<Instant>,
    pointer_held: bool,
    pending_drop: Option<PendingDrop>,
}

impl<O: Overlay> Worker<O> {
    async fn run(mut self) {
        let mut queries = match DragQuery::new(self.overlay.drag_reader()).await {
            Ok(queries) => Some(queries),
            Err(error) => {
                warn!(%error, "拖放预览读取不可用，放下文件仍可发送");
                None
            }
        };
        let mut last = Instant::now();
        let mut next_frame: Option<Instant> = None;
        loop {
            // 输入只改变目标；动画按固定的下一帧截止时间推进，不随每个输入重新计时。
            let wake = next_frame
                .into_iter()
                .chain(self.deadline())
                .chain(self.query_due)
                .min();
            tokio::select! {
                changed = self.pointer.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.on_pointer();
                }
                changed = self.edges.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if self.edges.borrow().is_empty() {
                        self.drag = None;
                        self.pending_drop = None;
                    }
                }
                changed = self.incoming.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    self.on_remote();
                }
                _ = self.accent.changed() => self.dirty = true,
                _ = self.lang.changed() => self.dirty = true,
                Some(update) = self.transfers.recv() => self.on_transfer(update),
                Some(event) = self.events.recv() => self.on_event(event).await,
                result = next_query(&mut queries) => {
                    match result {
                        Ok(reply) => self.on_query(reply),
                        Err(error) => {
                            warn!(%error, "拖放预览读取停止，放下文件仍可发送");
                            queries = None;
                        }
                    }
                }
                _ = sleep_or_never(wake) => {}
            }
            let now = Instant::now();
            self.expire(now);
            self.schedule_query(&mut queries, now);
            self.watch();
            self.decide();
            self.sync_offer();
            self.publish_hint();
            if (self.moving || self.dirty) && next_frame.is_none() {
                next_frame = Some((last + FRAME).max(now));
            }
            if next_frame.is_some_and(|at| at <= now) {
                self.draw((now - last).as_secs_f32());
                last = now;
                next_frame = self.moving.then_some(now + FRAME);
            }
        }
    }

    /// 下一个到期的提示或卡片。
    fn deadline(&self) -> Option<Instant> {
        let brief = self.brief.as_ref().map(|b| b.until);
        let card = self
            .card
            .as_ref()
            .filter(|_| !self.dragging_out)
            .map(|c| c.until);
        let remote = self.remote.as_ref().and_then(|r| r.until);
        brief
            .into_iter()
            .chain(card)
            .chain(remote)
            .chain(self.hint_due)
            .chain(self.pending_drop.as_ref().map(|drop| drop.until))
            .min()
    }

    fn expire(&mut self, now: Instant) {
        if self
            .pending_drop
            .as_ref()
            .is_some_and(|drop| drop.until <= now)
        {
            self.pending_drop = None;
            self.drag = None;
        }
        if self.brief.as_ref().is_some_and(|b| b.until <= now) {
            self.brief = None;
        }
        // 正拖着卡片里的文件时，卡片不能自己收起。
        if !self.dragging_out && self.card.as_ref().is_some_and(|c| c.until <= now) {
            self.card = None;
        }
        if self
            .remote
            .as_ref()
            .and_then(|r| r.until)
            .is_some_and(|until| until <= now)
        {
            self.remote = None;
        }
    }

    fn on_remote(&mut self) {
        let incoming = self.incoming.borrow_and_update().clone();
        let from = |remote: &Option<Remote>, peer| remote.as_ref().is_some_and(|r| r.peer == peer);
        match incoming {
            None => self.remote = None,
            Some((peer, None)) => {
                if from(&self.remote, peer) {
                    self.remote = None;
                }
            }
            Some((peer, Some(hint))) if hint.state == HintState::Gone => {
                if let Some(remote) = &mut self.remote
                    && remote.peer == peer
                    && remote.until.is_none()
                {
                    let linger = if remote.hint.state == HintState::Over {
                        DROP_GRACE
                    } else {
                        GRACE
                    };
                    remote.until = Some(Instant::now() + linger);
                }
            }
            Some((peer, Some(hint))) => {
                self.remote = Some(Remote {
                    peer,
                    hint,
                    until: None,
                });
            }
        }
    }

    fn on_pointer(&mut self) {
        let pointer = *self.pointer.borrow_and_update();
        let released = self.pointer_held && !pointer.held;
        self.pointer_held = pointer.held;
        if pointer.held && self.pending_drop.take().is_some() {
            self.drag = None;
            self.drag_target = None;
        }
        if released && self.drag.is_some() {
            let near = pointer.here
                && edge_at(&self.edges.borrow(), &pointer)
                    .is_some_and(|edge| distance(edge, &pointer) <= NEAR);
            if near && (self.watching.is_some() || self.drag.as_ref().is_some_and(|drag| drag.over))
            {
                self.pending_drop = Some(PendingDrop {
                    until: Instant::now() + LOCAL_DROP_GRACE,
                    peer: self
                        .drag_target
                        .or_else(|| edge_at(&self.edges.borrow(), &pointer).map(|edge| edge.id)),
                    along: self.along(),
                });
                self.query_due = None;
                return;
            }
            self.drag = None;
        }
        match (&mut self.drag, pointer.held) {
            (None, true) if !self.dragging_out => {
                self.drag_target = None;
                self.drag_generation = self.drag_generation.wrapping_add(1);
                self.drag = Some(Drag {
                    count: None,
                    generation: self.drag_generation,
                    force_read: false,
                    files: None,
                    over: false,
                    summary: None,
                });
            }
            (Some(drag), false) if !drag.over && self.pending_drop.is_none() => self.drag = None,
            (Some(_), true) => {}
            _ => {}
        }
    }

    fn schedule_query(&mut self, queries: &mut Option<DragQuery>, now: Instant) {
        if self.pending_drop.is_some() {
            self.query_due = None;
            return;
        }
        let Some(query) = queries.as_mut() else {
            self.query_due = None;
            return;
        };
        if query.busy() {
            // 一项查询没有完成就不再设置周期唤醒或排入更多工作。
            self.query_due = None;
            return;
        }
        let pointer = *self.pointer.borrow();
        let near = pointer.here
            && edge_at(&self.edges.borrow(), &pointer)
                .is_some_and(|edge| distance(edge, &pointer) <= NEAR);
        let request = match self.drag.as_ref() {
            Some(drag)
                if (pointer.held || drag.over) && !self.dragging_out && drag.files.is_none() =>
            {
                match drag.count {
                    None => Some(QueryRequest::Baseline {
                        generation: drag.generation,
                    }),
                    Some(baseline) if near || drag.force_read => {
                        if self.query_due.is_some_and(|due| due > now) {
                            return;
                        }
                        Some(QueryRequest::Files {
                            generation: drag.generation,
                            baseline,
                            force: drag.force_read,
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        let request = request.or_else(|| {
            if self.card.is_none() {
                self.card_query = None;
            }
            self.card_query.take().map(|path| QueryRequest::Directory {
                generation: self.card_generation,
                path,
            })
        });
        self.query_due = None;
        if let Some(request) = request
            && let Err(error) = query.request(request)
        {
            warn!(%error, "拖放预览读取停止，放下文件仍可发送");
            *queries = None;
        }
    }

    fn on_query(&mut self, reply: QueryReply) {
        match reply {
            QueryReply::Baseline { generation, count } => {
                if let Some(drag) = &mut self.drag
                    && drag.generation == generation
                {
                    drag.count = Some(count);
                    self.query_due = Some(Instant::now());
                }
            }
            QueryReply::Files {
                generation,
                content,
            } => {
                let Some(drag) = &mut self.drag else {
                    return;
                };
                if drag.generation != generation
                    || (!self.pointer.borrow().held && !drag.over)
                    || self.pending_drop.is_some()
                    || self.dragging_out
                {
                    return;
                }
                if let Some(content) = content {
                    let had_files = !content.files.is_empty();
                    let files = not_ours(content.files, &self.offered);
                    if files.is_empty() {
                        if had_files {
                            // 已确认是当前卡片自己提供的文件，不把它当成新的跨机发送。
                            drag.files = Some(Vec::new());
                            drag.over = false;
                            drag.force_read = false;
                            self.query_due = None;
                            return;
                        }
                        // 原生拖动数据可能尚未准备好；空读取不是整个手势的永久结论。
                        self.query_due = Some(Instant::now() + QUERY_GAP);
                        return;
                    }
                    drag.summary = (!files.is_empty()).then_some(content.summary).flatten();
                    drag.files = Some(files);
                    drag.force_read = false;
                } else {
                    self.query_due = Some(Instant::now() + QUERY_GAP);
                }
            }
            QueryReply::Directory {
                generation,
                directory,
            } => {
                if generation == self.card_generation
                    && directory
                    && let Some(card) = &mut self.card
                    && let Look::Card { tile, .. } = &mut card.look
                {
                    *tile = Tile::Folder;
                }
            }
        }
    }

    async fn on_event(&mut self, event: OverlayEvent) {
        match event {
            // 从卡片拖出去的文件经过浮层自己，不算往对方拖。
            OverlayEvent::DragEntered { .. } | OverlayEvent::Dropped { .. }
                if self.dragging_out => {}
            OverlayEvent::DragEntered { files } => {
                if self.pending_drop.is_some() {
                    return;
                }
                if files && self.drag.is_none() && !self.edges.borrow().is_empty() {
                    self.drag_target = None;
                    self.drag_generation = self.drag_generation.wrapping_add(1);
                    self.drag = Some(Drag {
                        count: None,
                        generation: self.drag_generation,
                        force_read: true,
                        files: None,
                        over: true,
                        summary: None,
                    });
                }
                if let Some(drag) = &mut self.drag {
                    drag.over = files;
                    // 有的系统（Windows）要到拖进接收条才知道拖的是什么。
                    if drag.files.is_none() && files {
                        drag.force_read = true;
                        self.query_due = Some(Instant::now());
                    }
                }
            }
            OverlayEvent::DragExited => {
                if self.pending_drop.is_some() {
                    return;
                }
                if let Some(drag) = &mut self.drag {
                    drag.over = false;
                }
                if !self.pointer.borrow().held {
                    self.drag = None;
                }
            }
            OverlayEvent::Dropped { paths } => {
                let pending = self.pending_drop.take();
                let along = pending
                    .as_ref()
                    .map_or_else(|| self.along(), |drop| drop.along);
                self.drag = None;
                self.query_due = None;
                let target = pending
                    .and_then(|drop| drop.peer)
                    .or(self.drag_target.take())
                    .or_else(|| {
                        let pointer = *self.pointer.borrow();
                        edge_at(&self.edges.borrow(), &pointer).map(|e| e.id)
                    })
                    .filter(|peer| self.edges.borrow().iter().any(|edge| edge.id == *peer));
                info!(items = paths.len(), "边缘收到文件放下，正在准备传输");
                let lang = *self.lang.borrow();
                self.brief = Some(Brief {
                    look: wide(
                        Avatar::Device(peer_glyph()),
                        [lang.pick("正在准备发送", "Preparing transfer"), "", ""],
                        Action::None,
                    ),
                    along,
                    until: Instant::now() + NOTICE,
                    peer: target,
                });
                self.decide();
                self.draw(1.0);
                let sent = match target {
                    Some(peer) => self.files.send(peer, paths).await,
                    None => Err(crate::error::Error::NotConnected),
                };
                match sent {
                    Ok(_) => {
                        // 进度到来之前先占住位置，免得胶囊收起又弹出来。
                        self.brief = None;
                        self.job = None;
                        self.pending_along = Some(along);
                    }
                    Err(e) => {
                        warn!("边缘文件发送准备失败，已在本机显示原因");
                        let lang = *self.lang.borrow();
                        let look = wide(
                            Avatar::Warn,
                            [lang.pick("发不出去", "Couldn't send"), &e.to_string(), ""],
                            Action::None,
                        );
                        self.brief = Some(Brief {
                            look,
                            along,
                            until: Instant::now() + NOTICE,
                            peer: target,
                        });
                    }
                }
            }
            OverlayEvent::Clicked(id) => self.on_click(id),
            OverlayEvent::DragOutStarted => {
                self.dragging_out = true;
                self.drag = None;
                self.pending_drop = None;
            }
            OverlayEvent::DragOutEnded { dropped } => {
                self.dragging_out = false;
                if dropped {
                    // 文件已经放到别处（可能被移走了），卡片完成了使命。
                    self.card = None;
                } else if let Some(card) = &mut self.card {
                    card.until = Instant::now() + CARD;
                }
            }
        }
    }

    /// 告诉浮层卡片上能拖出去哪些文件。
    fn sync_offer(&mut self) {
        if self.dragging_out {
            return;
        }
        let wanted = self
            .card
            .as_ref()
            .map_or_else(Vec::new, |card| card.saved.clone());
        if wanted != self.offered {
            self.overlay.offer(wanted.clone());
            self.offered = wanted;
        }
    }

    fn on_click(&mut self, id: u32) {
        match id {
            hit::CANCEL => {
                if let Some(job) = &self.job {
                    self.files.cancel(job.update.id);
                }
            }
            hit::OPEN | hit::REVEAL | hit::CLOSE => {
                let Some(card) = self.card.take() else {
                    return;
                };
                if let Some(first) = card.saved.first() {
                    match id {
                        hit::OPEN => self.overlay.open(first),
                        hit::REVEAL => self.overlay.reveal(first),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn on_transfer(&mut self, update: TransferUpdate) {
        let along = self.transfer_along(&update);
        let lang = *self.lang.borrow();
        let what = name_of(&update, lang);
        let id = update.peer;
        let peer = self.peer_name(&id);
        let brief = |look, until| {
            Some(Brief {
                look,
                along,
                until: Instant::now() + until,
                peer: Some(id),
            })
        };
        match &update.state {
            TransferState::Waiting | TransferState::Running => {
                self.card = None;
                self.brief = None;
                if !update.outgoing {
                    // 传输开始了，接收胶囊换成进度。
                    self.remote = None;
                }
                self.job = Some(Job { update, along });
                return;
            }
            TransferState::Done { saved } if !update.outgoing => {
                self.card_generation = self.card_generation.wrapping_add(1);
                self.card_query = (update.items == 1)
                    .then(|| saved.first().cloned())
                    .flatten();
                let size = size_text(update.bytes);
                let look = Look::Card {
                    from: lang.pick(format!("来自 {peer}"), format!("From {peer}")),
                    name: what.clone(),
                    detail: lang.pick(
                        format!("已保存到「下载」· {size}"),
                        format!("Saved to Downloads · {size}"),
                    ),
                    tile: tile_of(&update),
                    chip: true,
                };
                self.card = Some(Card {
                    peer: id,
                    saved: saved.clone(),
                    look,
                    along,
                    until: Instant::now() + CARD,
                });
            }
            TransferState::Done { .. } => {
                let to = lang.pick(format!("到 {peer}"), format!("to {peer}"));
                let look = wide(
                    Avatar::Check,
                    [lang.pick("已发送", "Sent"), &to, ""],
                    Action::None,
                );
                self.brief = brief(look, BRIEF);
            }
            TransferState::Interrupted => {
                let look = wide(
                    Avatar::Warn,
                    [
                        lang.pick("传输中断", "Transfer interrupted"),
                        &what,
                        lang.pick("重新连上后接着传", "Resumes after reconnecting"),
                    ],
                    Action::None,
                );
                self.brief = brief(look, NOTICE);
            }
            TransferState::Cancelled { .. } => {
                let detail = if update.outgoing {
                    lang.pick("没有发送", "Nothing was sent")
                } else {
                    lang.pick("临时文件已删除", "Partial files were deleted")
                };
                let look = wide(
                    Avatar::Cross,
                    [lang.pick("已取消", "Cancelled"), detail, ""],
                    Action::None,
                );
                self.brief = brief(look, BRIEF);
            }
            TransferState::Failed { reason } => {
                let look = wide(
                    Avatar::Warn,
                    [lang.pick("传输失败", "Transfer failed"), &what, reason],
                    Action::None,
                );
                self.brief = brief(look, NOTICE);
            }
        }
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.update.id == update.id)
        {
            self.job = None;
        }
    }

    /// 活动拖动靠近边缘时保留原生接收条，直到放下/取消；可见胶囊由平台优先显示。
    fn watch(&mut self) {
        if self.pending_drop.is_some() {
            return;
        }
        let pointer = *self.pointer.borrow();
        let edge = edge_at(&self.edges.borrow(), &pointer).cloned();
        let wanted = match (&self.drag, &edge) {
            (Some(drag), Some(edge))
                if pointer.here
                    && (pointer.held || drag.over)
                    && distance(edge, &pointer) <= NEAR =>
            {
                Some(strip_of(edge))
            }
            _ => None,
        };
        if wanted != self.watching {
            self.overlay.watch_edge(wanted);
            self.watching = wanted;
        }
    }

    /// 根据现状决定胶囊该显示成什么样子、贴在哪一段边的哪里。
    fn decide(&mut self) {
        let edges = self.edges.borrow().clone();
        if edges.is_empty() {
            self.drag_target = None;
            self.retarget(None);
            return;
        }
        if self.pending_drop.is_some() {
            return;
        }
        if self.fallback_only() {
            // 旧传输卡片不能遮住新拖动所在高度的原生接收条。保留卡片状态，临时让出窗口。
            self.drag_target = edge_at(&edges, &self.pointer.borrow()).map(|edge| edge.id);
            self.target = None;
            self.capsule = Capsule::new();
            self.moving = false;
            self.dirty = false;
            if self.shown {
                self.overlay.hide();
                self.shown = false;
            }
            return;
        }
        let pointer = *self.pointer.borrow();
        // 关于某台设备的东西贴在它那一段；它已经不在了，就贴在还在的第一段。
        let of = |peer: Option<Fingerprint>| -> Option<Edge> {
            peer.and_then(|id| edges.iter().find(|e| e.id == id))
                .or(edges.first())
                .cloned()
        };
        let dragging = self.dragging(&edges, &pointer);
        self.drag_target = dragging.as_ref().map(|(_, _, edge)| edge.id);
        let wanted = dragging
            .or_else(|| {
                let job = self.job.as_ref()?;
                let lang = *self.lang.borrow();
                Some((
                    progress(&job.update, lang),
                    job.along,
                    of(Some(job.update.peer))?,
                ))
            })
            .or_else(|| self.receiving(&edges))
            .or_else(|| {
                let brief = self.brief.as_ref()?;
                Some((brief.look.clone(), brief.along, of(brief.peer)?))
            })
            .or_else(|| {
                let card = self.card.as_ref()?;
                Some((card.look.clone(), card.along, of(Some(card.peer))?))
            });
        let target = wanted.map(|(look, along, edge)| {
            let frame = place(&look, edge.side, edge.screen, along, edge.scale);
            self.side = edge.side;
            self.scale = edge.scale;
            (look, frame)
        });
        self.retarget(target);
    }

    /// 拖着文件靠近某一段边时的样子，以及那一段。
    fn dragging(&self, edges: &[Edge], pointer: &Pointer) -> Option<(Look, f32, Edge)> {
        let drag = self.drag.as_ref()?;
        let files = drag.files.as_ref().filter(|f| !f.is_empty())?;
        if !pointer.here {
            return None;
        }
        let edge = edge_at(edges, pointer)?.clone();
        let along = along_of(&edge, pointer);
        let offline = !edge.online;
        if drag.over {
            let lang = *self.lang.borrow();
            let lines = if offline {
                [
                    lang.pick("未连接", "Not connected").into(),
                    edge.peer.clone(),
                    lang.pick("连上后才能发送", "Connect first to send").into(),
                ]
            } else {
                [
                    lang.pick("松手发送到", "Drop to send to").into(),
                    edge.peer.clone(),
                    files_text(files, lang),
                ]
            };
            let look = Look::Wide {
                avatar: Avatar::Device(peer_glyph()),
                lines,
                hot: !offline,
                offline,
                action: Action::None,
            };
            return Some((look, along, edge));
        }
        let d = distance(&edge, pointer);
        if d > NEAR {
            return None;
        }
        let closeness = ((NEAR - d) / (NEAR - CLOSE)).clamp(0.0, 1.0);
        let look = Look::Tall {
            glyph: peer_glyph(),
            sending: true,
            offline,
            closeness,
        };
        Some((look, along, edge))
    }

    /// 对方正在拖过来时的接收胶囊，贴在那台设备那一段。
    fn receiving(&self, edges: &[Edge]) -> Option<(Look, f32, Edge)> {
        let remote = self.remote.as_ref()?;
        let edge = edges.iter().find(|e| e.id == remote.peer)?.clone();
        let hint = &remote.hint;
        let along = along_from(&edge, hint.along);
        let look = match hint.state {
            HintState::Over => {
                let lang = *self.lang.borrow();
                let what = with_more(&hint.name, hint.items, lang);
                let detail = if hint.bytes > 0 {
                    format!("{what} · {}", size_text(hint.bytes))
                } else {
                    what
                };
                Look::Wide {
                    avatar: Avatar::Device(peer_glyph()),
                    lines: [
                        lang.pick(format!("来自 {}", edge.peer), format!("From {}", edge.peer)),
                        lang.pick("松手即接收", "Drop to receive").into(),
                        detail,
                    ],
                    hot: true,
                    offline: false,
                    action: Action::None,
                }
            }
            _ => Look::Tall {
                glyph: peer_glyph(),
                sending: false,
                offline: false,
                closeness: f32::from(hint.closeness) / 100.0,
            },
        };
        Some((look, along, edge))
    }

    /// 把本机拖动的现状告诉拖向的那台设备。只在有明显变化时更新，免得占用输入连接。
    fn publish_hint(&mut self) {
        let pointer = *self.pointer.borrow();
        let edge = edge_at(&self.edges.borrow(), &pointer)
            .filter(|e| e.online)
            .cloned();
        let hint = edge.and_then(|edge| {
            let drag = self.drag.as_ref()?;
            let (name, items, bytes) = drag.summary.clone()?;
            if !pointer.here {
                return None;
            }
            let d = distance(&edge, &pointer);
            let (state, closeness) = if drag.over {
                (HintState::Over, 100)
            } else if d <= NEAR {
                let k = ((NEAR - d) / (NEAR - CLOSE)).clamp(0.0, 1.0);
                (HintState::Near, (k * 100.0).round() as u8)
            } else {
                return None;
            };
            let hint = DropHint {
                state,
                along: fraction_of(&edge, &pointer),
                closeness,
                name,
                items,
                bytes,
            };
            Some((edge.id, hint))
        });
        // 状态变了、换了设备马上发；只是挪了一点，攒够间隔再发。
        let (changed, urgent) = match (&self.sent, &hint) {
            (None, None) => (false, false),
            (Some((pa, a)), Some((pb, b))) => {
                let urgent = pa != pb || a.state != b.state || a.name != b.name;
                let moved =
                    a.closeness.abs_diff(b.closeness) >= 5 || a.along.abs_diff(b.along) >= 50;
                (urgent || moved, urgent)
            }
            _ => (true, true),
        };
        if !changed {
            self.hint_due = None;
            return;
        }
        let now = Instant::now();
        let due = self.sent_at + HINT_GAP;
        if urgent || now >= due {
            self.outgoing.send_replace(hint.clone());
            self.sent = hint;
            self.sent_at = now;
            self.hint_due = None;
        } else {
            self.hint_due = Some(due);
        }
    }

    fn retarget(&mut self, target: Option<(Look, Frame)>) {
        if target == self.target {
            return;
        }
        match &target {
            Some((look, frame)) => self.capsule.show(look.clone(), self.side, *frame),
            None => self.capsule.hide(),
        }
        self.target = target;
        self.moving = true;
        self.dirty = true;
    }

    fn draw(&mut self, dt: f32) {
        if !self.moving && !self.dirty {
            return;
        }
        self.moving = self.capsule.step(dt);
        self.dirty = false;
        if self.fallback_only() {
            if self.shown {
                self.overlay.hide();
                self.shown = false;
            }
            return;
        }
        let scale = self.scale;
        let accent = *self.accent.borrow();
        match self.capsule.render(scale, accent) {
            Some((window, scene)) => {
                self.overlay.show(window, scale, scene);
                self.shown = true;
            }
            None if self.shown => {
                self.overlay.hide();
                self.shown = false;
            }
            None => {}
        }
    }

    fn fallback_only(&self) -> bool {
        let pointer = *self.pointer.borrow();
        self.pending_drop.is_none()
            && self
                .drag
                .as_ref()
                .is_some_and(|drag| drag.files.is_none() && (pointer.held || drag.over))
            && pointer.here
            && edge_at(&self.edges.borrow(), &pointer)
                .is_some_and(|edge| distance(edge, &pointer) <= NEAR)
    }

    /// 光标沿着它靠近的那一段边的位置。
    fn along(&self) -> f32 {
        let pointer = *self.pointer.borrow();
        edge_at(&self.edges.borrow(), &pointer).map_or(pointer.y, |edge| along_of(edge, &pointer))
    }

    /// 一次传输的胶囊放在哪里：沿用已有的位置，对方拖过来的放在拖过来的地方，
    /// 其余放在那台设备那一段的中间。
    fn transfer_along(&mut self, update: &TransferUpdate) -> f32 {
        if let Some(job) = self.job.as_ref().filter(|j| j.update.id == update.id) {
            return job.along;
        }
        if let Some(along) = self.pending_along.take() {
            return along;
        }
        let edges = self.edges.borrow();
        let Some(edge) = edges.iter().find(|e| e.id == update.peer).or(edges.first()) else {
            return 0.0;
        };
        if let Some(remote) = self.remote.as_ref().filter(|r| r.peer == update.peer) {
            return along_from(edge, remote.hint.along);
        }
        match edge.side {
            Side::Left | Side::Right => edge.screen.y + edge.screen.h / 2.0,
            Side::Top | Side::Bottom => edge.screen.x + edge.screen.w / 2.0,
        }
    }

    /// 设备名，用于显示。
    fn peer_name(&self, peer: &Fingerprint) -> String {
        self.edges
            .borrow()
            .iter()
            .find(|e| e.id == *peer)
            .map_or_else(
                || {
                    self.lang
                        .borrow()
                        .pick("对方", "the other computer")
                        .to_owned()
                },
                |e| e.peer.clone(),
            )
    }
}

/// 光标沿边落在哪一段：落在里面的几段中离光标最近的那一段。
fn edge_at<'a>(edges: &'a [Edge], p: &Pointer) -> Option<&'a Edge> {
    edges
        .iter()
        .filter(|edge| {
            let s = edge.screen;
            match edge.side {
                Side::Left | Side::Right => p.y >= s.y && p.y < s.bottom(),
                Side::Top | Side::Bottom => p.x >= s.x && p.x < s.right(),
            }
        })
        .min_by(|a, b| distance(a, p).total_cmp(&distance(b, p)))
}

/// 光标离朝向对方的那条边多远（点）。
fn distance(edge: &Edge, p: &Pointer) -> f32 {
    let s = edge.screen;
    let d = match edge.side {
        Side::Left => p.x - s.x,
        Side::Right => s.right() - 1.0 - p.x,
        Side::Top => p.y - s.y,
        Side::Bottom => s.bottom() - 1.0 - p.y,
    };
    d.max(0.0) / edge.scale.max(0.01)
}

/// 贴着朝向对方那条边的一窄条（桌面坐标）。
fn strip_of(edge: &Edge) -> Frame {
    let s = edge.screen;
    let w = STRIP * edge.scale;
    match edge.side {
        Side::Left => Frame::new(s.x, s.y, w, s.h),
        Side::Right => Frame::new(s.right() - w, s.y, w, s.h),
        Side::Top => Frame::new(s.x, s.y, s.w, w),
        Side::Bottom => Frame::new(s.x, s.bottom() - w, s.w, w),
    }
}

/// 告诉对方“没在拖了”的提示。
pub(crate) fn gone() -> DropHint {
    DropHint {
        state: HintState::Gone,
        along: 0,
        closeness: 0,
        name: String::new(),
        items: 0,
        bytes: 0,
    }
}

/// 沿边位置换成提示里的比例（0–10000）。
fn fraction_of(edge: &Edge, p: &Pointer) -> u16 {
    let s = edge.screen;
    let k = match edge.side {
        Side::Left | Side::Right => (p.y - s.y) / s.h.max(1.0),
        Side::Top | Side::Bottom => (p.x - s.x) / s.w.max(1.0),
    };
    (k.clamp(0.0, 1.0) * ALONG_FULL).round() as u16
}

/// 提示里的比例换回本机沿边的坐标。
fn along_from(edge: &Edge, fraction: u16) -> f32 {
    let k = f32::from(fraction.min(10_000)) / ALONG_FULL;
    let s = edge.screen;
    match edge.side {
        Side::Left | Side::Right => s.y + k * s.h,
        Side::Top | Side::Bottom => s.x + k * s.w,
    }
}

/// 拖的正是卡片里自己交出去的文件时，不当作要发给对方的文件。
fn not_ours(files: Vec<PathBuf>, offered: &[PathBuf]) -> Vec<PathBuf> {
    if !offered.is_empty() && files == offered {
        Vec::new()
    } else {
        files
    }
}

fn along_of(edge: &Edge, p: &Pointer) -> f32 {
    match edge.side {
        Side::Left | Side::Right => p.y,
        Side::Top | Side::Bottom => p.x,
    }
}

/// 对方是什么设备。还没有从协议拿到对方的系统，暂按“另一种系统”画：Mac 上画显示器，Windows 上画笔记本。
fn peer_glyph() -> Glyph {
    if std::env::consts::OS == "macos" {
        Glyph::Monitor
    } else {
        Glyph::Laptop
    }
}

fn wide(avatar: Avatar, lines: [&str; 3], action: Action) -> Look {
    Look::Wide {
        avatar,
        lines: lines.map(str::to_owned),
        hot: false,
        offline: false,
        action,
    }
}

/// 传输中的样子：进度环、百分比、名字、已传多少。
fn progress(update: &TransferUpdate, lang: Lang) -> Look {
    let fraction = if update.bytes == 0 {
        if matches!(update.state, TransferState::Running) {
            1.0
        } else {
            0.0
        }
    } else {
        update.done as f32 / update.bytes as f32
    };
    let pct = (fraction * 100.0).round() as u32;
    let verb = if update.outgoing {
        lang.pick("发送中", "Sending")
    } else {
        lang.pick("接收中", "Receiving")
    };
    let amount = format!("{} / {}", size_text(update.done), size_text(update.bytes));
    Look::Wide {
        avatar: Avatar::Ring(fraction),
        lines: [format!("{verb} {pct}%"), name_of(update, lang), amount],
        hot: false,
        offline: false,
        action: Action::Cancel,
    }
}

fn name_of(update: &TransferUpdate, lang: Lang) -> String {
    with_more(&update.name, update.items, lang)
}

/// 第一项的名字，几项时带上数量：“报告.pdf 等 3 项”“报告.pdf and 2 more”。
fn with_more(first: &str, items: u32, lang: Lang) -> String {
    if items > 1 {
        lang.pick(
            format!("{first} 等 {items} 项"),
            format!("{first} and {} more", items - 1),
        )
    } else {
        first.to_owned()
    }
}

/// 卡片上的小图标：多项、文件夹，或者带扩展名的文件。
fn tile_of(update: &TransferUpdate) -> Tile {
    if update.items > 1 {
        return Tile::Stack(update.items);
    }
    let ext = std::path::Path::new(&update.name)
        .extension()
        .map(|e| e.to_string_lossy().to_uppercase())
        .unwrap_or_default();
    let ext: String = ext.chars().take(4).collect();
    let tone = match ext.as_str() {
        "PDF" => Rgba::hex(0xC2410C, 255),
        "MOV" | "MP4" | "MKV" | "AVI" => Rgba::hex(0x6A4BD8, 255),
        "PNG" | "JPG" | "JPEG" | "GIF" | "HEIC" | "WEBP" | "SVG" => Rgba::hex(0x00796B, 255),
        "DOC" | "DOCX" | "TXT" | "MD" | "PAGE" => Rgba::hex(0x0A6CFF, 255),
        "ZIP" | "RAR" | "7Z" | "DMG" => Rgba::hex(0x8A6D1F, 255),
        _ => Rgba::hex(0x0F766E, 255),
    };
    Tile::Doc { ext, tone }
}

/// 拖着的文件的说明：一个时是名字，多个时是第一个的名字和数量。
fn files_text(files: &[PathBuf], lang: Lang) -> String {
    let first = files
        .first()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    with_more(&first, u32::try_from(files.len()).unwrap_or(u32::MAX), lang)
}

/// 文件大小，给人看的写法。
pub(crate) fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

async fn sleep_or_never(at: Option<Instant>) {
    match at {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

async fn next_query(queries: &mut Option<DragQuery>) -> std::io::Result<QueryReply> {
    match queries {
        Some(queries) => queries.completed().await,
        None => std::future::pending().await,
    }
}

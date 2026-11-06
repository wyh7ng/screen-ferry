//! 剪贴板同步：在数据连接上运行，服务端与客户端共用。数据连接上的文件控制消息也在这里收发，
//! 交给 [`FileLink`]。
//!
//! 同一时刻只有一台是“活动端”（光标所在的那台）。规则只有两条：
//!
//! - **通告**：活动端轮询本机剪贴板，变了就发 `ClipboardOffer`，只说有哪些格式、多大；
//!   成为非活动端的那一刻再检查一次，刚复制完就切走也不会漏。
//! - **拉取**：手上有对方未拉取的通告、且本机是活动端时，立即拉取并写入本机剪贴板。
//!
//! 所以复制多次只传最后一次；光标不过去，对方的剪贴板就不会被覆盖。
//! 例外是刚连上时：服务端先通告一次当前内容，见 [`Start`]。
//! 写入后记下剪贴板的变化计数，自己写入的内容不会再被当作新复制通告回去。

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ferry_net::Framed;
use ferry_platform::Clipboard;
use ferry_proto::limits::MAX_CHUNK_LEN;
use ferry_proto::{
    Cancel, CancelReason, Chunk, ClipboardItem, ClipboardOffer, ClipboardRequest, Message,
};
use ferry_trust::Fingerprint;
use ferry_types::{ClipboardContent, ClipboardFormat};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, info, warn};

use crate::error::{Error, Result};
use crate::events::{ClipSummary, Event, Events};
use crate::files::FileLink;
use crate::link::send;

/// 活动端检查本机剪贴板的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 转换后的共享缓存总量；会话各自的单格式限制不改变缓存的原内容。
const CACHE_BYTES: usize = 64 * 1024 * 1024;
/// 没有会话读取缓存后及时释放大图片；轮询变化计数不延长寿命。
const CACHE_IDLE: Duration = Duration::from_secs(5);
const CACHE_SWEEP: Duration = Duration::from_secs(1);
const STABLE_READ_ATTEMPTS: usize = 3;

/// 单个格式缺省的大小上限：32 MB。
pub const DEFAULT_CLIP_LIMIT: u64 = 32 * 1024 * 1024;

/// 剪贴板单个格式的大小上限，超过的不通告也不拉取。克隆后共用同一个值，会话中途也能改。
#[derive(Clone, Debug)]
pub struct ClipLimit(Arc<AtomicU64>);

impl ClipLimit {
    pub fn new(bytes: u64) -> Self {
        Self(Arc::new(AtomicU64::new(bytes)))
    }

    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    pub fn set(&self, bytes: u64) {
        self.0.store(bytes, Ordering::Relaxed);
    }
}

impl Default for ClipLimit {
    fn default() -> Self {
        Self::new(DEFAULT_CLIP_LIMIT)
    }
}

/// 每端一条专用串行工作线程。系统调用和图片编解码不占用输入/心跳运行时。
pub(crate) struct SharedClipboard<C> {
    requests: mpsc::Sender<ClipboardWork>,
    formats: &'static [ClipboardFormat],
    marker: PhantomData<fn() -> C>,
}

impl<C> Clone for SharedClipboard<C> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            formats: self.formats,
            marker: PhantomData,
        }
    }
}

enum ClipboardWork {
    Sweep,
    Count(oneshot::Sender<u64>),
    Read(oneshot::Sender<(u64, ferry_platform::Result<ClipboardContent>)>),
    Write(
        ClipboardContent,
        oneshot::Sender<ferry_platform::Result<u64>>,
    ),
}

impl<C: Clipboard> SharedClipboard<C> {
    pub(crate) fn new(mut clipboard: C) -> Result<Self> {
        let formats = clipboard.formats();
        let (requests, mut incoming) = mpsc::channel(8);
        let weak = requests.downgrade();
        std::thread::Builder::new()
            .name("ferry-clipboard".into())
            .spawn(move || {
                let mut cache = ClipboardCache::default();
                while let Some(work) = incoming.blocking_recv() {
                    cache.expire();
                    match work {
                        ClipboardWork::Count(reply) if !reply.is_closed() => {
                            let count = clipboard.change_count();
                            cache.changed(count);
                            let _ = reply.send(count);
                        }
                        ClipboardWork::Read(reply) if !reply.is_closed() => {
                            let _ = reply.send(cache.read(&mut clipboard));
                        }
                        ClipboardWork::Write(content, reply) if !reply.is_closed() => {
                            cache.entry = None;
                            let _ = reply.send(clipboard.write(&content));
                        }
                        _ => {} // 会话已结束的排队工作直接丢弃。
                    }
                }
            })?;
        // 只持有弱发送端：所有会话结束时专用线程能自然退出，不会被清理任务留住。
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CACHE_SWEEP).await;
                let Some(sender) = weak.upgrade() else {
                    break;
                };
                if matches!(
                    sender.try_send(ClipboardWork::Sweep),
                    Err(mpsc::error::TrySendError::Closed(_))
                ) {
                    break;
                }
            }
        });
        Ok(Self {
            requests,
            formats,
            marker: PhantomData,
        })
    }

    async fn count(&self) -> Result<u64> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ClipboardWork::Count(reply))
            .await
            .map_err(|_| worker_closed())?;
        result.await.map_err(|_| worker_closed())
    }

    async fn read(&self) -> Result<(u64, ferry_platform::Result<ClipboardContent>)> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ClipboardWork::Read(reply))
            .await
            .map_err(|_| worker_closed())?;
        result.await.map_err(|_| worker_closed())
    }

    async fn write(&self, content: ClipboardContent) -> Result<u64> {
        let (reply, result) = oneshot::channel();
        self.requests
            .send(ClipboardWork::Write(content, reply))
            .await
            .map_err(|_| worker_closed())?;
        result
            .await
            .map_err(|_| worker_closed())?
            .map_err(Error::from)
    }
}

#[derive(Default)]
struct ClipboardCache {
    entry: Option<CachedClipboard>,
}

struct CachedClipboard {
    count: u64,
    content: ClipboardContent,
    used: Instant,
}

impl ClipboardCache {
    fn expire(&mut self) {
        if self
            .entry
            .as_ref()
            .is_some_and(|entry| entry.used.elapsed() >= CACHE_IDLE)
        {
            self.entry = None;
        }
    }

    fn changed(&mut self, count: u64) {
        if self
            .entry
            .as_ref()
            .is_some_and(|entry| entry.count != count)
        {
            self.entry = None;
        }
    }

    fn read<C: Clipboard>(
        &mut self,
        clipboard: &mut C,
    ) -> (u64, ferry_platform::Result<ClipboardContent>) {
        let mut count = clipboard.change_count();
        self.changed(count);
        if let Some(entry) = &mut self.entry {
            entry.used = Instant::now();
            return (count, Ok(entry.content.clone()));
        }
        for _ in 0..STABLE_READ_ATTEMPTS {
            let content = match clipboard.read() {
                Ok(content) => content,
                Err(error) => return (count, Err(error)),
            };
            let after = clipboard.change_count();
            if count == after {
                if content.total_len() <= CACHE_BYTES {
                    self.entry = Some(CachedClipboard {
                        count,
                        content: content.clone(),
                        used: Instant::now(),
                    });
                }
                return (count, Ok(content));
            }
            count = after;
        }
        (
            count,
            Err(ferry_platform::Error::Failed(
                "剪贴板在读取期间持续变化，请稍后再试".into(),
            )),
        )
    }
}

fn worker_closed() -> Error {
    Error::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "剪贴板工作线程已停止",
    ))
}

/// 正在运行的同步任务，丢弃即停止。
pub(crate) struct ClipboardTask(JoinHandle<()>);

impl ClipboardTask {
    pub(crate) async fn finished(&mut self) {
        let _ = (&mut self.0).await;
    }
}

impl Drop for ClipboardTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// 连上时谁先通告本机剪贴板。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// 会话开始时光标总在服务端，它的剪贴板就是用户眼下的剪贴板，连上就通告。
    Announce,
    /// 客户端的旧内容只记下计数，免得一连上就覆盖服务端。
    Quiet,
    /// 数据连接恢复时，当前有光标的一方才是权威来源，不再重放首次连接的旧内容。
    Resume,
}

/// 和哪台设备同步，结果报告给谁，单项多大以内才同步。
#[derive(Clone)]
pub(crate) struct Reporter {
    pub(crate) peer: Fingerprint,
    pub(crate) events: Events,
    pub(crate) limit: ClipLimit,
}

impl Reporter {
    fn skipped(&self, to_peer: bool, format: ClipboardFormat, bytes: u64, limit: u64) {
        self.events.emit(Event::ClipboardSkipped {
            peer: self.peer,
            to_peer,
            format,
            bytes,
            limit,
        });
    }
}

/// 在数据连接上启动剪贴板同步。`active` 表示对这台设备而言本机是不是活动端：
/// 客户端上是光标是否在本机，服务端上是光标是否不在这台设备上。
///
/// 数据连接比输入连接晚建立，期间光标可能已经切走，
/// 所以连上时是否通告由 `start` 决定，而不看这一刻谁是活动端。
/// `sync` 为假（没有协商剪贴板能力）时只收发文件控制消息。
pub(crate) fn spawn<S, C>(
    framed: Framed<S>,
    clipboard: SharedClipboard<C>,
    active: watch::Receiver<bool>,
    start: Start,
    report: Reporter,
    sync: bool,
    files: Option<FileLink>,
) -> ClipboardTask
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    C: Clipboard,
{
    ClipboardTask(tokio::spawn(async move {
        let mut sync = ClipboardSync::new(framed, clipboard, active, report, sync, files);
        match sync.run(start).await {
            Ok(()) => debug!("数据连接已关闭"),
            Err(e) => warn!("剪贴板同步中断：{e}"),
        }
    }))
}

struct ClipboardSync<S, C> {
    framed: Framed<S>,
    clipboard: SharedClipboard<C>,
    active: watch::Receiver<bool>,
    /// 最近一次看到的本机剪贴板变化计数。
    seen: Option<u64>,
    /// 最近一次通告出去的本机内容，等对方来拉取。拉走一种格式就放掉一种，大图片不长期占着内存。
    offered: Option<(u32, ClipboardContent)>,
    next_seq: u32,
    /// 对方最近一次通告、还没拉取的内容。
    offer: Option<ClipboardOffer>,
    fetch: Option<Fetch>,
    next_transfer: u32,
    report: Reporter,
    /// 最近一次报告“已被对方拉走”的通告序号，同一份内容的几个格式只报告一次。
    reported: Option<u32>,
    /// 是否同步剪贴板。
    sync: bool,
    /// 文件传输的控制消息。
    files: Option<FileLink>,
}

/// 正在拉取的一份对方内容。各格式都到齐后一次写入本机剪贴板。
struct Fetch {
    seq: u32,
    /// 传输号 → 格式、通告的大小、已收到的数据。
    pending: HashMap<u32, (ClipboardFormat, u64, Vec<u8>)>,
    done: ClipboardContent,
}

impl<S, C> ClipboardSync<S, C>
where
    S: AsyncRead + AsyncWrite + Unpin,
    C: Clipboard,
{
    fn new(
        framed: Framed<S>,
        clipboard: SharedClipboard<C>,
        active: watch::Receiver<bool>,
        report: Reporter,
        sync: bool,
        files: Option<FileLink>,
    ) -> Self {
        Self {
            framed,
            clipboard,
            active,
            seen: None,
            offered: None,
            next_seq: 1,
            offer: None,
            fetch: None,
            next_transfer: 1,
            report,
            reported: None,
            sync,
            files,
        }
    }

    async fn run(&mut self, start: Start) -> Result<()> {
        let count = self.clipboard.count().await?;
        self.seen = Some(count);
        if self.sync && (start == Start::Announce || (start == Start::Resume && self.is_active())) {
            self.offer_local().await?;
        }

        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                message = self.framed.recv() => match message? {
                    None | Some(Message::Bye(_)) => return Ok(()),
                    Some(message) => self.on_message(message).await?,
                },
                changed = self.active.changed() => {
                    if changed.is_err() {
                        return Ok(()); // 会话已结束
                    }
                    if !self.sync {
                        continue;
                    }
                    if self.is_active() {
                        self.start_fetch().await?;
                    } else {
                        self.poll().await?;
                    }
                }
                _ = tick.tick() => {
                    if self.sync && self.is_active() {
                        self.poll().await?;
                    }
                }
                Some(message) = next_file_message(&mut self.files) => {
                    send(&mut self.framed, &message).await?;
                }
            }
        }
    }

    async fn on_message(&mut self, message: Message) -> Result<()> {
        if !self.sync
            && matches!(
                &message,
                Message::ClipboardOffer(_)
                    | Message::ClipboardRequest(_)
                    | Message::Chunk(_)
                    | Message::Cancel(_)
            )
        {
            return Err(Error::Protocol("未协商剪贴板同步"));
        }
        match message {
            Message::Ping { nonce } => send(&mut self.framed, &Message::Pong { nonce }).await?,
            Message::ClipboardOffer(offer) => {
                debug!(seq = offer.seq, items = ?offer.items, "对方剪贴板有新内容");
                // 新通告让正在拉取的旧内容作废；迟到的旧分块按未知传输号丢弃。
                self.fetch = None;
                self.offer = Some(offer);
                if self.is_active() {
                    self.start_fetch().await?;
                }
            }
            Message::ClipboardRequest(request) => self.serve(request).await?,
            Message::Chunk(chunk) => self.on_chunk(chunk).await?,
            Message::Cancel(cancel) => {
                if let Some(fetch) = self.fetch.as_mut()
                    && fetch.pending.remove(&cancel.transfer).is_some()
                {
                    debug!(reason = cancel.reason.0, "对方取消了一个格式的传输");
                    self.finish_if_done().await?;
                }
            }
            message @ (Message::FileOffer(_)
            | Message::FileAccept { .. }
            | Message::FileCancel(_)) => match &self.files {
                Some(files) => files.on_control(message),
                None => return Err(Error::Protocol("未协商文件传输")),
            },
            other => debug!(
                kind = format_args!("{:#06x}", other.type_id()),
                "数据连接上忽略消息"
            ),
        }
        Ok(())
    }

    /// 本机剪贴板变了就通告。
    async fn poll(&mut self) -> Result<()> {
        if Some(self.clipboard.count().await?) != self.seen {
            self.offer_local().await?;
        }
        Ok(())
    }

    async fn offer_local(&mut self) -> Result<()> {
        let (count, content) = self.clipboard.read().await?;
        let content = match content {
            Ok(content) => content,
            Err(error) => {
                // 失败不是一个新的空版本；保留重试机会，并拒绝对旧内容的迟到请求。
                self.seen = None;
                self.offered = None;
                warn!("读取本机剪贴板失败：{error}");
                return Ok(());
            }
        };
        self.seen = Some(count);

        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        // 太大的格式不通告，也不留在内存里。
        let limit = self.report.limit.get();
        let mut content = content;
        let too_large: Vec<(ClipboardFormat, u64)> = content
            .iter()
            .map(|(format, data)| (format, data.len() as u64))
            .filter(|&(_, size)| size > limit)
            .collect();
        for &(format, size) in &too_large {
            content.remove_shared(format);
            info!(?format, size, limit, "剪贴板内容超过大小上限，没有通告");
            self.report.skipped(true, format, size, limit);
        }
        // 没有可共享的格式也照样通告（空列表），让对方丢掉之前的旧通告。
        let items = content
            .iter()
            .map(|(format, data)| ClipboardItem {
                format,
                size: data.len() as u64,
            })
            .collect();
        let offer = ClipboardOffer { seq, items };
        debug!(seq, items = ?offer.items, "通告本机剪贴板");
        self.offered = Some((seq, content));
        send(&mut self.framed, &Message::ClipboardOffer(offer)).await
    }

    /// 拉取对方通告中本机支持的格式。
    async fn start_fetch(&mut self) -> Result<()> {
        if self.fetch.is_some() {
            return Ok(());
        }
        let Some(offer) = self.offer.take() else {
            return Ok(());
        };
        let supported = self.clipboard.formats;
        let limit = self.report.limit.get();
        let mut fetch = Fetch {
            seq: offer.seq,
            pending: HashMap::new(),
            done: ClipboardContent::new(),
        };
        for item in offer.items {
            if !supported.contains(&item.format) {
                continue;
            }
            if item.size > limit {
                info!(format = ?item.format, size = item.size, limit, "对方的剪贴板内容超过大小上限，没有拉取");
                self.report.skipped(false, item.format, item.size, limit);
                continue;
            }
            let transfer = self.next_transfer;
            self.next_transfer = self.next_transfer.wrapping_add(1);
            let request = ClipboardRequest {
                seq: offer.seq,
                format: item.format,
                transfer,
            };
            send(&mut self.framed, &Message::ClipboardRequest(request)).await?;
            fetch
                .pending
                .insert(transfer, (item.format, item.size, Vec::new()));
        }
        if !fetch.pending.is_empty() {
            self.fetch = Some(fetch);
        }
        Ok(())
    }

    /// 回应对方的拉取：按块发出内容，发完就放掉这种格式。过期的请求回 `Cancel(STALE)`。
    async fn serve(&mut self, request: ClipboardRequest) -> Result<()> {
        let current = matches!(&self.offered, Some((seq, _)) if *seq == request.seq);
        if current
            && self.reported != Some(request.seq)
            && let Some((_, content)) = &self.offered
        {
            // 对方一次请求全部格式，第一个请求到的时候内容还是完整的。
            self.reported = Some(request.seq);
            self.report.events.emit(Event::Clipboard {
                peer: self.report.peer,
                to_peer: true,
                summary: ClipSummary::of(content),
            });
        }
        let data = match &mut self.offered {
            Some((_, content)) if current => content.remove_shared(request.format),
            _ => None,
        };
        let Some(data) = data else {
            let cancel = Cancel {
                transfer: request.transfer,
                reason: CancelReason::STALE,
            };
            return send(&mut self.framed, &Message::Cancel(cancel)).await;
        };

        // 空内容也要发一块 last = true，对方才知道传完了。
        let count = data.len().div_ceil(MAX_CHUNK_LEN).max(1);
        for (i, part) in data
            .chunks(MAX_CHUNK_LEN)
            .chain(data.is_empty().then_some(&[][..]))
            .enumerate()
        {
            let chunk = Chunk {
                transfer: request.transfer,
                last: i + 1 == count,
                data: part.to_vec(),
            };
            send(&mut self.framed, &Message::Chunk(chunk)).await?;
        }
        Ok(())
    }

    async fn on_chunk(&mut self, chunk: Chunk) -> Result<()> {
        let Some(fetch) = self.fetch.as_mut() else {
            return Ok(());
        };
        let Some((_, size, data)) = fetch.pending.get_mut(&chunk.transfer) else {
            return Ok(()); // 已作废的传输
        };
        data.extend_from_slice(&chunk.data);
        if data.len() as u64 > *size {
            return Err(Error::Protocol("剪贴板内容超过了通告的大小"));
        }
        if chunk.last {
            let (format, _, data) = fetch.pending.remove(&chunk.transfer).expect("刚刚查到过");
            fetch.done.insert(format, data);
            self.finish_if_done().await?;
        }
        Ok(())
    }

    /// 所有格式都到齐（或被取消）后写入本机剪贴板。
    async fn finish_if_done(&mut self) -> Result<()> {
        if self.fetch.as_ref().is_some_and(|f| !f.pending.is_empty()) {
            return Ok(());
        }
        let Some(fetch) = self.fetch.take() else {
            return Ok(());
        };
        if fetch.done.is_empty() {
            return Ok(());
        }
        let summary = ClipSummary::of(&fetch.done);
        let written = self.clipboard.write(fetch.done).await;
        match written {
            Ok(count) => {
                self.seen = Some(count);
                info!(seq = fetch.seq, bytes = summary.bytes, "已同步对方的剪贴板");
                self.report.events.emit(Event::Clipboard {
                    peer: self.report.peer,
                    to_peer: false,
                    summary,
                });
            }
            Err(e) => warn!("写入本机剪贴板失败：{e}"),
        }
        Ok(())
    }

    fn is_active(&self) -> bool {
        *self.active.borrow()
    }
}

/// 下一条要发出的文件控制消息；没有文件传输时永远等待。
async fn next_file_message(files: &mut Option<FileLink>) -> Option<Message> {
    match files {
        Some(files) => files.next().await,
        None => std::future::pending().await,
    }
}

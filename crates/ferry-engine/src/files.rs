//! 文件传输：把若干文件和文件夹交给对方。设计见 ADR 0005，线上格式见 `docs/protocol.md` 的“文件”一节。
//!
//! [`Files`] 是一台设备上所有传输的控制中心，服务端和客户端各一个，跨会话保留，
//! 断线重连后据此续传。控制消息（通告、接受、取消）走数据连接，
//! 内容走每次单独打开的文件连接，文件连接始终由客户端打开。
//!
//! 服务端可能同时连着几台设备：每台一条数据连接、一组任务，续传只对同一台设备有效（ADR 0006）。

mod budget;
mod commit;
mod manifest;
mod receive;
mod send;
pub use budget::FileRateLimit;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ferry_net::{ClientStream, Framed};
use ferry_platform::FileCommit;
use ferry_proto::{CancelReason, FileCancel, FileOffer, Message};
use ferry_store::Store;
use ferry_trust::Fingerprint;
use ferry_types::{TransferId, mark};
use ring::rand::{SecureRandom, SystemRandom};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Notify, Semaphore};
use tokio::time::timeout;
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::events::{Event, Events, TransferState, TransferUpdate};
use manifest::Plan;

/// 服务端等客户端在新的文件连接上说明用途的时间。
const OPEN_WAIT: Duration = Duration::from_secs(10);

/// 会话还在、文件连接却断了时，发送方在本会话内最多重试几次。
const MAX_RETRIES: u32 = 3;

/// 传输中报告进度的最短间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const RETRY_DELAY: Duration = Duration::from_millis(500);
const MAX_JOBS: usize = 64;
const MAX_PEER_JOBS: usize = 8;
const MAX_CANCELLED: usize = 512;
const RECEIPT_TTL: u64 = 7 * 24 * 3600;
const MAX_RECEIPTS: usize = 1024;

/// 客户端打开一条已完成握手的文件连接。
pub(crate) type Opener = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Framed<ClientStream>>> + Send>> + Send + Sync,
>;

/// 一台设备上所有文件传输的控制中心。克隆后共用同一份状态。
#[derive(Clone)]
pub struct Files {
    inner: Arc<Inner>,
}

struct Inner {
    /// 正在接收的文件放在这里，每次传输一个子目录。
    partial: PathBuf,
    /// 收完的文件放在这里。
    downloads: PathBuf,
    events: Events,
    state: Mutex<State>,
    commit: Arc<dyn FileCommit>,
    scans: Arc<Semaphore>,
    rate: FileRateLimit,
    cleanup: Arc<Semaphore>,
}

#[derive(Default)]
struct State {
    /// 每台连着的设备一条数据连接。
    links: HashMap<Fingerprint, Link>,
    /// 每次连上数据连接加一，旧连接迟到的断开据此识别。
    generation: u64,
    jobs: HashMap<TransferId, Job>,
    cancelled: VecDeque<(TransferId, Fingerprint)>,
    /// 已保留给回执清理的目录；磁盘操作不持有state锁，新任务只能登记等待。
    cleaning: HashSet<(Fingerprint, TransferId)>,
}

impl State {
    /// 发给 `peer` 的控制消息的去处；没连着时为 `None`。
    fn tx(&self, peer: &Fingerprint) -> Option<UnboundedSender<Message>> {
        self.links.get(peer).map(|link| link.tx.clone())
    }
}

/// 原生磁盘操作即使在调用方取消后仍可能继续，保留标记必须由实际工作线程释放。
/// Drop也覆盖平台错误和panic；运行时只会短暂等待内存状态，绝不等清理磁盘。
struct ReceiptReservation {
    files: Files,
    peer: Fingerprint,
    id: TransferId,
}

impl ReceiptReservation {
    fn acquire(files: &Files, peer: Fingerprint, id: TransferId) -> Option<Self> {
        let mut state = files.lock();
        if state.jobs.get(&id).is_some_and(|job| job.peer == peer)
            || !state.cleaning.insert((peer, id))
        {
            return None;
        }
        Some(Self {
            files: files.clone(),
            peer,
            id,
        })
    }
}

impl Drop for ReceiptReservation {
    fn drop(&mut self) {
        let mut state = self.files.lock();
        state.cleaning.remove(&(self.peer, self.id));
        if let Some(job) = state.jobs.get_mut(&self.id)
            && job.peer == self.peer
            && matches!(job.kind, JobKind::Receive(_))
            && job.task.is_none()
        {
            // 已有有界watch任务负责接受和打开连接；清理等待不消耗网络重试预算。
            job.retry_at = Instant::now();
        }
    }
}

/// 一台设备当前会话的数据连接。
struct Link {
    generation: u64,
    /// 要在数据连接上发出的控制消息。
    tx: UnboundedSender<Message>,
    /// 客户端打开文件连接的方式；服务端为 `None`，等客户端连过来。
    opener: Option<Opener>,
}

struct Job {
    /// 对方。
    peer: Fingerprint,
    kind: JobKind,
    /// 正在传输时的任务。
    task: Option<Cancellation>,
    /// 本会话内因文件连接断开而重试的次数。
    retries: u32,
    attempt: u64,
    retry_at: Instant,
    committed: bool,
    cancelled_by_peer: bool,
}

/// 网络等待可立即取消；阻塞的复制/提交协作退出，结束前不会清理它正在使用的目录。
#[derive(Clone, Default)]
pub(crate) struct Cancellation(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    // 0：运行；1：断线；2：取消；3：已提交，之后的取消不能撤销成功。
    state: AtomicU8,
    changed: Notify,
}

impl Cancellation {
    fn request(&self, state: u8) -> bool {
        let mut old = self.0.state.load(Ordering::Acquire);
        loop {
            if old == 3 {
                return false;
            }
            match self.0.state.compare_exchange_weak(
                old,
                old.max(state),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.0.changed.notify_waiters();
                    return true;
                }
                Err(now) => old = now,
            }
        }
    }

    pub(crate) fn check(&self) -> Result<(), Stop> {
        match self.0.state.load(Ordering::Acquire) {
            1 => Err(Stop::Net("文件连接已关闭".into())),
            2 => Err(Stop::Cancelled),
            _ => Ok(()),
        }
    }

    pub(crate) fn commit(&self) -> Result<(), Stop> {
        match self
            .0
            .state
            .compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) | Err(3) => Ok(()),
            Err(_) => self.check(),
        }
    }

    /// 原子改名已经成功：取消不能撤销已经公开的用户文件。
    pub(crate) fn published(&self) {
        self.0.state.store(3, Ordering::Release);
    }

    pub(crate) async fn wait<T>(&self, work: impl Future<Output = T>) -> Result<T, Stop> {
        let stopped = async {
            loop {
                let changed = self.0.changed.notified();
                if let Err(stop) = self.check() {
                    return stop;
                }
                changed.await;
            }
        };
        tokio::select! {
            biased;
            stop = stopped => Err(stop),
            result = work => Ok(result),
        }
    }
}

#[derive(Clone)]
enum JobKind {
    Send(Arc<Plan>),
    Receive(FileOffer),
}

impl JobKind {
    fn update(&self, peer: Fingerprint, done: u64, state: TransferState) -> TransferUpdate {
        match self {
            Self::Send(plan) => TransferUpdate {
                id: plan.id,
                peer,
                outgoing: true,
                name: plan.name.clone(),
                items: plan.items,
                files: plan.files,
                bytes: plan.bytes,
                done,
                state,
            },
            Self::Receive(offer) => TransferUpdate {
                id: offer.id,
                peer,
                outgoing: false,
                name: offer.name.clone(),
                items: offer.items,
                files: offer.files,
                bytes: offer.bytes,
                done,
                state,
            },
        }
    }
}

/// 一次传输为什么停下。
pub(crate) enum Stop {
    /// 连接断了：发送方保留任务，接收方保留已收到的部分，之后接着传。
    Net(String),
    /// 出错了，不再继续。
    Fail(String),
    Cancelled,
}

impl From<ferry_net::Error> for Stop {
    fn from(e: ferry_net::Error) -> Self {
        match e {
            ferry_net::Error::Decode(_)
            | ferry_net::Error::Encode(_)
            | ferry_net::Error::UnexpectedMessage { .. } => {
                Self::Fail("对方的文件传输消息不符合协议".into())
            }
            _ => Self::Net(e.to_string()),
        }
    }
}

impl From<Error> for Stop {
    fn from(e: Error) -> Self {
        Self::Net(e.to_string())
    }
}

impl Files {
    /// 收到的文件放在 `store` 的下载目录，接收中的部分放在它的 `partial/`。
    pub fn new(store: &Store, events: Events, commit: Arc<dyn FileCommit>) -> Self {
        let files = Self {
            inner: Arc::new(Inner {
                partial: store.partial_dir(),
                downloads: store.downloads_dir(),
                events,
                state: Mutex::new(State::default()),
                commit,
                scans: Arc::new(Semaphore::new(2)),
                rate: FileRateLimit::default(),
                cleanup: Arc::new(Semaphore::new(1)),
            }),
        };
        files.schedule_cleanup();
        files.maintain_receipts();
        files
    }

    /// 所有设备、所有文件发送任务共用的上限，单位字节/秒，0 不限。立即影响排队中的任务。
    pub fn set_rate_limit(&self, bytes_per_second: u64) {
        self.inner.rate.set(bytes_per_second);
    }

    pub fn rate_limit(&self) -> u64 {
        self.inner.rate.get()
    }

    /// 清理超过7天或最近1024份之外的已完成回执。窗口以先到者为准。
    /// 活动任务、含未完成意图或内容的目录不清理；不删除下载目录中的文件。
    pub async fn prune_receipts(&self) -> std::result::Result<usize, String> {
        let Ok(permit) = self.inner.cleanup.clone().try_acquire_owned() else {
            return Ok(0);
        };
        let files = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut candidates = Vec::new();
            let peers = match std::fs::read_dir(&files.inner.partial) {
                Ok(peers) => peers,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
                Err(_) => return Err("无法读取完成回执缓存".into()),
            };
            for peer in peers.flatten() {
                if !peer.file_type().is_ok_and(|ty| ty.is_dir()) {
                    continue;
                }
                let Ok(fingerprint) = peer.file_name().to_string_lossy().parse::<Fingerprint>()
                else {
                    continue;
                };
                let Ok(entries) = std::fs::read_dir(peer.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if !entry.file_type().is_ok_and(|ty| ty.is_dir()) {
                        continue;
                    }
                    let path = entry.path();
                    let Ok(id) = entry.file_name().to_string_lossy().parse::<TransferId>() else {
                        continue;
                    };
                    if path.join("data").try_exists().unwrap_or(true)
                        || path.join(commit::INTENT).try_exists().unwrap_or(true)
                    {
                        continue;
                    }
                    if let Some(at) = commit::completed_at(&path)
                        && let Ok(identity) = files.inner.commit.identity(&path)
                        && let Ok(receipt_identity) =
                            files.inner.commit.identity(&path.join(commit::RECEIPT))
                    {
                        candidates.push((at, fingerprint, id, path, identity, receipt_identity));
                    }
                }
            }
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.3.cmp(&b.3)));
            let now = commit::now();
            let mut removed = 0;
            for (index, (at, peer, id, path, identity, receipt_identity)) in
                candidates.into_iter().enumerate()
            {
                if index < MAX_RECEIPTS && now.saturating_sub(at) < RECEIPT_TTL {
                    continue;
                }
                let Some(_reservation) = ReceiptReservation::acquire(&files, peer, id) else {
                    continue;
                };
                // 保留目录后再检查磁盘。目录被替换、出现未完成内容或意图时保守保留。
                // 新任务可以登记，但在_reservation释放前不能接受或启动恢复。
                if files.inner.commit.identity(&path).ok().as_ref() != Some(&identity)
                    || files
                        .inner
                        .commit
                        .identity(&path.join(commit::RECEIPT))
                        .ok()
                        .as_ref()
                        != Some(&receipt_identity)
                    || commit::completed_at(&path) != Some(at)
                    || path.join("data").try_exists().unwrap_or(true)
                    || path.join(commit::INTENT).try_exists().unwrap_or(true)
                {
                    continue;
                }
                if std::fs::remove_file(path.join(commit::RECEIPT)).is_ok() {
                    let _ = std::fs::remove_file(path.join("state"));
                    let _ = std::fs::remove_dir(&path);
                    removed += 1;
                }
            }
            Ok(removed)
        })
        .await
        .map_err(|_| "清理完成回执时出错".to_owned())?
    }

    fn schedule_cleanup(&self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let files = self.clone();
            runtime.spawn(async move {
                if let Err(error) = files.prune_receipts().await {
                    warn!(%error, "完成回执清理失败");
                }
            });
        }
    }

    fn maintain_receipts(&self) {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let inner = Arc::downgrade(&self.inner);
            runtime.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    let Some(inner) = inner.upgrade() else {
                        break;
                    };
                    let files = Files { inner };
                    if let Err(error) = files.prune_receipts().await {
                        warn!(%error, "完成回执定时清理失败");
                    }
                }
            });
        }
    }

    /// 把这些文件和文件夹发给连着的 `peer`，返回这次传输的编号。
    ///
    /// 展开文件夹需要读磁盘，在后台线程完成。对方会自动接受，进展经 [`Event::Transfer`] 报告。
    pub async fn send(&self, peer: Fingerprint, paths: Vec<PathBuf>) -> Result<TransferId> {
        if !self.lock().links.contains_key(&peer) {
            return Err(Error::NotConnected);
        }
        let id = new_id()?;
        self.check_budget(peer)?;
        let permit = self
            .inner
            .scans
            .clone()
            .try_acquire_owned()
            .map_err(|_| Error::Transfer("正在整理其他文件，请稍后再试".into()))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let _scan = ScanGuard(cancelled.clone());
        let plan = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            manifest::build(id, &paths, &cancelled)
        })
        .await
        .map_err(|_| Error::Transfer("整理要发送的文件时出错".into()))?
        .map_err(Error::Transfer)?;
        let plan = Arc::new(plan);
        let offer = plan.offer();
        {
            let mut state = self.lock();
            if !within_budget(&state, peer) {
                return Err(Error::Transfer("正在传输的任务太多，请稍后再试".into()));
            }
            let Some(tx) = state.tx(&peer) else {
                return Err(Error::NotConnected);
            };
            let _ = tx.send(Message::FileOffer(offer));
            state.jobs.insert(
                id,
                Job {
                    peer,
                    kind: JobKind::Send(plan.clone()),
                    task: None,
                    retries: 0,
                    attempt: 0,
                    retry_at: Instant::now() + OPEN_WAIT,
                    committed: false,
                    cancelled_by_peer: false,
                },
            );
        }
        info!(
            name = %mark::file(&plan.name),
            files = plan.files,
            bytes = plan.bytes,
            "准备发送文件"
        );
        self.emit(JobKind::Send(plan).update(peer, 0, TransferState::Waiting));
        self.watch(id);
        Ok(id)
    }

    /// 现在连着、可以收发文件的设备。
    pub fn connected(&self) -> Vec<Fingerprint> {
        self.lock().links.keys().copied().collect()
    }

    /// 取消一次传输。对方会收到通知；接收方删掉已收到的部分。
    pub fn cancel(&self, id: TransferId) {
        self.stop(id, false);
    }

    fn check_budget(&self, peer: Fingerprint) -> Result<()> {
        if within_budget(&self.lock(), peer) {
            Ok(())
        } else {
            Err(Error::Transfer("正在传输的任务太多，请稍后再试".into()))
        }
    }

    /// 与 `peer` 连上一条数据连接。之前没传给它的发送任务立即重新通告，接着传。
    ///
    /// 返回的 [`FileLink`] 交给数据连接的任务：它要发出 `outbound` 中的消息，
    /// 并把收到的文件控制消息交给 [`FileLink::on_control`]。丢弃它即表示数据连接已断开。
    pub(crate) fn attach(&self, peer: Fingerprint, opener: Option<Opener>) -> FileLink {
        let (tx, outbound) = mpsc::unbounded_channel();
        let (generation, resumed) = {
            let mut state = self.lock();
            state.generation += 1;
            let generation = state.generation;
            state.links.insert(
                peer,
                Link {
                    generation,
                    tx: tx.clone(),
                    opener,
                },
            );
            let resumed: Vec<Arc<Plan>> = state
                .jobs
                .values_mut()
                .filter(|job| job.peer == peer)
                .filter_map(|job| {
                    job.retries = 0;
                    job.retry_at = Instant::now() + RETRY_DELAY;
                    match &job.kind {
                        JobKind::Send(plan) => {
                            job.retries = 0;
                            Some(plan.clone())
                        }
                        JobKind::Receive(_) => None,
                    }
                })
                .collect();
            (generation, resumed)
        };
        for plan in resumed {
            info!(name = %mark::file(&plan.name), "重新连上，接着发送");
            let _ = tx.send(Message::FileOffer(plan.offer()));
            self.emit(JobKind::Send(plan).update(peer, 0, TransferState::Waiting));
        }
        FileLink {
            files: self.clone(),
            outbound,
            generation,
            peer,
        }
    }

    /// `peer` 的数据连接上收到的文件控制消息。
    fn on_control(&self, peer: Fingerprint, message: Message) {
        match message {
            Message::FileOffer(offer) => self.on_offer(peer, offer),
            Message::FileAccept { id } => self.on_accept(peer, id),
            Message::FileCancel(FileCancel { id, .. }) if self.belongs(id, peer) => {
                self.stop(id, true);
            }
            _ => {}
        }
    }

    /// 这次传输是不是和 `peer` 之间的。
    fn belongs(&self, id: TransferId, peer: Fingerprint) -> bool {
        let state = self.lock();
        state.jobs.get(&id).is_some_and(|job| job.peer == peer)
            || state.cancelled.contains(&(id, peer))
    }

    /// 服务端收到 `peer` 的一条文件连接（握手已完成）：等它说明是哪次传输，再开始收发。
    pub(crate) fn serve<S>(&self, peer: Fingerprint, mut framed: Framed<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let files = self.clone();
        tokio::spawn(async move {
            let id = match timeout(OPEN_WAIT, framed.recv()).await {
                Ok(Ok(Some(Message::FileOpen { id }))) => id,
                _ => {
                    warn!("新的文件连接没有说明是哪次传输，已关闭");
                    return;
                }
            };
            let idle = {
                let state = files.lock();
                !state.cleaning.contains(&(peer, id))
                    && state
                        .jobs
                        .get(&id)
                        .is_some_and(|j| j.task.is_none() && j.peer == peer)
            };
            if !idle {
                warn!(%id, "文件连接对应的传输不存在或已在进行，已关闭");
                return;
            }
            let worker = files.clone();
            files.launch(id, move |cancel| async move {
                worker.run(id, framed, cancel).await
            });
        });
    }

    fn on_offer(&self, peer: Fingerprint, offer: FileOffer) {
        let id = offer.id;
        let (tx, opener, new, cleaning) = {
            let mut state = self.lock();
            let Some(link) = state.links.get(&peer) else {
                return;
            };
            let found = (link.tx.clone(), link.opener.clone());
            let cleaning = state.cleaning.contains(&(peer, id));
            if state.cancelled.contains(&(id, peer)) {
                let _ = found.0.send(Message::FileCancel(FileCancel {
                    id,
                    reason: CancelReason::CANCELLED,
                }));
                return;
            }
            if let Some(job) = state.jobs.get(&id) {
                if job.peer != peer || !matches!(&job.kind, JobKind::Receive(old) if *old == offer)
                {
                    let _ = found.0.send(Message::FileCancel(FileCancel {
                        id,
                        reason: CancelReason::UNAVAILABLE,
                    }));
                    return;
                }
                // 相同通告可以重发：重新确认，正在运行的任务不会启动第二份。
                (found.0, found.1, false, cleaning)
            } else {
                if !within_budget(&state, peer) {
                    let _ = found.0.send(Message::FileCancel(FileCancel {
                        id,
                        reason: CancelReason::UNAVAILABLE,
                    }));
                    return;
                }
                state.jobs.insert(
                    id,
                    Job {
                        peer,
                        kind: JobKind::Receive(offer.clone()),
                        task: None,
                        retries: 0,
                        attempt: 0,
                        retry_at: Instant::now() + OPEN_WAIT,
                        committed: false,
                        cancelled_by_peer: false,
                    },
                );
                (found.0, found.1, true, cleaning)
            }
        };
        if new {
            info!(name = %mark::file(&offer.name), files = offer.files, bytes = offer.bytes, "对方发来文件，开始接收");
            self.emit(JobKind::Receive(offer).update(peer, 0, TransferState::Waiting));
            self.watch(id);
        }
        if cleaning {
            return;
        }
        let _ = tx.send(Message::FileAccept { id });
        if let Some(opener) = opener {
            self.open_and_run(id, opener);
        }
    }

    fn on_accept(&self, peer: Fingerprint, id: TransferId) {
        let opener = {
            let state = self.lock();
            let sending = state.jobs.get(&id).is_some_and(|j| {
                j.peer == peer && j.task.is_none() && matches!(j.kind, JobKind::Send(_))
            });
            match state.links.get(&peer) {
                Some(link) if sending => link.opener.clone(),
                _ => None,
            }
        };
        // 服务端没有 opener：等客户端打开文件连接。
        if let Some(opener) = opener {
            self.open_and_run(id, opener);
        }
    }

    /// 客户端：打开文件连接并开始收发。
    fn open_and_run(&self, id: TransferId, opener: Opener) {
        let files = self.clone();
        self.launch(id, move |cancel| async move {
            let mut framed = cancel.wait(opener()).await??;
            send::send(&mut framed, &Message::FileOpen { id }, &cancel).await?;
            files.run(id, framed, cancel).await
        });
    }

    async fn run<S>(
        &self,
        id: TransferId,
        mut framed: Framed<S>,
        cancel: Cancellation,
    ) -> Result<Vec<PathBuf>, Stop>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let job = self
            .lock()
            .jobs
            .get(&id)
            .map(|job| (job.peer, job.kind.clone()));
        let Some((peer, kind)) = job else {
            return Err(Stop::Fail("这次传输已经不存在".into()));
        };
        let mut progress = Progress::new(self.inner.events.clone(), peer, kind.clone());
        match &kind {
            JobKind::Send(plan) => {
                send::run(&mut framed, plan, &mut progress, &cancel, &self.inner.rate).await
            }
            JobKind::Receive(offer) => {
                receive::run(
                    &mut framed,
                    offer,
                    &self.inner.partial.join(peer.to_hex()),
                    &self.inner.downloads,
                    &mut progress,
                    self.inner.commit.clone(),
                    &cancel,
                )
                .await
            }
        }
    }

    /// 在后台运行一次传输，结束后交给 [`Files::finish`]。
    ///
    /// 任务先记下自己的句柄再开工，取消时一定能找到它。
    fn launch<F>(&self, id: TransferId, work: impl FnOnce(Cancellation) -> F + Send + 'static)
    where
        F: Future<Output = Result<Vec<PathBuf>, Stop>> + Send + 'static,
    {
        let cancel = Cancellation::default();
        let attempt = {
            let mut state = self.lock();
            if state
                .jobs
                .get(&id)
                .is_some_and(|job| state.cleaning.contains(&(job.peer, id)))
            {
                return;
            }
            let Some(job) = state.jobs.get_mut(&id).filter(|job| job.task.is_none()) else {
                return;
            };
            job.attempt += 1;
            job.task = Some(cancel.clone());
            job.attempt
        };
        let files = self.clone();
        tokio::spawn(async move {
            let outcome = work(cancel).await;
            files.finish(id, attempt, outcome);
        });
    }

    fn finish(&self, id: TransferId, attempt: u64, outcome: Result<Vec<PathBuf>, Stop>) {
        let cancelled = {
            let mut state = self.lock();
            let Some(job) = state.jobs.get_mut(&id).filter(|j| j.attempt == attempt) else {
                return;
            };
            job.committed |= job
                .task
                .as_ref()
                .is_some_and(|c| c.0.state.load(Ordering::Acquire) == 3);
            job.task
                .as_ref()
                .is_some_and(|c| matches!(c.check(), Err(Stop::Cancelled)))
        };
        let outcome = if cancelled {
            Err(Stop::Cancelled)
        } else {
            outcome
        };
        match outcome {
            Ok(saved) => {
                let Some(job) = self.lock().jobs.remove(&id) else {
                    return;
                };
                let total = match &job.kind {
                    JobKind::Send(plan) => plan.bytes,
                    JobKind::Receive(offer) => offer.bytes,
                };
                match saved.first() {
                    Some(first) => info!(at = %mark::path(first), "文件收好了"),
                    None => info!("文件发送完成"),
                }
                self.emit(
                    job.kind
                        .update(job.peer, total, TransferState::Done { saved }),
                );
                self.schedule_cleanup();
            }
            Err(Stop::Net(reason)) => {
                let (peer, kind, exhausted) = {
                    let mut state = self.lock();
                    let Some(peer) = state.jobs.get(&id).map(|job| job.peer) else {
                        return;
                    };
                    let online = state.links.contains_key(&peer);
                    let Some(job) = state.jobs.get_mut(&id) else {
                        return;
                    };
                    job.task = None;
                    let kind = job.kind.clone();
                    if online {
                        job.retries += 1;
                    }
                    job.retry_at = Instant::now() + RETRY_DELAY;
                    (peer, kind, online && job.retries > MAX_RETRIES)
                };
                warn!(%reason, "文件传输中断");
                self.emit(kind.update(peer, 0, TransferState::Interrupted));
                if exhausted {
                    self.finish(
                        id,
                        attempt,
                        Err(Stop::Fail("文件连接多次失败，请重新发送".into())),
                    );
                }
            }
            Err(Stop::Fail(reason)) => {
                let (job, tx) = {
                    let mut state = self.lock();
                    let job = state.jobs.remove(&id);
                    let tx = job.as_ref().and_then(|job| state.tx(&job.peer));
                    (job, tx)
                };
                let Some(job) = job else {
                    return;
                };
                warn!(%reason, "文件传输失败");
                if let Some(tx) = tx {
                    let cancel = FileCancel {
                        id,
                        reason: CancelReason::UNAVAILABLE,
                    };
                    let _ = tx.send(Message::FileCancel(cancel));
                }
                if matches!(job.kind, JobKind::Receive(_)) {
                    self.discard_partial(job.peer, id);
                }
                self.emit(
                    job.kind
                        .update(job.peer, 0, TransferState::Failed { reason }),
                );
            }
            Err(Stop::Cancelled) => {
                let Some(job) = self.lock().jobs.remove(&id) else {
                    return;
                };
                self.finish_cancel(id, job.peer, job.kind, job.cancelled_by_peer);
            }
        }
    }

    /// 停止并忘掉一次传输。`by_peer` 表示是对方取消的，不必再通知对方。
    fn stop(&self, id: TransferId, by_peer: bool) {
        let (peer, kind, working, tx) = {
            let mut state = self.lock();
            let Some(job) = state.jobs.get(&id) else {
                return;
            };
            if job.committed {
                return;
            }
            if job.task.as_ref().is_some_and(|task| !task.request(2)) {
                return;
            }
            let peer = job.peer;
            let kind = job.kind.clone();
            let working = job.task.is_some();
            let tx = state.tx(&peer);
            if !working {
                state.jobs.remove(&id);
            } else if let Some(job) = state.jobs.get_mut(&id) {
                job.cancelled_by_peer = by_peer;
            }
            state.cancelled.push_back((id, peer));
            while state.cancelled.len() > MAX_CANCELLED {
                state.cancelled.pop_front();
            }
            (peer, kind, working, tx)
        };
        if !by_peer && let Some(tx) = tx {
            let cancel = FileCancel {
                id,
                reason: CancelReason::CANCELLED,
            };
            let _ = tx.send(Message::FileCancel(cancel));
        }
        if !working {
            self.finish_cancel(id, peer, kind, by_peer);
        }
    }

    /// 与 `peer` 的数据连接断开：停下和它之间正在进行的传输。发送任务留着，下次连上接着传。
    fn detach(&self, peer: Fingerprint, generation: u64) {
        let stopped: Vec<JobKind> = {
            let mut state = self.lock();
            if state
                .links
                .get(&peer)
                .is_none_or(|l| l.generation != generation)
            {
                return;
            }
            state.links.remove(&peer);
            state
                .jobs
                .values_mut()
                .filter(|job| job.peer == peer)
                .map(|job| {
                    if let Some(task) = &job.task {
                        task.request(1);
                    }
                    job.kind.clone()
                })
                .collect()
        };
        for kind in stopped {
            self.emit(kind.update(peer, 0, TransferState::Interrupted));
        }
    }

    /// 删掉一次接收已经收到的部分。
    fn discard_partial(&self, peer: Fingerprint, id: TransferId) {
        let dir = self.inner.partial.join(peer.to_hex()).join(id.to_string());
        tokio::task::spawn_blocking(move || {
            // 已完成回执是幂等性的依据，后续错误或迟到取消不能删除它。
            if !dir.join("committed.json").exists() && !dir.join(commit::INTENT).exists() {
                let _ = std::fs::remove_dir_all(dir);
            }
        });
    }

    fn finish_cancel(&self, id: TransferId, peer: Fingerprint, kind: JobKind, by_peer: bool) {
        let dir = self.inner.partial.join(peer.to_hex()).join(id.to_string());
        let events = self.inner.events.clone();
        let platform = self.inner.commit.clone();
        tokio::task::spawn_blocking(move || {
            let mut state = TransferState::Cancelled { by_peer };
            if matches!(kind, JobKind::Receive(_))
                && dir.join(commit::INTENT).exists()
                && !matches!(commit::abandon(&dir, &*platform), Ok(true))
            {
                state = TransferState::Failed {
                    reason: "传输已停止，但已有提交记录需恢复；内容已安全保留".into(),
                };
            }
            if matches!(kind, JobKind::Receive(_))
                && !dir.join("committed.json").exists()
                && !dir.join(commit::INTENT).exists()
                && let Err(e) = std::fs::remove_dir_all(dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                state = TransferState::Failed {
                    reason: "传输已停止，但无法删除接收缓存，请检查写入权限".into(),
                };
            }
            info!(%id, by_peer, "文件传输已停止并完成清理");
            events.emit(Event::Transfer(kind.update(peer, 0, state)));
        });
    }

    /// 等待确认/文件连接也有期限；双方都能重试，重复消息不会生成第二个工作任务。
    fn watch(&self, id: TransferId) {
        let inner = Arc::downgrade(&self.inner);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RETRY_DELAY).await;
                let Some(inner) = inner.upgrade() else {
                    break;
                };
                let files = Files { inner };
                if !files.retry(id) {
                    break;
                }
            }
        });
    }

    fn retry(&self, id: TransferId) -> bool {
        let action = {
            let mut state = self.lock();
            let Some(job) = state.jobs.get(&id) else {
                return false;
            };
            if state.cleaning.contains(&(job.peer, id)) {
                return true;
            }
            if job.task.is_some() || Instant::now() < job.retry_at {
                return true;
            }
            let peer = job.peer;
            let Some(link) = state.links.get(&peer) else {
                return true;
            };
            let (tx, opener) = (link.tx.clone(), link.opener.clone());
            let job = state.jobs.get_mut(&id).expect("刚刚找到任务");
            if job.retries > MAX_RETRIES {
                Some((job.attempt, None, None))
            } else {
                job.retries += 1;
                job.retry_at = Instant::now() + OPEN_WAIT;
                let message = match &job.kind {
                    JobKind::Send(plan) => Message::FileOffer(plan.offer()),
                    JobKind::Receive(_) => Message::FileAccept { id },
                };
                let opener = matches!(job.kind, JobKind::Receive(_))
                    .then_some(opener)
                    .flatten();
                Some((job.attempt, Some((tx, message)), opener))
            }
        };
        if let Some((attempt, message, opener)) = action {
            match message {
                Some((tx, message)) => {
                    let _ = tx.send(message);
                }
                None => self.finish(
                    id,
                    attempt,
                    Err(Stop::Fail("对方没有开始文件传输，请重新发送".into())),
                ),
            }
            if let Some(opener) = opener {
                self.open_and_run(id, opener);
            }
        }
        true
    }

    fn emit(&self, update: TransferUpdate) {
        self.inner.events.emit(Event::Transfer(update));
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 一条数据连接与文件传输之间的接口。丢弃即表示数据连接已断开。
pub(crate) struct FileLink {
    files: Files,
    outbound: UnboundedReceiver<Message>,
    generation: u64,
    peer: Fingerprint,
}

impl FileLink {
    /// 下一条要在数据连接上发出的控制消息。
    pub(crate) async fn next(&mut self) -> Option<Message> {
        self.outbound.recv().await
    }

    /// 数据连接上收到的文件控制消息。
    pub(crate) fn on_control(&self, message: Message) {
        self.files.on_control(self.peer, message);
    }
}

impl Drop for FileLink {
    fn drop(&mut self) {
        self.files.detach(self.peer, self.generation);
    }
}

/// 传输进度，限频报告。
pub(crate) struct Progress {
    events: Events,
    peer: Fingerprint,
    kind: JobKind,
    done: u64,
    last: Instant,
}

impl Progress {
    fn new(events: Events, peer: Fingerprint, kind: JobKind) -> Self {
        Self {
            events,
            peer,
            kind,
            done: 0,
            last: Instant::now(),
        }
    }

    /// 开始传内容。`done` 是续传前已经传过的字节数。
    pub(crate) fn start(&mut self, done: u64) {
        self.done = done;
        self.report();
    }

    pub(crate) fn add(&mut self, bytes: u64) {
        self.done += bytes;
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            self.report();
        }
    }

    fn report(&mut self) {
        self.last = Instant::now();
        let update = self
            .kind
            .update(self.peer, self.done, TransferState::Running);
        self.events.emit(Event::Transfer(update));
    }
}

fn new_id() -> Result<TransferId> {
    let mut id = [0u8; 16];
    SystemRandom::new()
        .fill(&mut id)
        .map_err(|_| Error::Transfer("系统随机数不可用".into()))?;
    Ok(TransferId(id))
}

fn within_budget(state: &State, peer: Fingerprint) -> bool {
    state.jobs.len() < MAX_JOBS
        && state.jobs.values().filter(|job| job.peer == peer).count() < MAX_PEER_JOBS
}

struct ScanGuard(Arc<AtomicBool>);

impl Drop for ScanGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

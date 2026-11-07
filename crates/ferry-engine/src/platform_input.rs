//! 串行平台输入工作线程。原生光标调用等待不占用网络运行时，确认后才推进输入状态。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use ferry_core::Pressed;
use ferry_platform::{CaptureEvent, CaptureSink, InputCapture, InputInject};
use ferry_types::{InputEvent, KeyAction, LockKeys, Point};
use tokio::sync::{Semaphore, mpsc, oneshot};

type Reply<T> = oneshot::Sender<ferry_platform::Result<T>>;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// 已开始的原生调用不能强行取消；完成后由同一线程释放资源，不在Drop中join。
/// 通道至多一项，所有方法独占借用，按键/定位/显示的顺序不变。
pub(crate) struct CaptureWorker {
    requests: mpsc::Sender<CaptureWork>,
    failed: bool,
}

enum CaptureWork {
    Grab(Reply<LockKeys>),
    Release(Point, Reply<()>),
}

struct CaptureGuard<C: InputCapture> {
    capture: C,
    return_at: Arc<AtomicU64>,
    grabbed: bool,
}

impl<C: InputCapture> CaptureGuard<C> {
    fn restore(&mut self) {
        if self.grabbed
            && self
                .capture
                .release(unpack(self.return_at.load(Ordering::Relaxed)))
                .is_ok()
        {
            self.grabbed = false;
        }
    }
}

impl<C: InputCapture> Drop for CaptureGuard<C> {
    fn drop(&mut self) {
        self.restore();
    }
}

impl CaptureWorker {
    pub async fn start(
        scope: PathBuf,
        capture: impl InputCapture + 'static,
        mut sink: CaptureSink,
        fallback: Point,
    ) -> crate::Result<Self> {
        let permit = permit(scope).await?;
        let (requests, mut receiver) = mpsc::channel(1);
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new()
            .name("ferry-capture-control".into())
            .spawn(move || {
                let _permit = permit;
                let return_at = Arc::new(AtomicU64::new(pack(fallback)));
                let latest = return_at.clone();
                let mut guard = CaptureGuard {
                    capture,
                    return_at,
                    grabbed: false,
                };
                let result = guard.capture.start(Box::new(move |event| {
                    if !event.grabbed
                        && let CaptureEvent::Pointer { position, .. } = event.event
                    {
                        latest.store(pack(position), Ordering::Relaxed);
                    }
                    sink(event);
                }));
                let failed = result.is_err();
                if ready.send(result).is_err() || failed {
                    return;
                }
                while let Some(work) = receiver.blocking_recv() {
                    match work {
                        CaptureWork::Grab(reply) if !reply.is_closed() => {
                            // 失败也可能部分改变原生状态，必须尝试恢复。
                            guard.grabbed = true;
                            let result = guard.capture.grab().map(|()| guard.capture.lock_keys());
                            if result.is_err() {
                                guard.restore();
                            }
                            if reply.send(result).is_err() {
                                guard.restore();
                            }
                        }
                        CaptureWork::Release(at, reply) => {
                            guard.return_at.store(pack(at), Ordering::Relaxed);
                            let result = guard.capture.release(at);
                            if result.is_ok() {
                                guard.grabbed = false;
                            }
                            let _ = reply.send(result);
                        }
                        _ => {}
                    }
                }
            })?;
        tokio::time::timeout(CONFIRM_TIMEOUT, started)
            .await
            .map_err(|_| timed_out())?
            .map_err(|_| closed())??;
        Ok(Self {
            requests,
            failed: false,
        })
    }

    pub async fn grab(&mut self) -> ferry_platform::Result<LockKeys> {
        if self.failed {
            return Err(closed());
        }
        let (reply, done) = oneshot::channel();
        self.requests
            .send(CaptureWork::Grab(reply))
            .await
            .map_err(|_| closed())?;
        let result = confirmed(done, &mut self.failed).await;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub async fn release(&mut self, at: Point) -> ferry_platform::Result<()> {
        if self.failed {
            return Err(closed());
        }
        let (reply, done) = oneshot::channel();
        self.requests
            .send(CaptureWork::Release(at, reply))
            .await
            .map_err(|_| closed())?;
        let result = confirmed(done, &mut self.failed).await;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    pub fn is_failed(&self) -> bool {
        self.failed || self.requests.is_closed()
    }
}

pub(crate) struct InjectWorker {
    requests: mpsc::Sender<InjectWork>,
    failed: bool,
}

enum InjectWork {
    Input(InputEvent, Reply<()>),
    Locks(LockKeys, LockKeys, Reply<()>),
    Hidden(bool, Reply<()>),
    Reset(Reply<()>),
}

struct InjectGuard<I: InputInject> {
    inject: I,
    pressed: Pressed,
    cursor: Point,
    hidden: bool,
}

impl<I: InputInject> InjectGuard<I> {
    fn restore(&mut self) -> ferry_platform::Result<()> {
        let mut failed = None;
        for event in self.pressed.release_all(self.cursor) {
            if let Err(error) = self.inject.inject(&event) {
                // 不忘掉未成功释放的键，Drop还有一次恢复机会。
                let held = match event {
                    InputEvent::Key { code, .. } => InputEvent::Key {
                        code,
                        action: KeyAction::Down,
                    },
                    InputEvent::Button {
                        button, position, ..
                    } => InputEvent::Button {
                        button,
                        position,
                        pressed: true,
                    },
                    _ => event,
                };
                self.pressed.track(&held);
                failed.get_or_insert(error);
            }
        }
        if self.hidden {
            match self.inject.set_cursor_hidden(false) {
                Ok(()) => self.hidden = false,
                Err(error) => {
                    failed.get_or_insert(error);
                }
            }
        }
        failed.map_or(Ok(()), Err)
    }
}

impl<I: InputInject> Drop for InjectGuard<I> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

impl InjectWorker {
    pub async fn new(scope: PathBuf, inject: impl InputInject + 'static) -> crate::Result<Self> {
        let permit = permit(scope).await?;
        let (requests, mut receiver) = mpsc::channel(1);
        std::thread::Builder::new()
            .name("ferry-inject".into())
            .spawn(move || {
                let _permit = permit;
                let mut guard = InjectGuard {
                    inject,
                    pressed: Pressed::new(),
                    cursor: Point::default(),
                    hidden: false,
                };
                while let Some(work) = receiver.blocking_recv() {
                    match work {
                        InjectWork::Input(event, reply) if !reply.is_closed() => {
                            let releasing = matches!(
                                event,
                                InputEvent::Key {
                                    action: KeyAction::Up,
                                    ..
                                } | InputEvent::Button { pressed: false, .. }
                            );
                            // Down失败也可能已经部分注入；Up失败不能忘掉清理责任。
                            if !releasing {
                                guard.pressed.track(&event);
                            }
                            match event {
                                InputEvent::PointerMotion { position } => guard.cursor = position,
                                InputEvent::PointerDelta { dx, dy } => {
                                    guard.cursor.x = guard.cursor.x.saturating_add(dx);
                                    guard.cursor.y = guard.cursor.y.saturating_add(dy);
                                }
                                _ => {}
                            }
                            let result = guard.inject.inject(&event);
                            if releasing && result.is_ok() {
                                guard.pressed.track(&event);
                            }
                            if matches!(result, Err(ferry_platform::Error::Unsupported(_)))
                                && !releasing
                            {
                                // Unsupported表示没有执行，不应为从未注入的Down合成清理Up。
                                match event {
                                    InputEvent::Key { code, .. } => {
                                        guard.pressed.track(&InputEvent::Key {
                                            code,
                                            action: KeyAction::Up,
                                        })
                                    }
                                    InputEvent::Button {
                                        button, position, ..
                                    } => guard.pressed.track(&InputEvent::Button {
                                        button,
                                        position,
                                        pressed: false,
                                    }),
                                    _ => {}
                                }
                            }
                            let _ = reply.send(result);
                        }
                        InjectWork::Locks(wanted, mask, reply) if !reply.is_closed() => {
                            let _ = reply.send(guard.inject.sync_locks(wanted, mask));
                        }
                        InjectWork::Hidden(hidden, reply) if !reply.is_closed() => {
                            // 即使调用失败也可能已经隐藏，退出时仍尝试显示。
                            guard.hidden |= hidden;
                            let result = guard.inject.set_cursor_hidden(hidden);
                            if result.is_ok() {
                                guard.hidden = hidden;
                            }
                            let _ = reply.send(result);
                        }
                        InjectWork::Reset(reply) => {
                            let _ = reply.send(guard.restore());
                        }
                        _ => {}
                    }
                }
            })?;
        Ok(Self {
            requests,
            failed: false,
        })
    }

    pub async fn inject(&mut self, event: &InputEvent) -> ferry_platform::Result<()> {
        self.request(|reply| InjectWork::Input(*event, reply)).await
    }

    pub async fn sync_locks(
        &mut self,
        wanted: LockKeys,
        mask: LockKeys,
    ) -> ferry_platform::Result<()> {
        self.request(|reply| InjectWork::Locks(wanted, mask, reply))
            .await
    }

    pub async fn set_cursor_hidden(&mut self, hidden: bool) -> ferry_platform::Result<()> {
        self.request(|reply| InjectWork::Hidden(hidden, reply))
            .await
    }

    /// 会话结束时清理worker真实持有的按键；核心状态可能已在一次失败的Up后清空。
    pub async fn reset(&mut self) -> ferry_platform::Result<()> {
        let result = self.request(InjectWork::Reset).await;
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    async fn request(
        &mut self,
        work: impl FnOnce(Reply<()>) -> InjectWork,
    ) -> ferry_platform::Result<()> {
        if self.failed {
            return Err(closed());
        }
        let (reply, done) = oneshot::channel();
        self.requests
            .send(work(reply))
            .await
            .map_err(|_| closed())?;
        confirmed(done, &mut self.failed).await
    }

    pub fn is_failed(&self) -> bool {
        self.failed || self.requests.is_closed()
    }
}

async fn confirmed<T>(
    done: oneshot::Receiver<ferry_platform::Result<T>>,
    failed: &mut bool,
) -> ferry_platform::Result<T> {
    match tokio::time::timeout(CONFIRM_TIMEOUT, done).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            *failed = true;
            Err(closed())
        }
        Err(_) => {
            *failed = true;
            Err(timed_out())
        }
    }
}

struct WorkerPermit {
    _scope: tokio::sync::OwnedSemaphorePermit,
    _budget: tokio::sync::OwnedSemaphorePermit,
}

async fn permit(scope: PathBuf) -> crate::Result<WorkerPermit> {
    // 同数据目录的新引擎（包括改换角色）要等旧调用与释放结束，避免清理影响新一代输入。
    // 只保存弱引用，空闲目录在下一次创建时回收，不保留用户目录历史。
    let scope = tokio::fs::canonicalize(scope).await?;
    type Leases = HashMap<PathBuf, Weak<Semaphore>>;
    static LEASES: OnceLock<Mutex<Leases>> = OnceLock::new();
    let scope = {
        let mut leases = LEASES
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        leases.retain(|_, semaphore| semaphore.strong_count() > 0);
        let entry = leases.entry(scope).or_default();
        let semaphore = entry
            .upgrade()
            .unwrap_or_else(|| Arc::new(Semaphore::new(1)));
        *entry = Arc::downgrade(&semaphore);
        semaphore
    };
    let scope = scope.acquire_owned().await.map_err(|_| closed())?;
    static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let budget = WORKERS
        .get_or_init(|| Arc::new(Semaphore::new(64)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| closed())?;
    Ok(WorkerPermit {
        _scope: scope,
        _budget: budget,
    })
}

fn closed() -> ferry_platform::Error {
    ferry_platform::Error::Failed("平台输入工作线程已停止".into())
}

fn timed_out() -> ferry_platform::Error {
    ferry_platform::Error::Failed("等待平台输入操作确认超时，已停止此输入引擎".into())
}

fn pack(point: Point) -> u64 {
    (u64::from(point.x as u32) << 32) | u64::from(point.y as u32)
}

fn unpack(value: u64) -> Point {
    Point::new((value >> 32) as i32, value as i32)
}

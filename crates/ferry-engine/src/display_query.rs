//! 串行显示器查询。原生显示驱动调用不占用输入和心跳的运行时线程。

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ferry_platform::Displays;
use ferry_types::Display;
use tokio::sync::{Semaphore, mpsc, oneshot};

use crate::error::Result;

pub(crate) struct DisplayResult {
    /// 工作线程内的查询耗时，不含请求等待或运行时接收结果的排队时间。
    pub elapsed: Duration,
    pub displays: ferry_platform::Result<Vec<Display>>,
}

/// 每个引擎持有一个查询器；请求未消费前不再排队，也不会并行调用平台对象。
///
/// 关闭时只丢弃通道。已经进入原生 API 的调用不能取消，但返回后结果不会送到新引擎，
/// 工作线程也会退出；Drop 不等待该线程，避免阻塞控制权归还。
pub(crate) struct DisplayQuery {
    requests: mpsc::Sender<oneshot::Sender<DisplayResult>>,
    pending: Option<oneshot::Receiver<DisplayResult>>,
}

impl DisplayQuery {
    pub async fn new(displays: impl Displays + 'static) -> Result<Self> {
        // 原生调用无法中止；连续重启也不能无限遗留线程。等待名额可随引擎启动取消。
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(32)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| closed())?;
        let (requests, mut incoming) = mpsc::channel::<oneshot::Sender<DisplayResult>>(1);
        std::thread::Builder::new()
            .name("ferry-displays".into())
            .spawn(move || {
                let _permit = permit;
                while let Some(reply) = incoming.blocking_recv() {
                    if reply.is_closed() {
                        continue;
                    }
                    let started = Instant::now();
                    let displays = displays.displays();
                    let _ = reply.send(DisplayResult {
                        elapsed: started.elapsed(),
                        displays,
                    });
                }
            })?;
        Ok(Self {
            requests,
            pending: None,
        })
    }

    /// 请求正在进行时不补排下一次；慢查询不会产生周期性任务积压。
    pub fn request(&mut self) -> Result<()> {
        if self.pending.is_none() {
            let (reply, result) = oneshot::channel();
            self.requests.try_send(reply).map_err(|_| closed())?;
            self.pending = Some(result);
        }
        Ok(())
    }

    /// 可放入 select；取消等待不会消费或丢弃在飞查询。
    pub async fn completed(&mut self) -> Result<DisplayResult> {
        let Some(pending) = self.pending.as_mut() else {
            return std::future::pending().await;
        };
        let result = pending.await.map_err(|_| closed());
        self.pending = None;
        result
    }

    /// 开始新会话时丢弃上个会话的查询，等它退出后再查询；不能让迟到的旧结果覆盖新布局。
    pub async fn fresh(&mut self) -> Result<DisplayResult> {
        if self.pending.is_some() {
            let _ = self.completed().await?;
        }
        self.request()?;
        self.completed().await
    }
}

fn closed() -> crate::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "显示器查询线程已停止").into()
}

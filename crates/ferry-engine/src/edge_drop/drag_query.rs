//! 拖放剪贴板和文件元数据的串行读取。结果只用于预览，不阻挡真正的放下事件。

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use ferry_platform::DragReader;
use tokio::sync::{Semaphore, mpsc, oneshot};

pub(super) enum Request {
    Baseline {
        generation: u64,
    },
    Files {
        generation: u64,
        baseline: i64,
        force: bool,
    },
    Directory {
        generation: u64,
        path: PathBuf,
    },
}

pub(super) enum Reply {
    Baseline {
        generation: u64,
        count: i64,
    },
    Files {
        generation: u64,
        content: Option<Content>,
    },
    Directory {
        generation: u64,
        directory: bool,
    },
}

pub(super) struct Content {
    pub files: Vec<PathBuf>,
    pub summary: Option<(String, u32, u64)>,
}

/// 每个浮层一个读取线程，只有一项请求在飞；关闭不等待不可取消的系统调用。
pub(super) struct DragQuery {
    requests: mpsc::Sender<(Request, oneshot::Sender<Reply>)>,
    pending: Option<oneshot::Receiver<Reply>>,
}

impl DragQuery {
    pub async fn new(mut reader: Box<dyn DragReader>) -> std::io::Result<Self> {
        static WORKERS: OnceLock<Arc<Semaphore>> = OnceLock::new();
        let permit = WORKERS
            .get_or_init(|| Arc::new(Semaphore::new(32)))
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| closed())?;
        let (requests, mut incoming) = mpsc::channel::<(Request, oneshot::Sender<Reply>)>(1);
        std::thread::Builder::new()
            .name("ferry-drag-read".into())
            .spawn(move || {
                let _permit = permit;
                while let Some((request, reply)) = incoming.blocking_recv() {
                    if reply.is_closed() {
                        continue;
                    }
                    let value = match request {
                        Request::Baseline { generation } => Reply::Baseline {
                            generation,
                            count: reader.change_count(),
                        },
                        Request::Files {
                            generation,
                            baseline,
                            force,
                        } => {
                            let count = reader.change_count();
                            let content = if force || count != baseline {
                                let files = reader.files();
                                // 原生读取期间变化了就下次重试，不能把两次拖动的内容拼在一起。
                                (reader.change_count() == count).then(|| Content {
                                    summary: summarize(&files),
                                    files,
                                })
                            } else {
                                None
                            };
                            Reply::Files {
                                generation,
                                content,
                            }
                        }
                        Request::Directory { generation, path } => Reply::Directory {
                            generation,
                            directory: is_directory(&path),
                        },
                    };
                    let _ = reply.send(value);
                }
            })?;
        Ok(Self {
            requests,
            pending: None,
        })
    }

    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    pub fn request(&mut self, request: Request) -> std::io::Result<()> {
        if self.busy() {
            return Ok(());
        }
        let (reply, pending) = oneshot::channel();
        self.requests
            .try_send((request, reply))
            .map_err(|_| closed())?;
        self.pending = Some(pending);
        Ok(())
    }

    /// select 取消安全；旧代结果仍需消费后丢弃，不能提前增加一个在飞请求。
    pub async fn completed(&mut self) -> std::io::Result<Reply> {
        let Some(pending) = self.pending.as_mut() else {
            return std::future::pending().await;
        };
        let result = pending.await.map_err(|_| closed());
        self.pending = None;
        result
    }
}

fn summarize(files: &[PathBuf]) -> Option<(String, u32, u64)> {
    let name = files.first()?.file_name()?.to_string_lossy().into_owned();
    // 接收目标的出现不能等待网络盘/云文件的metadata。真实大小由发送扫描提供。
    Some((name, u32::try_from(files.len()).unwrap_or(u32::MAX), 0))
}

fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

fn closed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "拖放读取线程已停止")
}

//! 文件准备、预览、诊断与日志任务的监督和回收。

use super::*;

impl<H: Host> Runtime<H> {
    /// 预览写盘不占用输入运行时；新内容到达后旧任务结果不再应用。
    pub(super) fn save_preview(&mut self, png: Arc<[u8]>) -> Option<String> {
        self.preview_pending = Some((self.preview_generation, png));
        self.start_preview();
        None
    }

    pub(super) fn start_preview(&mut self) {
        if self.preview_busy || self.workers.len() >= 8 {
            return;
        }
        let Some((generation, png)) = self.preview_pending.take() else {
            return;
        };
        self.preview_busy = true;
        let store = self.store.clone();
        let previous = self.clip_preview.clone();
        self.workers.spawn_blocking(move || {
            let result = store.put_blob(&png).map_err(|e| e.to_string());
            if let (Ok(path), Some(old)) = (&result, previous)
                && path != &old
            {
                let _ = store.remove_blob(&old);
            }
            WorkResult::Preview { generation, result }
        });
    }

    pub(crate) async fn shutdown_workers(&mut self) {
        self.preview_pending = None;
        self.workers.abort_all();
        // 已开始的阻塞IO不能abort，等它们退出再移交实例锁/启动更新版本。
        while self.workers.join_next().await.is_some() {}
    }

    pub(super) fn send_files(
        &mut self,
        from: u64,
        command_id: Option<u64>,
        paths: Vec<String>,
        to: Option<String>,
    ) -> bool {
        let target = match to {
            Some(hex) => hex.parse::<Fingerprint>().ok(),
            None => self.focus_link().map(|l| l.fingerprint),
        };
        let Some(peer) = target else {
            self.toast(
                Phrase::new("not_connected"),
                Error::NotConnected.to_string(),
            );
            return false;
        };
        if !self.config.device(&peer.to_hex()).files {
            self.toast(Phrase::new("files_disabled"), "已关闭这台设备的文件传输");
            return false;
        }
        if self.workers.len() >= 8 {
            self.toast(Phrase::new("error"), "后台任务较多，请稍后再试");
            return false;
        }
        let files = self.files.clone();
        let name = self.peer_name(&peer);
        let mut generation = self.generation_signal.subscribe();
        self.workers.spawn(async move {
            let result = tokio::select! {
                biased;
                _ = generation.changed() => Err("共享状态已改变，文件准备已取消".to_owned()),
                result = files.send(peer, paths.into_iter().map(PathBuf::from).collect()) => {
                    result.map(|_| format!("开始发给 {name}")).map_err(|e| e.to_string())
                }
            };
            WorkResult::Sent {
                from,
                command_id,
                result,
            }
        });
        true
    }

    pub(crate) async fn next_work(&mut self) -> Result<WorkResult, tokio::task::JoinError> {
        let engine = async {
            match self.engine.as_mut() {
                Some(task) => (&mut task.handle).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            result = engine => {
                let result = result.unwrap_or_else(|e| Err(std::io::Error::other(format!("引擎意外退出：{e}")).into()));
                Ok(WorkResult::EngineExit(result))
            }
            result = self.workers.join_next(), if !self.workers.is_empty() => result.expect("已有任务"),
        }
    }

    pub(crate) fn on_work(&mut self, result: Result<WorkResult, tokio::task::JoinError>) {
        let work = match result {
            Ok(work) => work,
            Err(e) => {
                warn!("后台任务结束：{e}");
                return;
            }
        };
        match work {
            WorkResult::PlatformInfo {
                addresses,
                natural_scrolling,
            } => {
                self.platform_info_pending = false;
                self.platform_info_at = Instant::now();
                if addresses != self.addresses || natural_scrolling != self.natural_scrolling {
                    self.addresses = addresses;
                    self.natural_scrolling = natural_scrolling;
                    self.dirty = true;
                }
            }
            WorkResult::EngineExit(result) => {
                self.engine = None;
                self.on_engine_exit(result);
            }
            WorkResult::Sent {
                from,
                command_id,
                result,
            } => {
                let ok = result.is_ok();
                let text = result.as_ref().map_or_else(Clone::clone, Clone::clone);
                self.reply(from, Notice::Toast { text, phrase: None });
                if let Some(id) = command_id {
                    self.command_result(from, id, result.map(|_| ()));
                }
                if !ok {
                    self.dirty = true;
                }
            }
            WorkResult::RunLog {
                from,
                chunk,
                request_id,
            } => {
                self.logs_in_flight.remove(&from);
                self.reply(
                    from,
                    Notice::RunLog {
                        lines: chunk.lines,
                        next: chunk.next,
                        fresh: chunk.fresh,
                        bytes: chunk.bytes,
                        lines_total: chunk.lines_total,
                        cursor: chunk.cursor,
                        more: chunk.more,
                        error: chunk.error,
                        request_id,
                    },
                );
            }
            WorkResult::Diagnostic {
                from,
                command_id,
                result,
            } => {
                let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
                match result {
                    Ok((Some(path), text)) => {
                        self.last_export = Some(path.clone());
                        self.reply(
                            from,
                            Notice::DiagnosticsExported {
                                path: path.display().to_string(),
                                bytes: text.len() as u64,
                            },
                        );
                    }
                    Ok((None, text)) => self.reply(from, Notice::DiagnosticsSummary { text }),
                    Err(message) => self.reply(
                        from,
                        Notice::Toast {
                            text: message,
                            phrase: None,
                        },
                    ),
                }
                if let Some(id) = command_id {
                    self.command_result(from, id, status);
                }
            }
            WorkResult::Preview { generation, result } => {
                self.preview_busy = false;
                if let Ok(path) = result {
                    self.clip_preview = Some(path.clone());
                    if generation == self.preview_generation
                        && let Some(image) = self.clipboard.as_mut().and_then(|c| c.image.as_mut())
                    {
                        image.path = Some(path.display().to_string());
                        self.dirty = true;
                    }
                }
                self.start_preview();
            }
        }
    }
}

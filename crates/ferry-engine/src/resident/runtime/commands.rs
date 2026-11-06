//! 界面命令解包、会话与请求关联。

use super::*;

impl<H: Host> Runtime<H> {
    pub(super) async fn on_request(&mut self, from: u64, request: Request) {
        let (request, command_id) = match request {
            Request::Execute {
                session,
                id,
                request,
            } => {
                if session != self.session
                    || matches!(
                        *request,
                        Request::Execute { .. }
                            | Request::Hello { .. }
                            | Request::UpdateHealth { .. }
                    )
                {
                    self.command_result(from, id, Err("请求已经过期，请重新操作".into()));
                    return;
                }
                (*request, Some(id))
            }
            Request::PairReply { .. } => return,
            request => (request, None),
        };
        self.command_error = None;
        let operation = command_id
            .filter(|_| {
                matches!(
                    &request,
                    Request::ConnectAddress { .. } | Request::Invite { .. }
                )
            })
            .map(|id| {
                let operation = self.next_operation;
                self.next_operation = self.next_operation.wrapping_add(1);
                self.dial_requests
                    .insert(operation, (from, id, Instant::now()));
                operation
            });
        match request {
            Request::Hello { .. } | Request::UpdateHealth { .. } | Request::Execute { .. } => {}
            Request::SetSharing { on } => self.set_sharing(on).await,
            Request::UpdateSettings { settings } => self.update_settings(settings).await,
            Request::SetDevice { device } => self.set_device(device).await,
            Request::OpenPeerSettings { fingerprint } => self.open_peer_settings(&fingerprint),
            Request::Forget { fingerprint } => self.forget(&fingerprint).await,
            Request::PairReply { id, accept } => {
                if self.pending.get(&id).is_some_and(|p| !p.reply.is_closed()) {
                    self.answer(id, accept);
                } else {
                    self.command_error = Some("配对请求已经结束".into());
                }
            }
            Request::Connect { fingerprint } => self.connect_to(&fingerprint),
            Request::ConnectAddress { address } => self.connect_address(address, operation),
            Request::Invite { address } => self.invite_address(address, operation),
            Request::OpenPermissionSettings => self.host.open_permission_settings(),
            Request::ClearLog => {
                self.log.clear();
                self.broadcast(Notice::History {
                    entries: Vec::new(),
                });
            }
            Request::OpenSettings => self.open_settings(),
            Request::SendFiles { paths, to } => {
                if self.send_files(from, command_id, paths, to) {
                    return;
                }
            }
            Request::Quit => {
                info!("收到退出请求");
                self.quit_requested = true;
            }
            Request::ReadRunLog {
                since,
                cursor,
                request_id,
            } => {
                if self.workers.len() < 32 && self.logs_in_flight.insert(from) {
                    let path = self.store.log_file();
                    let files = self.host.file_committer();
                    self.workers.spawn_blocking(move || WorkResult::RunLog {
                        from,
                        request_id,
                        chunk: diagnostics::read(&path, since, cursor.as_deref(), files.as_ref()),
                    });
                } else {
                    self.reply(
                        from,
                        Notice::RunLog {
                            lines: Vec::new(),
                            next: since.unwrap_or(0),
                            fresh: false,
                            bytes: 0,
                            lines_total: 0,
                            cursor: cursor.unwrap_or_default(),
                            more: false,
                            error: Some("运行日志读取繁忙，请稍后重试".into()),
                            request_id,
                        },
                    );
                }
            }
            Request::ExportDiagnostics => {
                if self.start_diagnostics(from, command_id, false) {
                    return;
                }
            }
            Request::CopyDiagnostics => {
                if self.start_diagnostics(from, command_id, true) {
                    return;
                }
            }
            Request::RevealDiagnostics => match &self.last_export {
                Some(path) if path.exists() => self.host.reveal(path),
                _ => self.reply(
                    from,
                    Notice::Toast {
                        text: "诊断文件已经不在了，请重新导出".into(),
                        phrase: Some(Phrase::new("diagnostics_missing")),
                    },
                ),
            },
            Request::OpenLogsFolder => {
                let dir = self.store.logs_dir();
                let _ = fs::create_dir_all(&dir);
                self.host.open_folder(&dir);
            }
            Request::SetVerbose { on } => self.set_verbose(on),
            Request::CheckUpdate => self.updates.check(true),
            Request::DownloadUpdate => self.updates.download(),
            Request::CancelUpdate => self.updates.cancel(),
            Request::InstallUpdate => self.updates.install(!self.clients.is_empty()),
            Request::DismissUpdate { version } => self.updates.dismiss(version),
        }
        self.dirty = true;
        if let Some(id) = command_id {
            let result = self.command_error.take().map_or(Ok(()), Err);
            self.command_result(from, id, result);
        }
    }

    pub(super) fn command_result(&mut self, from: u64, id: u64, result: Result<(), String>) {
        // 同一出站队列中先同步真实状态，再撤销 UI 的乐观显示。
        self.reply(
            from,
            Notice::State {
                state: Box::new(self.snapshot()),
            },
        );
        self.reply(
            from,
            Notice::CommandResult {
                session: self.session.clone(),
                id,
                ok: result.is_ok(),
                message: result.err().unwrap_or_default(),
            },
        );
    }
}

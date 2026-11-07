//! 日志设置与诊断文件生成。

use super::*;

impl<H: Host> Runtime<H> {
    // ── 运行日志与诊断文件 ───────────────────────

    pub(super) fn set_verbose(&mut self, on: bool) {
        if on == self.verbose_since.is_some() {
            return;
        }
        self.host.set_verbose(on);
        self.verbose_since = on.then(now_ms);
        info!("详细记录已{}", if on { "打开" } else { "关闭" });
        self.dirty = true;
    }

    pub(super) fn start_diagnostics(
        &mut self,
        from: u64,
        command_id: Option<u64>,
        summary: bool,
    ) -> bool {
        if self.workers.len() >= 8 {
            self.toast(Phrase::new("error"), "后台任务较多，请稍后再试");
            return false;
        }
        let (state, host, config, store, history) = (
            self.snapshot(),
            self.host.clone(),
            self.config.clone(),
            self.store.clone(),
            self.performance_history.clone(),
        );
        self.workers.spawn_blocking(move || {
            let text = Self::diagnostics(state, &*host, &config, &store, &history, summary);
            let result = if summary {
                Ok((None, text))
            } else {
                let (_, stamp) = diagnostics::local_time(now_ms(), host.utc_offset());
                let dir = store.downloads_dir();
                let saved = (|| -> std::io::Result<PathBuf> {
                    use std::io::Write;
                    fs::create_dir_all(&dir)?;
                    let name = format!("ScreenFerry-诊断-{stamp}.txt");
                    for n in 1..=u32::MAX {
                        let name = if n == 1 {
                            name.clone()
                        } else {
                            ferry_core::file_name::numbered(&name, n)
                        };
                        let path = dir.join(name);
                        match fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path)
                        {
                            Ok(mut file) => {
                                file.write_all(text.as_bytes())?;
                                return Ok(path);
                            }
                            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
                            Err(e) => return Err(e),
                        }
                    }
                    Err(std::io::Error::other("无法找到可用文件名"))
                })();
                saved
                    .map(|path| (Some(path), text))
                    .map_err(|e| format!("诊断导出失败：{e}"))
            };
            WorkResult::Diagnostic {
                from,
                command_id,
                result,
            }
        });
        true
    }

    /// 诊断文件（`summary` 为真时是只带最后 200 行运行日志的摘要）。设备名、文件名、用户目录换成代号。
    pub(super) fn diagnostics(
        state: State,
        host: &H,
        config: &Config,
        store: &Store,
        history: &crate::MetricsHistory,
        summary: bool,
    ) -> String {
        let home = ferry_store::home_dir();
        let mut redactor = Redactor::new(home.as_deref().and_then(Path::to_str));
        redactor.local(&state.local.name);
        let server = state.local.role == Role::Server;
        let on = |b: bool| if b { "开" } else { "关" };

        let (now, _) = diagnostics::local_time(now_ms(), host.utc_offset());
        let version = match option_env!("SCREENFERRY_COMMIT") {
            Some(commit) if !commit.is_empty() => {
                format!("{}（提交 {commit}）", state.local.version)
            }
            _ => state.local.version.clone(),
        };
        let mut local = format!(
            "本机 · {}",
            if server {
                "控制别的电脑"
            } else {
                "被控制"
            }
        );
        if let Some(port) = state.local.port {
            local.push_str(&format!(" · 监听 {port}"));
        }
        if server {
            local.push_str(&format!(" · 局域网广播 {}", on(state.settings.announce)));
        } else {
            match state.settings.server.trim() {
                "" => local.push_str(" · 自动寻找控制端"),
                fixed => local.push_str(&format!(" · 固定连接 {fixed}")),
            }
            if let Some(port) = state.local.invite_port {
                local.push_str(&format!(" · 等邀请 {port}"));
            }
        }
        let displays = match ferry_platform::Displays::displays(&host.displays()) {
            Ok(list) => {
                let each: Vec<String> = list
                    .iter()
                    .map(|d| {
                        format!(
                            "{}×{} @{}%{}",
                            d.bounds.width,
                            d.bounds.height,
                            d.scale_percent,
                            if d.primary { "（主）" } else { "" }
                        )
                    })
                    .collect();
                format!("{} 台：{}", list.len(), each.join(" · "))
            }
            Err(e) => format!("读不到：{e}"),
        };
        let permission = match host.check_permissions(state.local.role) {
            Ok(()) => "键鼠控制 已允许".to_owned(),
            Err(e) => format!("键鼠控制 未允许：{e}"),
        };
        let mut devices = format!("{} 台已配对", state.peers.len());
        for peer in &state.peers {
            let code = redactor.device(&[&peer.name, &peer.alias]);
            let platform = match peer.platform.as_str() {
                "macos" => "macOS",
                "windows" => "Windows",
                "" => "系统未知",
                other => other,
            };
            let mut row = format!(
                "{code}  {platform} · {}",
                if peer.online { "在线" } else { "离线" }
            );
            if !peer.addr.is_empty() {
                row.push_str(&format!(" · {}", peer.addr));
            }
            if let Some(transport) = &peer.input_transport {
                row.push_str(&format!(" · 输入 {transport}"));
            }
            if let Some(reason) = &peer.fallback_reason {
                row.push_str(&format!(" · 回退 {reason}"));
            }
            if let Some(side) = peer.side {
                row.push_str(&format!(" · 在本机{}", side_label(side)));
            }
            let preset = match config.device(&peer.fingerprint).modifiers {
                ferry_types::ModifierPreset::Habit => "修饰键按本机习惯",
                ferry_types::ModifierPreset::Plain => "修饰键原样",
                ferry_types::ModifierPreset::Custom => "修饰键自定义",
            };
            row.push_str(&format!(" · {preset}"));
            devices.push('\n');
            devices.push_str(&row);
        }
        let settings = format!(
            "共享 {} · 剪贴板 {}（上限 {} MB）· 开机自启 {}",
            on(state.sharing),
            on(state.settings.clipboard),
            state.settings.clipboard_limit_mb,
            on(state.settings.autostart)
        );
        let settings = format!(
            "{settings} · 文件上限 {} MB/s（0 不限）· 合并移动 {} · 输入偏好 {} · QUIC可用 {} · 剪贴板/文件 TCP+TLS1.3",
            state.settings.file_rate_limit_mbps,
            on(state.settings.coalesce_motion),
            state.settings.input_transport.as_str(),
            on(state.local.quic_available)
        );
        let verbose = match state.local.verbose_since {
            Some(since) => format!("开着，已有 {} 分钟", (now_ms() - since) / 60_000),
            None => "关".to_owned(),
        };
        let mut header = vec![
            ("生成于", now),
            ("版本", version),
            (
                "系统",
                if state.local.system.is_empty() {
                    std::env::consts::OS.to_owned()
                } else {
                    state.local.system.clone()
                },
            ),
            ("本机", local),
            ("显示器", displays),
            ("权限", permission),
            ("设备", devices),
            ("设置", settings),
            ("详细记录", verbose),
            (
                "阶段统计",
                format!(
                    "本机单调时钟；近似分位数；RTT为应用心跳往返，含两端处理和排队，不是端到端输入延迟\n{}",
                    serde_json::to_string(&state.performance).unwrap_or_default()
                ),
            ),
            (
                "尖峰上下文",
                if summary {
                    format!(
                        "已保留 {} 次 >100ms 事件上下文；每次最多5个窗口，完整数据请导出诊断文件。慢后台查询不代表输入被阻塞。",
                        history.incidents().count()
                    )
                } else {
                    format!(
                        "按来源收集前2、触发1、后2个窗口；重叠触发合并；慢后台查询不代表输入被阻塞。连接或引擎换代后清空对应来源。\n{}",
                        super::super::performance::history(history)
                    )
                },
            ),
        ];
        // 局域网里发现、还没配对的设备也可能出现在日志里。
        for found in &state.found {
            redactor.device(&[&found.name]);
        }
        if let Some(problem) = &state.problem {
            header.push(("问题", redactor.line(&problem.message)));
        }
        let title = if summary {
            "ScreenFerry 诊断摘要"
        } else {
            "ScreenFerry 诊断文件"
        };
        let previous = store.previous_log_file();
        let current = store.log_file();
        diagnostics::report(
            title,
            &header,
            &[previous.as_path(), current.as_path()],
            &mut redactor,
            summary,
        )
    }
}

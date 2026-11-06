//! 设置保存、在线更新、连接邀请与共享状态切换。

use super::*;

impl<H: Host> Runtime<H> {
    pub(super) async fn set_sharing(&mut self, on: bool) {
        if on != self.paused {
            return;
        }
        self.paused = !on;
        self.problem = None;
        self.dirty = true;
        if on {
            self.start_engine();
            let phrase = Phrase::new("sharing_resumed");
            self.log(
                LogKind::Link,
                LogIcon::Play,
                phrase.clone(),
                "继续共享",
                "键鼠可以切到对方",
            );
            self.toast(phrase, "已继续共享");
        } else {
            self.stop_engine().await;
            self.status = LinkStatus::Paused;
            let phrase = Phrase::new("sharing_paused");
            self.log(
                LogKind::Link,
                LogIcon::Pause,
                phrase.clone(),
                "暂停共享",
                "键鼠留在本机",
            );
            self.toast(phrase, "已暂停共享");
        }
    }

    pub(super) async fn update_settings(&mut self, patch: SettingsPatch) {
        if let Some(transport) = patch.input_transport
            && let Err(error) = crate::Transport::from(transport).ensure_available()
        {
            self.command_error = Some(error.to_string());
            self.toast(
                Phrase::new("error").arg("message", &error),
                error.to_string(),
            );
            return;
        }
        let mut next = self.config.clone();
        // 需要重新握手才能生效的设置。
        let mut restart = false;
        if let Some(transport) = patch.input_transport {
            restart |= transport != next.input_transport;
            next.input_transport = transport;
        }
        if let Some(name) = patch.name {
            restart |= name != next.name;
            next.name = name;
        }
        let role_changed = patch.role.filter(|&role| role != next.role);
        if let Some(role) = role_changed {
            restart = true;
            next.role = role;
        }
        if let Some(announce) = patch.announce {
            restart |= announce != next.announce;
            next.announce = announce;
        }
        if let Some(clipboard) = patch.clipboard {
            restart |= clipboard != next.clipboard;
            next.clipboard = clipboard;
        }
        if let Some(mb) = patch.clipboard_limit_mb {
            let range = Config::CLIPBOARD_LIMIT_MB;
            next.clipboard_limit_mb = mb.clamp(*range.start(), *range.end());
        }
        if let Some(rate) = patch.file_rate_limit_mbps {
            next.file_rate_limit_mbps = rate.min(Config::MAX_FILE_RATE_MBPS);
        }
        if let Some(on) = patch.coalesce_motion {
            next.coalesce_motion = on;
        }
        if let Some(server) = patch.server {
            restart |= server != next.server;
            next.server = server;
        }
        let moved = patch.side.filter(|&side| side != next.side);
        if let Some(side) = patch.side {
            next.side = side;
        }
        let arranged = patch.layout.as_ref().is_some_and(|l| *l != next.layout);
        if let Some(layout) = patch.layout {
            next.layout = layout;
        }
        if let Some(dpi) = patch.mouse_dpi {
            let range = Config::MOUSE_DPI;
            next.mouse_dpi = dpi.clamp(*range.start(), *range.end());
        }
        if let Some(theme) = patch.theme {
            next.theme = theme;
        }
        if let Some(on) = patch.auto_update {
            next.auto_update = on;
        }
        if let Some(language) = patch.language {
            next.language = language;
        }
        if let Some(accent) = patch.accent {
            next.accent = if accent < Config::ACCENT_COUNT {
                accent
            } else {
                0
            };
        }
        if let Some(on) = patch.autostart {
            match self.host.set_autostart(on) {
                Ok(()) => self.autostart = self.host.autostart(),
                Err(e) => self.toast(
                    Phrase::new("autostart_failed").arg("message", &e),
                    format!("无法修改开机自启：{e}"),
                ),
            }
            self.dirty = true;
        }

        if next != self.config {
            if let Err(e) = self.store.save_config(&next) {
                self.toast(
                    Phrase::new("save_failed").arg("message", &e),
                    format!("保存设置失败：{e}"),
                );
                return;
            }
            self.config = next;
            self.lang = self.config.language.resolve(self.host.system_language());
            self.edge.set_lang(self.lang);
            self.edge.set_accent(accent_color(self.config.accent()));
            self.clip_limit.set(self.config.clipboard_limit_bytes());
            self.files
                .set_rate_limit(self.config.file_rate_limit_bytes());
            self.dirty = true;
        }
        if arranged {
            self.log(
                LogKind::Link,
                LogIcon::Layout,
                Phrase::new("layout_arranged"),
                "调整了设备的位置",
                "布局已保存",
            );
        } else if let Some(side) = moved {
            let (name, peer) = match self.links.as_slice() {
                [only] => (
                    only.peer.name.clone(),
                    mark::device(&only.peer.name).to_string(),
                ),
                _ => (String::new(), "其他设备".to_owned()),
            };
            let phrase = Phrase::new("layout_moved")
                .arg("name", &name)
                .arg("side", side_key(side));
            self.log(
                LogKind::Link,
                LogIcon::Layout,
                phrase,
                format!("{peer} 移到本机{}", side_label(side)),
                "布局已保存",
            );
        }
        if let Some(role) = role_changed {
            // 另一种角色用不上之前发现的服务端。
            self.found.clear();
            let (key, text, detail) = match role {
                Role::Server => (
                    "server",
                    "改为控制别的电脑",
                    "键盘鼠标在本机，等另一台电脑来连接",
                ),
                Role::Client => (
                    "client",
                    "改为由另一台电脑控制",
                    "在局域网里寻找键盘鼠标所在的那台",
                ),
            };
            let phrase = Phrase::new("role_changed").arg("role", key);
            self.log(LogKind::Link, LogIcon::Settings, phrase, text, detail);
        }
        if restart {
            self.name = resolve_name(&self.config, &*self.host);
            self.restart_engine().await;
        } else {
            self.push_live();
        }
    }

    /// 换上一台设备的单独设置。服务端上立即生效；剪贴板开关要重新连接才生效，
    /// 客户端上对服务端的剪贴板、拖文件开关也一样。
    pub(super) async fn set_device(&mut self, device: DeviceSettings) {
        let Ok(fingerprint) = device.fingerprint.parse::<Fingerprint>() else {
            return;
        };
        // 统一写成小写十六进制，免得同一台设备有两条。
        let device = DeviceSettings {
            fingerprint: fingerprint.to_hex(),
            ..device.clamped()
        };
        let before = self.config.device(&device.fingerprint);
        if before == device {
            return;
        }
        let mut next = self.config.clone();
        next.set_device(device.clone());
        if let Err(e) = self.store.save_config(&next) {
            return self.toast(
                Phrase::new("save_failed").arg("message", &e),
                format!("保存设置失败：{e}"),
            );
        }
        self.config = next;
        self.dirty = true;
        let linked = self.links.iter().any(|l| l.fingerprint == fingerprint);
        let reconnect = linked
            && (before.clipboard != device.clipboard
                || (self.config.role == Role::Client && before.files != device.files));
        if reconnect {
            self.restart_engine().await;
        } else {
            self.push_live();
        }
    }

    /// 客户端：请连着的服务端打开它的设置界面。
    pub(super) fn open_peer_settings(&mut self, fingerprint: &str) {
        let Ok(fingerprint) = fingerprint.parse::<Fingerprint>() else {
            return;
        };
        let online = self.links.iter().any(|l| l.fingerprint == fingerprint);
        let commands = self.engine.as_ref().and_then(|t| t.client.as_ref());
        match commands {
            Some(commands) if online => {
                let _ = commands.send(ClientCommand::OpenPeerSettings);
            }
            _ => {
                let name = self.peer_name(&fingerprint);
                self.toast(
                    Phrase::new("peer_offline").arg("name", &name),
                    format!("{name} 现在没连着"),
                );
            }
        }
    }

    /// 把服务端的即时设置（布局、每台设备的设置）交给正在运行的引擎。
    pub(super) fn push_live(&self) {
        if let Some(live) = self.engine.as_ref().and_then(|t| t.live.as_ref()) {
            live.send_if_modified(|current| {
                let next = self.live();
                let changed = *current != next;
                *current = next;
                changed
            });
        }
    }

    pub(super) async fn forget(&mut self, fingerprint: &str) {
        let Ok(fingerprint) = fingerprint.parse::<Fingerprint>() else {
            return;
        };
        let mut peers = match self.store.peers() {
            Ok(peers) => peers,
            Err(e) => {
                return self.toast(
                    Phrase::new("peers_read_failed").arg("message", &e),
                    format!("读取已配对设备失败：{e}"),
                );
            }
        };
        let Some(name) = peers.get(&fingerprint).map(|p| p.name.clone()) else {
            return;
        };
        peers.remove(&fingerprint);
        if let Err(e) = self.store.save_peers(&peers) {
            return self.toast(
                Phrase::new("remove_failed").arg("message", &e),
                format!("移除失败：{e}"),
            );
        }
        self.refresh_peers();
        // 它的位置安排和单独设置也不再需要。
        let hex = fingerprint.to_hex();
        if self.config.layout.iter().any(|p| p.fingerprint == hex)
            || self.config.devices.iter().any(|d| d.fingerprint == hex)
        {
            let mut next = self.config.clone();
            next.layout.retain(|p| p.fingerprint != hex);
            next.devices.retain(|d| d.fingerprint != hex);
            match self.store.save_config(&next) {
                Ok(()) => self.config = next,
                Err(e) => warn!("保存设置失败：{e}"),
            }
        }
        let phrase = Phrase::new("removed").arg("name", &name);
        self.log(
            LogKind::Link,
            LogIcon::Unlink,
            phrase.clone(),
            format!("已移除 {}", mark::device(&name)),
            "下次连接需要重新配对",
        );
        self.toast(phrase, format!("已移除 {name}"));
        // 正连着这台：断开它，它下次来要重新配对。
        if self.links.iter().any(|l| l.fingerprint == fingerprint) {
            self.restart_engine().await;
        }
    }

    /// 客户端：连接局域网里发现的一台服务端。只连这一次，不把地址写死。
    pub(super) fn connect_to(&mut self, fingerprint: &str) {
        if self.config.role != Role::Client {
            return;
        }
        let Ok(fingerprint) = fingerprint.parse::<Fingerprint>() else {
            return;
        };
        let Some(addr) = self
            .found
            .iter()
            .find(|f| f.fingerprint == fingerprint)
            .and_then(|f| f.addrs.first().copied())
        else {
            return self.toast(Phrase::new("server_gone"), "这台电脑已经不在局域网里了");
        };
        self.dial(addr);
    }

    /// 客户端：按地址连接一台服务端。电脑名在后台解析，结果以 `dialed` 告诉界面。
    pub(super) fn connect_address(&mut self, address: String, request_id: Option<u64>) {
        if self.config.role != Role::Client {
            self.command_error = Some("当前角色不能发起连接".into());
            return self.dial_result(request_id, address, false, Some(IpcDialError::WrongRole));
        }
        let Some(commands) = self.engine.as_ref().and_then(|t| t.client.clone()) else {
            self.toast(
                Phrase::new("paused_connect_later"),
                "已暂停共享，继续共享后再连接",
            );
            return self.dial_result(request_id, address, false, Some(IpcDialError::Unreachable));
        };
        if self.background.len() >= 64 {
            return self.dial_result(request_id, address, false, Some(IpcDialError::Busy));
        }
        let events = self.events.scoped(self.generation);
        self.background.spawn(async move {
            match resolve_input(&address, DEFAULT_PORT).await {
                Some(addr) => {
                    let command = match request_id {
                        Some(request_id) => ClientCommand::RequestedConnect { addr, request_id },
                        None => ClientCommand::Connect(addr),
                    };
                    let _ = commands.send(command);
                }
                None => events.emit(Event::Dialed {
                    request_id,
                    address,
                    invite: false,
                    error: Some(DialError::BadAddress),
                }),
            }
        });
    }

    pub(super) fn dial(&mut self, addr: SocketAddr) {
        match self.engine.as_ref().and_then(|t| t.client.as_ref()) {
            Some(commands) => {
                let _ = commands.send(ClientCommand::Connect(addr));
            }
            None => self.toast(
                Phrase::new("paused_connect_later"),
                "已暂停共享，继续共享后再连接",
            ),
        }
    }

    /// 服务端：邀请这个地址上的电脑连过来。结果以 `dialed` 告诉界面。
    pub(super) fn invite_address(&mut self, address: String, request_id: Option<u64>) {
        if self.config.role != Role::Server {
            self.command_error = Some("当前角色不能邀请设备".into());
            return self.dial_result(request_id, address, true, Some(IpcDialError::WrongRole));
        }
        let Some(port) = self.port else {
            self.toast(
                Phrase::new("not_sharing_invite_later"),
                "还没开始共享，开始后再邀请",
            );
            return self.dial_result(request_id, address, true, Some(IpcDialError::Unreachable));
        };
        if self.background.len() >= 64 {
            return self.dial_result(request_id, address, true, Some(IpcDialError::Busy));
        }
        let (store, name, events) = (
            self.store.clone(),
            self.name.clone(),
            self.events.scoped(self.generation),
        );
        self.background.spawn(async move {
            let error = match resolve_input(&address, INVITE_PORT).await {
                None => Some(DialError::BadAddress),
                Some(addr) => invite(&store, &name, addr, port)
                    .await
                    .err()
                    .map(|e| DialError::of(&e)),
            };
            events.emit(Event::Dialed {
                request_id,
                address,
                invite: true,
                error,
            });
        });
    }

    pub(super) fn dial_result(
        &mut self,
        operation: Option<u64>,
        address: String,
        invite: bool,
        error: Option<IpcDialError>,
    ) {
        if let Some(operation) = operation {
            if let Some((from, request_id, _)) = self.dial_requests.remove(&operation) {
                self.reply(
                    from,
                    Notice::Dialed {
                        request_id: Some(request_id),
                        address,
                        invite,
                        error,
                    },
                );
            }
        } else {
            self.broadcast(Notice::Dialed {
                request_id: None,
                address,
                invite,
                error,
            });
        }
    }

    /// 服务端开始共享时，邀请上次连过、现在没连着的客户端：局域网发现不可靠时也能自己连上来。
    /// 不成功也不提示。
    pub(super) fn invite_known(&mut self, port: u16) {
        for peer in self.peers.iter() {
            let Some(addr) = peer.addr else {
                continue;
            };
            if self.links.iter().any(|l| l.fingerprint == peer.fingerprint) {
                continue;
            }
            if self.background.len() >= 64 {
                break;
            }
            let to = SocketAddr::new(addr.ip(), INVITE_PORT);
            let (store, name) = (self.store.clone(), self.name.clone());
            self.background.spawn(async move {
                if invite(&store, &name, to, port).await.is_ok() {
                    info!(%to, "已邀请上次连过的电脑");
                }
            });
        }
    }
}

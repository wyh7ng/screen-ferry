//! 托盘摘要和设置界面的状态投影。

use super::*;

impl<H: Host> Runtime<H> {
    pub(crate) fn summary(&self) -> Summary {
        let lang = self.lang;
        Summary {
            sharing: !self.paused,
            connected_to: (!self.links.is_empty()).then(|| {
                self.links
                    .iter()
                    .map(|l| self.peer_name(&l.fingerprint))
                    .collect::<Vec<_>>()
                    .join(lang.pick("、", ", "))
            }),
            problem: self.problem.as_ref().map(|p| match (p.kind, lang) {
                (ProblemKind::Permission, Lang::En) => {
                    "Allow ScreenFerry to control the keyboard and mouse in System Settings"
                        .to_owned()
                }
                (ProblemKind::PortInUse, Lang::En) => format!(
                    "Port {} is in use: another copy of ScreenFerry may be running",
                    p.port.unwrap_or_default()
                ),
                _ => p.message.clone(),
            }),
            lang,
            update: self.updates.ready_version(),
        }
    }

    pub(crate) fn snapshot(&self) -> State {
        let focus = self.focus_link();
        let server = self.config.role == Role::Server;
        State {
            session: self.session.clone(),
            performance: ferry_ipc::Performance {
                peers: self.performance.values().cloned().collect(),
            },
            local: Local {
                quic_available: crate::Transport::quic_available(),
                name: self.name.clone(),
                fingerprint: self.fingerprint.to_hex(),
                short: self.fingerprint.short(),
                role: self.config.role,
                data_dir: self.store.root().display().to_string(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                port: self.port,
                platform: std::env::consts::OS.to_owned(),
                natural_scrolling: self.natural_scrolling,
                addresses: self.addresses.clone(),
                invite_port: self.invite_port,
                system: self.system.clone(),
                verbose_since: self.verbose_since,
            },
            sharing: !self.paused,
            problem: self.problem.clone(),
            link: Link {
                input_transport: focus.and_then(|link| link.input_transport.clone()),
                fallback_reason: focus.and_then(|link| link.fallback_reason.clone()),
                status: self.status,
                peer: focus.map(|l| l.peer.clone()),
                since: focus.map(|l| l.since),
                cursor_on_peer: self.cursor.is_some(),
                rtt_ms: focus.map_or_else(Vec::new, |l| l.rtt.history()),
            },
            clipboard: self.clipboard.clone().map(|mut c| {
                if let Some(peer) = &self.clipboard_peer {
                    c.peer = self.peer_name(peer);
                }
                c
            }),
            peers: self
                .peers
                .iter()
                .map(|p| PeerInfo {
                    input_transport: self
                        .links
                        .iter()
                        .find(|link| link.fingerprint == p.fingerprint)
                        .and_then(|link| link.input_transport.clone()),
                    fallback_reason: self
                        .links
                        .iter()
                        .find(|link| link.fingerprint == p.fingerprint)
                        .and_then(|link| link.fallback_reason.clone()),
                    name: p.name.clone(),
                    fingerprint: p.fingerprint.to_hex(),
                    short: p.fingerprint.short(),
                    online: self.links.iter().any(|l| l.fingerprint == p.fingerprint),
                    last_seen: self.last_seen.get(&p.fingerprint).copied(),
                    side: server.then(|| self.side_of(&p.fingerprint)),
                    cursor: self.cursor == Some(p.fingerprint),
                    alias: self.alias_of(&p.fingerprint).unwrap_or_default(),
                    platform: self
                        .platforms
                        .get(&p.fingerprint)
                        .cloned()
                        .unwrap_or_default(),
                    addr: p.addr.map(addr_text).unwrap_or_default(),
                    version: self
                        .versions
                        .get(&p.fingerprint)
                        .cloned()
                        .unwrap_or_default(),
                })
                .collect(),
            found: self
                .found
                .iter()
                .map(|f| FoundDevice {
                    name: f.name.clone(),
                    fingerprint: f.fingerprint.to_hex(),
                    short: f.fingerprint.short(),
                    addr: f
                        .addrs
                        .first()
                        .map(|a| a.ip().to_string())
                        .unwrap_or_default(),
                    paired: self.peers.contains(&f.fingerprint),
                })
                .collect(),
            settings: Settings {
                input_transport: self.config.input_transport,
                name: self.config.name.clone(),
                side: self.config.side,
                layout: self.config.layout.clone(),
                announce: self.config.announce,
                clipboard: self.config.clipboard,
                clipboard_limit_mb: self.config.clipboard_limit_mb,
                file_rate_limit_mbps: self
                    .config
                    .file_rate_limit_mbps
                    .min(Config::MAX_FILE_RATE_MBPS),
                coalesce_motion: self.config.coalesce_motion,
                mouse_dpi: self.config.mouse_dpi(),
                server: self.config.server.clone(),
                autostart: self.autostart,
                theme: self.config.theme,
                accent: self.config.accent(),
                language: self.config.language,
                auto_update: self.config.auto_update,
                devices: self.config.devices.clone(),
            },
            activity: self.activity.recent(),
            update: self.updates.info(!server && !self.links.is_empty()),
        }
    }
}

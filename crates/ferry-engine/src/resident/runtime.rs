//! 常驻运行的状态、引擎生命周期与事件分派。

mod commands;
mod projection;
mod reporting;
mod settings;
mod tasks;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::convert::Infallible;
use std::fs;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferry_core::Redactor;
use ring::rand::{SecureRandom, SystemRandom};

use ferry_ipc::{
    ClipboardImage, ClipboardSync, DialError as IpcDialError, FoundDevice, Link, LinkStatus, Local,
    LogIcon, LogKind, Notice, PairingPrompt, PeerInfo, PeerRef, Phrase, Problem, ProblemKind,
    Request, Settings, SettingsPatch, State,
};
use ferry_store::Store;
use ferry_trust::{Fingerprint, PeerList};
use ferry_types::{
    ClipboardFormat, Config, DeviceSettings, Lang, Role, Side, mark, uses_command_key,
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{info, warn};

use super::confirm::{PairAsk, UiConfirm};
use super::diagnostics;
use super::record::{Activity, Log, Rtt, now_ms};
use super::ui::UiEvent;
use super::update::{Outcome, UpdateEvent, Updates};
use super::{Command, Host, Summary};
use crate::client::{ClientCommand, ClientConfig, ServerAddr, connect};
use crate::clipboard::ClipLimit;
use crate::edge_drop::EdgeDrop;
use crate::error::{Error, Result};
use crate::events::{DialError, Event, Events, FoundServer, TransferState, TransferUpdate};
use crate::files::Files;
use crate::invite::{INVITE_PORT, invite};
use crate::link::DEFAULT_PORT;
use crate::server::{Live, ServerConfig, serve};

/// 缺少权限时多久再试一次：用户在系统设置里勾上之后很快就能用。
const RETRY_PERMISSION: Duration = Duration::from_secs(2);
/// 配对被拒绝后多久再试。
const RETRY_PAIRING: Duration = Duration::from_secs(3);
/// 其他问题多久再试。
const RETRY_OTHER: Duration = Duration::from_secs(5);
/// 多久重新读一次已配对设备（用户可能用命令行删了设备）。
const PEERS_REFRESH: Duration = Duration::from_secs(5);
/// 另一台设备请本机打开设置时，设置界面在这么久之内连上来才翻到它的设置页。
const SHOW_DEVICE_WINDOW: Duration = Duration::from_secs(30);

struct EngineTask {
    handle: JoinHandle<Result<Infallible>>,
    /// 服务端的即时设置；客户端没有。
    live: Option<watch::Sender<Live>>,
    /// 客户端的命令；服务端没有。
    client: Option<mpsc::UnboundedSender<ClientCommand>>,
}

pub(crate) enum WorkResult {
    PlatformInfo {
        addresses: Vec<String>,
        natural_scrolling: Option<bool>,
    },
    EngineExit(Result<Infallible>),
    Sent {
        from: u64,
        command_id: Option<u64>,
        result: Result<String, String>,
    },
    RunLog {
        from: u64,
        chunk: diagnostics::Chunk,
        request_id: Option<u64>,
    },
    Diagnostic {
        from: u64,
        command_id: Option<u64>,
        result: Result<(Option<PathBuf>, String), String>,
    },
    Preview {
        generation: u64,
        result: Result<PathBuf, String>,
    },
}

struct Pending {
    prompt: PairingPrompt,
    reply: oneshot::Sender<bool>,
}

/// 一台连着的设备。
struct LinkInfo {
    fingerprint: Fingerprint,
    peer: PeerRef,
    /// 连上的时刻（Unix 毫秒）。
    since: i64,
    started: Instant,
    rtt: Rtt,
}

pub(crate) struct Runtime<H: Host> {
    store: Store,
    session: String,
    performance: BTreeMap<String, ferry_ipc::PeerPerformance>,
    command_error: Option<String>,
    next_operation: u64,
    dial_requests: HashMap<u64, (u64, u64, Instant)>,
    host: Arc<H>,
    workers: JoinSet<WorkResult>,
    background: JoinSet<()>,
    generation_signal: watch::Sender<u64>,
    addresses: Vec<String>,
    natural_scrolling: Option<bool>,
    platform_info_at: Instant,
    platform_info_pending: bool,
    preview_generation: u64,
    preview_busy: bool,
    preview_pending: Option<(u64, Arc<[u8]>)>,
    config: Config,
    name: String,
    fingerprint: Fingerprint,
    events: Events,
    asks: mpsc::UnboundedSender<PairAsk>,
    /// 文件传输。引擎重启后仍保留，断线重连后接着传。
    files: Files,
    /// 边缘投递胶囊。
    edge: EdgeDrop,

    paused: bool,
    engine: Option<EngineTask>,
    generation: u64,
    retry_at: Option<Instant>,
    problem: Option<Problem>,

    status: LinkStatus,
    /// 服务端正在监听的端口。
    port: Option<u16>,
    /// 连着的设备，按连上的先后。
    links: Vec<LinkInfo>,
    /// 光标所在的那台；客户端上，光标不在本机时是服务端。
    cursor: Option<Fingerprint>,
    /// 摘要里的那台，也是 `send_files` 不指定时发给的那台：光标最近去过的，否则最近连上的。
    focus: Option<Fingerprint>,
    clipboard: Option<ClipboardSync>,
    /// 最近一次剪贴板同步的对方。界面上的名字按它现在的名字显示（中途可能改了名）。
    clipboard_peer: Option<Fingerprint>,
    /// 剪贴板单项的大小上限，交给引擎共用，改设置时立即生效。
    clip_limit: ClipLimit,
    /// 最近一次同步的剪贴板图片存在磁盘上的那份，给设置界面预览。
    clip_preview: Option<PathBuf>,
    peers: PeerList,
    peers_read: Instant,
    last_seen: HashMap<Fingerprint, i64>,
    found: Vec<FoundServer>,
    autostart: bool,

    log: Log,
    activity: Activity,
    last_second: Instant,

    clients: BTreeMap<u64, mpsc::Sender<Notice>>,
    logs_in_flight: HashSet<u64>,
    next_client: u64,
    /// 界面或 `quit` 命令请后台退出了。
    quit_requested: bool,
    pending: BTreeMap<u64, Pending>,
    next_prompt: u64,
    dirty: bool,
    /// 上次要打开设置界面的时刻，界面连上时据此记下启动用了多久。
    settings_opened: Option<Instant>,
    /// 本次运行中连上过的设备是什么系统。
    platforms: HashMap<Fingerprint, String>,
    /// 另一台设备请本机打开设置：等设置界面连上后翻到它（指纹的十六进制）的设置页。
    show_device: Option<(String, Instant)>,
    /// 客户端：等邀请的端口。
    invite_port: Option<u16>,
    /// 本机系统和版本，启动时问一次。
    system: String,
    /// 详细记录从什么时候开着（Unix 毫秒）。
    verbose_since: Option<i64>,
    /// 最近导出的诊断文件。
    last_export: Option<PathBuf>,
    /// 实际用的界面语言（托盘、屏幕边缘的胶囊）。
    lang: Lang,
    /// 版本更新。
    updates: Updates<H::Updater>,
    /// 本次运行中连上过的设备告诉的版本。
    versions: HashMap<Fingerprint, String>,
    /// 启动时的提示（例如“已更新到 …”），等第一个界面连上来再发。
    startup_toast: Option<(Phrase, String)>,
}

impl<H: Host> Runtime<H> {
    pub(crate) fn new(
        store: Store,
        host: H,
        events: Events,
        asks: mpsc::UnboundedSender<PairAsk>,
        update_tx: mpsc::UnboundedSender<UpdateEvent>,
    ) -> Result<Self> {
        let mut epoch = [0u8; 16];
        SystemRandom::new()
            .fill(&mut epoch)
            .map_err(|_| std::io::Error::other("无法生成后台会话标识"))?;
        let session = epoch.iter().map(|b| format!("{b:02x}")).collect();
        let config = store.config()?;
        let updates = Updates::new(
            host.updater(),
            host.update_source(),
            store.clone(),
            update_tx,
        );
        let name = resolve_name(&config, &host);
        let fingerprint = store.identity(&name)?.fingerprint();
        let peers = store.peers()?;
        let autostart = host.autostart();
        let system = host.system();
        let lang = config.language.resolve(host.system_language());
        let files = Files::new(&store, events.clone(), host.file_committer());
        files.set_rate_limit(config.file_rate_limit_bytes());
        let edge = EdgeDrop::spawn(
            host.overlay(),
            files.clone(),
            accent_color(config.accent),
            lang,
        );
        let clip_limit = ClipLimit::new(config.clipboard_limit_bytes());
        // 上次留下的剪贴板图片预览。
        if let Err(e) = store.clear_blobs() {
            warn!("清理上次的剪贴板预览失败：{e}");
        }
        let addresses = ferry_net::local_addresses()
            .into_iter()
            .map(|ip| ip.to_string())
            .collect();
        let natural_scrolling = host.natural_scrolling();
        let mut runtime = Self {
            store,
            addresses,
            natural_scrolling,
            platform_info_at: Instant::now(),
            platform_info_pending: false,
            session,
            performance: BTreeMap::new(),
            command_error: None,
            next_operation: 1,
            dial_requests: HashMap::new(),
            host: Arc::new(host),
            workers: JoinSet::new(),
            background: JoinSet::new(),
            generation_signal: watch::Sender::new(0),
            preview_generation: 0,
            preview_busy: false,
            preview_pending: None,
            config,
            name,
            fingerprint,
            events,
            asks,
            files,
            edge,
            paused: false,
            engine: None,
            generation: 0,
            retry_at: None,
            problem: None,
            status: LinkStatus::Starting,
            port: None,
            links: Vec::new(),
            cursor: None,
            focus: None,
            clipboard: None,
            clipboard_peer: None,
            clip_limit,
            clip_preview: None,
            peers,
            peers_read: Instant::now(),
            last_seen: HashMap::new(),
            found: Vec::new(),
            autostart,
            log: Log::default(),
            activity: Activity::default(),
            last_second: Instant::now(),
            clients: BTreeMap::new(),
            logs_in_flight: HashSet::new(),
            next_client: 1,
            quit_requested: false,
            pending: BTreeMap::new(),
            next_prompt: 1,
            dirty: true,
            settings_opened: None,
            platforms: HashMap::new(),
            show_device: None,
            invite_port: None,
            system,
            verbose_since: None,
            last_export: None,
            lang,
            updates,
            versions: HashMap::new(),
            startup_toast: None,
        };
        let (role, role_key) = match runtime.config.role {
            Role::Server => ("键鼠在本机", "server"),
            Role::Client => ("由另一台电脑控制", "client"),
        };
        let phrase = Phrase::new("started")
            .arg("name", &runtime.name)
            .arg("role", role_key);
        runtime.log(
            LogKind::Link,
            LogIcon::Power,
            phrase,
            "ScreenFerry 已启动",
            format!("{} · {role}", mark::device(&runtime.name)),
        );
        let (outcomes, open_settings) = runtime.updates.startup();
        runtime.apply(outcomes);
        if open_settings {
            runtime.open_settings();
        }
        Ok(runtime)
    }

    pub(crate) fn next_client_id(&mut self) -> u64 {
        let id = self.next_client;
        self.next_client += 1;
        id
    }

    /// 有人请后台退出了（`quit` 命令）。
    pub(crate) fn quit_requested(&self) -> bool {
        self.quit_requested
    }

    /// 退出前告诉所有界面一起关闭，并放开它们的发送队列：每个界面的发送任务写完这条后结束。
    /// 返回等待发送完毕的具体界面，旧连接迟到的Left不能代替它们。
    pub(crate) fn farewell(&mut self) -> HashSet<u64> {
        self.broadcast(Notice::Quit);
        let waiting = self.clients.keys().copied().collect();
        self.clients.clear();
        waiting
    }

    // ── 引擎 ─────────────────────────────────────

    pub(crate) fn start_engine(&mut self) {
        if self.paused || self.engine.is_some() {
            return;
        }
        self.retry_at = None;
        self.status = LinkStatus::Starting;
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
        self.generation_signal.send_replace(self.generation);
        let confirm = UiConfirm {
            generation: self.generation,
            asks: self.asks.clone(),
        };
        let started = match self.config.role {
            Role::Server => self.spawn_server(confirm),
            Role::Client => self.spawn_client(confirm),
        };
        if let Err(e) = started {
            self.fail(e);
        }
    }

    fn spawn_server(&mut self, confirm: UiConfirm) -> Result<()> {
        self.host.check_permissions(Role::Server)?;
        let capture = self.host.capture()?;
        let (live_tx, live_rx) = watch::channel(self.live());
        let config = ServerConfig {
            live: live_rx,
            name: self.name.clone(),
            clipboard: self.config.clipboard,
            clipboard_limit: self.clip_limit.clone(),
            command_key: uses_command_key(),
            store: self.store.clone(),
            announce: self.config.announce,
            events: self.events.scoped(self.generation),
            files: self.files.clone(),
            edge: self.edge.feed(),
        };
        let (displays, clipboard) = (self.host.displays(), self.host.clipboard());
        let port = self.host.listen_port();
        let handle = tokio::spawn(async move {
            let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await?;
            serve(listener, config, capture, displays, clipboard, confirm).await
        });
        self.engine = Some(EngineTask {
            handle,
            live: Some(live_tx),
            client: None,
        });
        Ok(())
    }

    fn spawn_client(&mut self, confirm: UiConfirm) -> Result<()> {
        self.host.check_permissions(Role::Client)?;
        let inject = self.host.inject()?;
        let address = self.config.server.trim().to_owned();
        let server = ServerAddr::Discover;
        let (commands_tx, commands) = mpsc::unbounded_channel();
        let config = ClientConfig {
            server,
            name: self.name.clone(),
            clipboard: self.config.clipboard,
            clipboard_limit: self.clip_limit.clone(),
            command_key: uses_command_key(),
            store: self.store.clone(),
            events: self.events.scoped(self.generation),
            files: self.files.clone(),
            edge: self.edge.feed(),
            devices: self.config.devices.clone(),
            commands: Some(commands),
            invite_port: Some(self.host.invite_port()),
        };
        let (displays, clipboard) = (self.host.displays(), self.host.clipboard());
        let handle = tokio::spawn(async move {
            let mut config = config;
            if !address.is_empty() {
                config.server = ServerAddr::Fixed(
                    resolve_input(&address, DEFAULT_PORT)
                        .await
                        .ok_or_else(|| Error::BadAddress(address))?,
                );
            }
            connect(config, inject, displays, clipboard, confirm).await
        });
        self.engine = Some(EngineTask {
            handle,
            live: None,
            client: Some(commands_tx),
        });
        Ok(())
    }

    /// 停下引擎：丢弃它的任务，引擎随之归还鼠标控制、松开按键、断开连接。
    pub(crate) async fn stop_engine(&mut self) {
        self.background.abort_all();
        self.generation = self.generation.wrapping_add(1);
        self.generation_signal.send_replace(self.generation);
        if let Some(task) = self.engine.take() {
            task.handle.abort();
            let _ = task.handle.await;
        }
        let closed: Vec<_> = self.pending.keys().copied().collect();
        for id in closed {
            self.answer(id, false);
        }
        self.edge.feed().reset();
        self.invite_port = None;
        self.drop_link();
        self.retry_at = None;
    }

    async fn restart_engine(&mut self) {
        self.stop_engine().await;
        self.start_engine();
    }

    pub(crate) fn on_engine_exit(&mut self, result: Result<Infallible>) {
        self.background.abort_all();
        self.generation = self.generation.wrapping_add(1);
        self.generation_signal.send_replace(self.generation);
        let closed: Vec<_> = self.pending.keys().copied().collect();
        for id in closed {
            self.answer(id, false);
        }
        self.edge.feed().reset();
        self.invite_port = None;
        let Err(e) = result;
        warn!("引擎停止：{e}");
        self.drop_link();
        self.fail(e);
    }

    /// 引擎无法工作：记下问题，过一会儿再试。
    fn fail(&mut self, e: Error) {
        self.dirty = true;
        let (kind, delay) = match &e {
            Error::Platform(ferry_platform::Error::PermissionDenied(_)) => {
                (ProblemKind::Permission, RETRY_PERMISSION)
            }
            Error::Io(io) if io.kind() == ErrorKind::AddrInUse => {
                (ProblemKind::PortInUse, RETRY_OTHER)
            }
            Error::PairingRejected => {
                // 不是故障：用户或对方拒绝了配对，稍后照常寻找。
                self.status = LinkStatus::Searching;
                self.retry_at = Some(Instant::now() + RETRY_PAIRING);
                return;
            }
            _ => (ProblemKind::Other, RETRY_OTHER),
        };
        let message = match kind {
            ProblemKind::Permission => {
                "需要在系统设置的“隐私与安全性”中允许 ScreenFerry 控制键盘和鼠标".to_owned()
            }
            ProblemKind::PortInUse => format!(
                "端口 {} 被占用：可能还开着 Deskflow 或另一份 ScreenFerry",
                self.host.listen_port()
            ),
            ProblemKind::Other => e.to_string(),
        };
        let port = (kind == ProblemKind::PortInUse).then(|| self.host.listen_port());
        if self.problem.as_ref().is_none_or(|p| p.message != message) {
            let key = match kind {
                ProblemKind::Permission => "permission",
                ProblemKind::PortInUse => "port_in_use",
                ProblemKind::Other => "other",
            };
            let mut phrase = Phrase::new("cannot_share")
                .arg("problem", key)
                .arg("message", &message);
            if let Some(port) = port {
                phrase = phrase.arg("port", port);
            }
            self.log(
                LogKind::Link,
                LogIcon::Warning,
                phrase,
                "无法开始共享",
                message.clone(),
            );
        }
        self.problem = Some(Problem {
            kind,
            message,
            port,
        });
        self.status = LinkStatus::Stopped;
        self.retry_at = Some(Instant::now() + delay);
    }

    fn clear_problem(&mut self) {
        if self.problem.take().is_some() {
            self.dirty = true;
        }
    }

    fn live(&self) -> Live {
        Live {
            side: self.config.side,
            layout: self.config.layout.clone(),
            devices: self.config.devices.clone(),
            fallback: DeviceSettings::default(),
            mouse_dpi: self.config.mouse_dpi(),
            input: crate::InputSettings {
                coalesce_motion: self.config.coalesce_motion,
            },
        }
    }

    /// 连接全没了（引擎停下）：记下各台设备最后在线的时刻，清空与连接有关的状态。
    fn drop_link(&mut self) {
        self.performance.clear();
        if self.engine.is_none() {
            self.port = None;
        }
        let now = now_ms();
        for link in self.links.drain(..) {
            self.last_seen.insert(link.fingerprint, now);
        }
        self.cursor = None;
        self.focus = None;
        self.dirty = true;
    }

    fn link_mut(&mut self, fingerprint: &Fingerprint) -> Option<&mut LinkInfo> {
        self.links
            .iter_mut()
            .find(|l| l.fingerprint == *fingerprint)
    }

    /// 摘要里的那台连着的设备。
    fn focus_link(&self) -> Option<&LinkInfo> {
        self.focus
            .and_then(|f| self.links.iter().find(|l| l.fingerprint == f))
            .or(self.links.last())
    }

    /// 一台设备的名字：本机给它起过名字就用起的，否则连着时用它报的名字，再否则用配对时记下的。
    fn peer_name(&self, fingerprint: &Fingerprint) -> String {
        self.alias_of(fingerprint)
            .or_else(|| {
                self.links
                    .iter()
                    .find(|l| l.fingerprint == *fingerprint)
                    .map(|l| l.peer.name.clone())
            })
            .or_else(|| self.peers.get(fingerprint).map(|p| p.name.clone()))
            .unwrap_or_else(|| "对方".to_owned())
    }

    /// 本机给一台设备起的名字。
    fn alias_of(&self, fingerprint: &Fingerprint) -> Option<String> {
        let hex = fingerprint.to_hex();
        self.config
            .devices
            .iter()
            .find(|d| d.fingerprint == hex)
            .map(|d| d.alias.trim())
            .filter(|alias| !alias.is_empty())
            .map(str::to_owned)
    }

    /// 服务端：一台设备在本机哪一侧。
    fn side_of(&self, fingerprint: &Fingerprint) -> Side {
        let hex = fingerprint.to_hex();
        self.config
            .layout
            .iter()
            .find(|p| p.fingerprint == hex)
            .map_or(self.config.side, |p| p.side)
    }

    // ── 引擎事件 ─────────────────────────────────

    pub(crate) fn on_engine(&mut self, event: Event) {
        let event = match event {
            Event::Engine { generation, event } if generation == self.generation => *event,
            Event::Engine { .. } => return,
            event => event,
        };
        let progress = matches!(
            &event,
            Event::Transfer(TransferUpdate {
                state: TransferState::Running,
                ..
            })
        );
        if !matches!(event, Event::Rtt { .. }) && !progress {
            self.dirty = true;
        }
        match event {
            Event::Engine { .. } => {}
            Event::Metrics { peer, metrics } => {
                if peer.is_some_and(|peer| {
                    !self.links.iter().any(|link| {
                        link.fingerprint == peer
                            && metrics.collected_at.is_some_and(|at| at >= link.started)
                    })
                }) {
                    return;
                }
                self.performance.insert(
                    peer.map(|p| p.to_hex()).unwrap_or_default(),
                    super::performance::project(peer, *metrics),
                );
            }
            Event::Listening { port, announced } => {
                self.status = LinkStatus::Waiting;
                self.port = Some(port);
                self.clear_problem();
                let detail = if announced {
                    format!("等待另一台电脑连接 · 端口 {port}")
                } else {
                    format!("端口 {port} · 没有在局域网里广播")
                };
                let phrase = Phrase::new("listening")
                    .arg("port", port)
                    .arg("announced", announced);
                self.log(LogKind::Link, LogIcon::Power, phrase, "已开始共享", detail);
                self.invite_known(port);
            }
            Event::InviteListening { port } => self.invite_port = Some(port),
            Event::Dialed {
                request_id,
                address,
                invite,
                error,
            } => {
                self.dial_result(request_id, address, invite, error.map(ipc_dial_error));
            }
            Event::Searching => {
                if self.links.is_empty() {
                    self.status = LinkStatus::Searching;
                }
                self.clear_problem();
            }
            Event::Found(list) => self.found = list,
            Event::Connecting { .. } => {
                if self.links.is_empty() {
                    self.status = LinkStatus::Connecting;
                }
            }
            Event::Connected {
                name,
                fingerprint,
                addr,
                platform,
            } => {
                if let Some(platform) = platform {
                    self.platforms.insert(fingerprint, platform);
                }
                self.status = LinkStatus::Connected;
                self.clear_problem();
                self.performance.remove(&fingerprint.to_hex());
                self.links.retain(|l| l.fingerprint != fingerprint);
                self.links.push(LinkInfo {
                    fingerprint,
                    peer: PeerRef {
                        name: name.clone(),
                        fingerprint: fingerprint.to_hex(),
                        short: fingerprint.short(),
                        addr: addr.ip().to_string(),
                    },
                    since: now_ms(),
                    started: Instant::now(),
                    rtt: Rtt::default(),
                });
                self.focus = Some(fingerprint);
                // 刚连上时光标总在服务端。
                if self.config.role == Role::Client {
                    self.cursor = Some(fingerprint);
                }
                self.refresh_peers();
                let name = self.alias_of(&fingerprint).unwrap_or(name);
                let phrase = Phrase::new("connected")
                    .arg("name", &name)
                    .arg("ip", addr.ip());
                self.log(
                    LogKind::Link,
                    LogIcon::Link,
                    phrase,
                    format!("已连接 {}", mark::device(&name)),
                    format!("TLS 1.3 · {}", addr.ip()),
                );
            }
            Event::Disconnected { peer, reason } => {
                self.performance.remove(&peer.to_hex());
                let name = self.peer_name(&peer);
                self.links.retain(|l| l.fingerprint != peer);
                self.last_seen.insert(peer, now_ms());
                if self.cursor == Some(peer) {
                    self.cursor = None;
                }
                if self.focus == Some(peer) {
                    self.focus = self.links.last().map(|l| l.fingerprint);
                }
                self.status = match (self.links.is_empty(), self.config.role) {
                    (false, _) => LinkStatus::Connected,
                    (true, Role::Server) => LinkStatus::Waiting,
                    (true, Role::Client) => LinkStatus::Searching,
                };
                let mut phrase = Phrase::new("disconnected").arg("name", &name);
                if let Some(reason) = &reason {
                    phrase = phrase.arg("reason", reason);
                }
                self.log(
                    LogKind::Link,
                    LogIcon::Unlink,
                    phrase,
                    format!("与 {} 断开", mark::device(&name)),
                    reason.unwrap_or_else(|| "对方已断开".into()),
                );
            }
            Event::Rtt { peer, rtt } => {
                if let Some(link) = self.link_mut(&peer) {
                    link.rtt.record(rtt);
                }
            }
            Event::Transfer(update) => self.on_transfer(update),
            Event::OpenSettings { peer } => {
                let name = self.peer_name(&peer);
                self.log(
                    LogKind::Link,
                    LogIcon::Settings,
                    Phrase::new("peer_opened_settings").arg("name", &name),
                    format!("{} 请本机打开设置", mark::device(&name)),
                    "已翻到它的设置页",
                );
                // 已经开着的界面直接翻过去；没开的，打开后连上来时再翻。
                if self.clients.is_empty() {
                    self.show_device = Some((peer.to_hex(), Instant::now()));
                } else {
                    self.broadcast(Notice::ShowDevice {
                        fingerprint: peer.to_hex(),
                    });
                }
                self.open_settings();
            }
            Event::PeerVersion { peer, version } => {
                self.versions.insert(peer, version);
            }
            Event::PeerSettings { peer, sent } => {
                let name = self.peer_name(&peer);
                if sent {
                    self.toast(
                        Phrase::new("peer_settings_opened").arg("name", &name),
                        format!("已在 {name} 上打开设置，把光标移过去就能改"),
                    );
                } else {
                    self.toast(
                        Phrase::new("peer_settings_unsupported").arg("name", &name),
                        format!("{name} 上的 ScreenFerry 版本较旧，请在那台电脑上打开设置"),
                    );
                }
            }
            Event::Cursor { here, hotkey, peer } => {
                self.cursor = (!here).then_some(peer);
                if !here {
                    self.focus = Some(peer);
                }
                let side = self.side_of(&peer);
                let name = self.peer_name(&peer);
                let peer = mark::device(&name).to_string();
                let (phrase, text, detail) = match (self.config.role, here) {
                    (Role::Server, false) => {
                        self.activity.switched();
                        (
                            Phrase::new("cursor_to_peer")
                                .arg("name", &name)
                                .arg("side", side_key(side)),
                            format!("光标切到 {peer}"),
                            format!("从本机{}边缘", side_label(side)),
                        )
                    }
                    (Role::Server, true) => (
                        Phrase::new("cursor_back").arg("hotkey", hotkey),
                        "光标回到本机".to_owned(),
                        if hotkey {
                            "快捷键 Ctrl + Option + Esc"
                        } else {
                            "从对方屏幕边缘"
                        }
                        .to_owned(),
                    ),
                    (Role::Client, true) => {
                        self.activity.switched();
                        (
                            Phrase::new("cursor_here").arg("name", &name),
                            "光标来到本机".to_owned(),
                            format!("从 {peer}"),
                        )
                    }
                    (Role::Client, false) => (
                        Phrase::new("cursor_returned").arg("name", &name),
                        format!("光标回到 {peer}"),
                        String::new(),
                    ),
                };
                self.log(LogKind::Cursor, LogIcon::Cursor, phrase, text, detail);
            }
            Event::Clipboard {
                peer,
                to_peer,
                summary,
            } => {
                self.clipboard_peer = Some(peer);
                let peer = self.peer_name(&peer);
                self.activity.clipped();
                let png_bytes = summary.image.as_ref().map(|i| i.png.len() as u64);
                let detail = match (&summary.image, summary.chars) {
                    (Some(image), _) => format!(
                        "图片 · {}×{} · {}",
                        image.width,
                        image.height,
                        size_text(image.png.len() as u64)
                    ),
                    (None, Some(n)) if summary.rich => format!("带格式的文本 · {n} 字"),
                    (None, Some(n)) => format!("文本 · {n} 字"),
                    (None, None) => size_text(summary.bytes),
                };
                self.preview_generation = self.preview_generation.wrapping_add(1);
                let image = summary.image.map(|image| ClipboardImage {
                    width: image.width,
                    height: image.height,
                    path: self.save_preview(image.png.clone()),
                });
                self.clipboard = Some(ClipboardSync {
                    to_peer,
                    peer: peer.clone(),
                    chars: summary.chars,
                    bytes: summary.bytes,
                    preview: summary.preview,
                    rich: summary.rich,
                    image,
                    at: now_ms(),
                });
                let text = if to_peer {
                    format!("剪贴板同步到 {}", mark::device(&peer))
                } else {
                    "剪贴板同步到本机".to_owned()
                };
                let mut phrase = Phrase::new("clipboard_synced")
                    .arg("name", &peer)
                    .arg("to_peer", to_peer)
                    .arg("bytes", summary.bytes);
                phrase = match (
                    &self.clipboard.as_ref().and_then(|c| c.image.as_ref()),
                    summary.chars,
                ) {
                    (Some(image), _) => phrase
                        .arg("content", "image")
                        .arg("bytes", png_bytes.unwrap_or(summary.bytes))
                        .arg("width", image.width)
                        .arg("height", image.height),
                    (None, Some(n)) => phrase
                        .arg("content", if summary.rich { "rich" } else { "text" })
                        .arg("chars", n),
                    (None, None) => phrase.arg("content", "data"),
                };
                self.log(LogKind::Clip, LogIcon::Clipboard, phrase, text, detail);
            }
            Event::ClipboardSkipped {
                peer,
                to_peer,
                format,
                bytes,
                limit,
            } => {
                let peer = self.peer_name(&peer);
                let what = match format {
                    ClipboardFormat::PNG => "图片",
                    ClipboardFormat::TEXT => "文本",
                    _ => "带格式的内容",
                };
                let text = if to_peer {
                    format!("剪贴板里的{what}太大，没有同步到 {}", mark::device(&peer))
                } else {
                    format!("{} 剪贴板里的{what}太大，没有同步过来", mark::device(&peer))
                };
                let detail = format!(
                    "{}，超过了 {} 的上限 · 可以在设置里调大",
                    size_text(bytes),
                    size_text(limit)
                );
                let format_key = match format {
                    ClipboardFormat::PNG => "image",
                    ClipboardFormat::TEXT => "text",
                    _ => "rich",
                };
                let phrase = Phrase::new("clipboard_skipped")
                    .arg("name", &peer)
                    .arg("to_peer", to_peer)
                    .arg("format", format_key)
                    .arg("bytes", bytes)
                    .arg("limit", limit);
                self.log(LogKind::Clip, LogIcon::Warning, phrase, text, detail);
            }
            Event::Paired { name, fingerprint } => {
                self.refresh_peers();
                let phrase = Phrase::new("paired")
                    .arg("name", &name)
                    .arg("short", fingerprint.short());
                self.log(
                    LogKind::Link,
                    LogIcon::Lock,
                    phrase.clone(),
                    format!("已与 {} 配对", mark::device(&name)),
                    format!("指纹 {}", fingerprint.short()),
                );
                self.toast(phrase, format!("已与 {name} 配对"));
            }
            Event::PairingRejected { name, by_peer } => {
                let text = if by_peer {
                    format!("{} 拒绝了配对", mark::device(&name))
                } else {
                    format!("已拒绝 {} 的配对请求", mark::device(&name))
                };
                let phrase = Phrase::new("pairing_rejected")
                    .arg("name", &name)
                    .arg("by_peer", by_peer);
                self.log(LogKind::Link, LogIcon::Warning, phrase, text, "");
            }
        }
    }

    // ── 配对 ─────────────────────────────────────

    pub(crate) fn on_pairing(&mut self, ask: PairAsk) {
        if ask.generation != self.generation || ask.reply.is_closed() || self.paused {
            let _ = ask.reply.send(false);
            return;
        }
        let id = self.next_prompt;
        self.next_prompt += 1;
        let request = &ask.request;
        let prompt = PairingPrompt {
            session: self.session.clone(),
            id,
            peer_name: request.peer_name.clone(),
            fingerprint: request.peer_fingerprint.to_hex(),
            short: request.peer_fingerprint.short(),
            code: request.code.to_string(),
        };
        let phrase = Phrase::new("pairing_request")
            .arg("name", &request.peer_name)
            .arg("code", request.code);
        self.log(
            LogKind::Link,
            LogIcon::Lock,
            phrase,
            format!("{} 请求配对", mark::device(&request.peer_name)),
            format!("配对码 {}", request.code),
        );
        self.broadcast(Notice::Pairing {
            prompt: prompt.clone(),
        });
        self.pending.insert(
            id,
            Pending {
                prompt,
                reply: ask.reply,
            },
        );
        // 没有界面在看：打开设置界面，用户才看得到配对码。
        if self.clients.is_empty() {
            self.host.open_settings();
        }
    }

    fn answer(&mut self, id: u64, accept: bool) {
        if let Some(pending) = self.pending.remove(&id) {
            let _ = pending.reply.send(accept);
            self.broadcast(Notice::PairingClosed { id });
        }
    }

    // ── 界面 ─────────────────────────────────────

    pub(crate) async fn on_ui(&mut self, event: UiEvent) {
        match event {
            UiEvent::Joined { id, outbox } => {
                let mut initial = vec![
                    Notice::History {
                        entries: self.log.entries(),
                    },
                    Notice::State {
                        state: Box::new(self.snapshot()),
                    },
                ];
                initial.extend(self.pending.values().map(|p| Notice::Pairing {
                    prompt: p.prompt.clone(),
                }));
                if let Some((fingerprint, at)) = self.show_device.take()
                    && at.elapsed() < SHOW_DEVICE_WINDOW
                {
                    initial.push(Notice::ShowDevice { fingerprint });
                }
                for notice in initial {
                    if outbox.try_send(notice).is_err() {
                        return;
                    }
                }
                if let Some((phrase, text)) = self.startup_toast.take() {
                    let _ = outbox.try_send(Notice::Toast {
                        text,
                        phrase: Some(phrase),
                    });
                }
                self.clients.insert(id, outbox);
                // 超过一分钟的不算：多半是那次没打开，这次是界面自己重连。
                if let Some(ms) = self
                    .settings_opened
                    .take()
                    .map(|at| at.elapsed().as_millis())
                    .filter(|&ms| ms < 60_000)
                {
                    info!(ms, "设置界面从打开到连上后台用了 {ms} 毫秒");
                }
            }
            UiEvent::Request { id, request } => self.on_request(id, request).await,
            UiEvent::Left { id } => {
                self.logs_in_flight.remove(&id);
                self.clients.remove(&id);
            }
        }
    }

    /// 文件传输有了进展：开始、结束、中断都记进日志，过程中的进度不记。
    fn on_transfer(&mut self, update: TransferUpdate) {
        self.edge.transfer(update.clone());
        let name = self.peer_name(&update.peer);
        let peer = mark::device(&name).to_string();
        let what = if update.items > 1 {
            format!("{} 等 {} 项", mark::file(&update.name), update.items)
        } else {
            mark::file(&update.name).to_string()
        };
        let size = size_text(update.bytes);
        let files = format!("{} 个文件 · {size}", update.files);
        // 界面按当前语言说：文件名、项数、文件数、字节数都带上。
        let base = |key: &str| {
            Phrase::new(key)
                .arg("name", &name)
                .arg("file", &update.name)
                .arg("items", update.items)
                .arg("files", update.files)
                .arg("bytes", update.bytes)
        };
        let (phrase, text, detail) = match (&update.state, update.outgoing) {
            (TransferState::Running, _) => return,
            (TransferState::Waiting, true) => (
                base("transfer_sending"),
                format!("发送 {what} 到 {peer}"),
                files,
            ),
            (TransferState::Waiting, false) => (
                base("transfer_receiving"),
                format!("{peer} 发来 {what}"),
                files,
            ),
            (TransferState::Done { .. }, true) => (
                base("transfer_sent"),
                format!("已发送 {what}"),
                format!("到 {peer} · {size}"),
            ),
            (TransferState::Done { saved }, false) => {
                let folder = saved.first().and_then(|p| p.parent());
                let place = folder.map_or_else(String::new, |p| format!("保存在 {}", p.display()));
                let mut phrase = base("transfer_received");
                if let Some(folder) = folder {
                    phrase = phrase.arg("folder", folder.display());
                }
                (phrase, format!("已收到 {what}"), place)
            }
            (TransferState::Interrupted, _) => (
                base("transfer_interrupted"),
                format!("传输中断：{what}"),
                "重新连上后接着传".into(),
            ),
            (TransferState::Cancelled { by_peer }, _) => (
                base("transfer_cancelled").arg("by_peer", by_peer),
                format!("已取消：{what}"),
                if *by_peer {
                    "对方取消了"
                } else {
                    "本机取消了"
                }
                .into(),
            ),
            (TransferState::Failed { reason }, _) => (
                base("transfer_failed").arg("reason", reason),
                format!("传输失败：{what}"),
                reason.clone(),
            ),
        };
        self.log(LogKind::File, LogIcon::File, phrase, text, detail);
    }

    fn open_settings(&mut self) {
        self.settings_opened = Some(Instant::now());
        self.host.open_settings();
    }

    pub(crate) async fn on_command(&mut self, command: Command) {
        match command {
            Command::SetSharing(on) => self.set_sharing(on).await,
            Command::OpenSettings => self.open_settings(),
            Command::InstallUpdate => self.updates.install(!self.clients.is_empty()),
            Command::Quit => {}
        }
    }

    // ── 版本更新 ─────────────────────────────────

    pub(crate) fn on_update(&mut self, event: UpdateEvent) {
        let outcomes = self.updates.on_event(event, self.config.auto_update);
        self.apply(outcomes);
        self.dirty = true;
    }

    /// 为安装更新退出时（macOS）交给 `run` 的安装。
    pub(crate) fn take_restart(&mut self) -> Option<(Arc<H::Updater>, ferry_platform::Prepared)> {
        self.updates.take_restart()
    }

    fn apply(&mut self, outcomes: Vec<Outcome>) {
        for outcome in outcomes {
            match outcome {
                Outcome::Toast(phrase, text) if self.clients.is_empty() => {
                    self.startup_toast = Some((phrase, text));
                }
                Outcome::Toast(phrase, text) => self.toast(phrase, text),
                Outcome::Log {
                    warning,
                    phrase,
                    text,
                    detail,
                } => {
                    let icon = if warning {
                        LogIcon::Warning
                    } else {
                        LogIcon::Settings
                    };
                    self.log(LogKind::Link, icon, phrase, text, detail);
                }
                Outcome::Restart => {
                    info!("为安装新版本退出");
                    self.quit_requested = true;
                }
            }
        }
    }

    // ── 定时 ─────────────────────────────────────

    /// 每 250 毫秒一次：结算每秒的统计、清理过期的配对请求、按时重试、推送状态。
    pub(crate) fn on_tick(&mut self) {
        while self.background.try_join_next().is_some() {}
        self.start_preview();
        if !self.platform_info_pending
            && self.platform_info_at.elapsed() >= Duration::from_secs(30)
            && self.workers.len() < 8
        {
            let host = self.host.clone();
            self.platform_info_pending = true;
            self.workers
                .spawn_blocking(move || WorkResult::PlatformInfo {
                    addresses: ferry_net::local_addresses()
                        .into_iter()
                        .map(|ip| ip.to_string())
                        .collect(),
                    natural_scrolling: host.natural_scrolling(),
                });
        }
        let outcomes = self.updates.tick(self.config.auto_update);
        if !outcomes.is_empty() {
            self.apply(outcomes);
            self.dirty = true;
        }
        let now = Instant::now();
        self.dial_requests
            .retain(|_, (_, _, at)| now.duration_since(*at) < Duration::from_secs(60));
        if now.duration_since(self.last_second) >= Duration::from_secs(1) {
            self.last_second = now;
            for link in &mut self.links {
                if link.rtt.roll() {
                    self.dirty = true;
                }
            }
            if !self.links.is_empty() {
                self.activity.connected_for(1);
            }
        }

        // 引擎那边已经不等了（超时、对方拒绝、连接断开）的配对请求。
        let closed: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| p.reply.is_closed())
            .map(|(&id, _)| id)
            .collect();
        for id in closed {
            self.pending.remove(&id);
            self.broadcast(Notice::PairingClosed { id });
        }

        if self.engine.is_none() && self.retry_at.is_some_and(|at| now >= at) {
            self.start_engine();
        }
        if now.duration_since(self.peers_read) >= PEERS_REFRESH {
            self.refresh_peers();
        }
        if self.dirty {
            self.dirty = false;
            if !self.clients.is_empty() {
                let state = Box::new(self.snapshot());
                self.broadcast(Notice::State { state });
            }
        }
    }

    fn refresh_peers(&mut self) {
        self.peers_read = Instant::now();
        match self.store.peers() {
            Ok(peers) if peers != self.peers => {
                self.peers = peers;
                self.dirty = true;
            }
            Ok(_) => {}
            Err(e) => warn!("读取已配对设备失败：{e}"),
        }
    }

    // ── 输出 ─────────────────────────────────────

    /// 记一条日志：`phrase` 给界面按当前语言说，`text`、`detail` 是中文（写进运行日志，旧版界面也显示它）。
    fn log(
        &mut self,
        kind: LogKind,
        icon: LogIcon,
        phrase: Phrase,
        text: impl Into<String>,
        detail: impl Into<String>,
    ) {
        // 设备名、文件名带着标记写进运行日志（导出时换成代号），界面上显示去掉标记的样子。
        let (text, detail) = (text.into(), detail.into());
        info!(target: "screenferry", "{text} {detail}");
        let entry = self
            .log
            .push(kind, icon, mark::strip(&text), mark::strip(&detail), phrase);
        self.broadcast(Notice::Log { entry });
    }

    /// 只发给界面 `id` 的通知。
    fn reply(&mut self, id: u64, notice: Notice) {
        if let Some(outbox) = self.clients.get(&id)
            && outbox.try_send(notice).is_err()
        {
            warn!(id, "设置界面没有及时接收，已断开");
            self.clients.remove(&id);
        }
    }

    /// 给所有界面一句提示：`phrase` 按当前语言说，`text` 是中文。
    fn toast(&mut self, phrase: Phrase, text: impl Into<String>) {
        let text = text.into();
        if matches!(
            phrase.key.as_str(),
            "save_failed"
                | "remove_failed"
                | "peers_read_failed"
                | "autostart_failed"
                | "error"
                | "files_disabled"
                | "not_connected"
                | "paused_connect_later"
                | "not_sharing_invite_later"
        ) {
            self.command_error = Some(text.clone());
        }
        self.broadcast(Notice::Toast {
            text,
            phrase: Some(phrase),
        });
    }

    /// 发给所有界面。某个界面积压太多就断开它，它重连后会重新同步。
    fn broadcast(&mut self, notice: Notice) {
        self.clients
            .retain(|id, outbox| match outbox.try_send(notice.clone()) {
                Ok(()) => true,
                Err(_) => {
                    warn!(id, "设置界面没有及时接收，已断开");
                    false
                }
            });
    }
}

fn resolve_name<H: Host>(config: &Config, host: &H) -> String {
    match config.name.trim() {
        "" => host.device_name(),
        name => name.to_owned(),
    }
}

/// 用户输入的地址：IP、IP:端口、电脑名或电脑名:端口；不写端口时用 `port`。解析不了时为 `None`。
async fn resolve_input(text: &str, port: u16) -> Option<SocketAddr> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let with_port = if text.contains(':') {
        text.to_owned()
    } else {
        format!("{text}:{port}")
    };
    let found = tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host(with_port))
        .await
        .ok()?
        .ok()?
        .collect::<Vec<_>>();
    found
        .iter()
        .find(|a| a.is_ipv4())
        .or(found.first())
        .copied()
}

/// 给用户看的地址：默认端口和不知道的端口（0）都不写。
fn addr_text(addr: SocketAddr) -> String {
    match addr.port() {
        0 | DEFAULT_PORT => addr.ip().to_string(),
        _ => addr.to_string(),
    }
}

fn ipc_dial_error(error: DialError) -> IpcDialError {
    match error {
        DialError::BadAddress => IpcDialError::BadAddress,
        DialError::Refused => IpcDialError::Refused,
        DialError::Unreachable => IpcDialError::Unreachable,
        DialError::NotScreenFerry => IpcDialError::NotScreenFerry,
        DialError::Incompatible => IpcDialError::Incompatible,
        DialError::Busy => IpcDialError::Busy,
    }
}

/// 方向写进 `Phrase` 的参数：`left`、`right`、`top`、`bottom`。
fn side_key(side: Side) -> &'static str {
    match side {
        Side::Left => "left",
        Side::Right => "right",
        Side::Top => "top",
        Side::Bottom => "bottom",
    }
}

fn side_label(side: Side) -> &'static str {
    match side {
        Side::Left => "左侧",
        Side::Right => "右侧",
        Side::Top => "上方",
        Side::Bottom => "下方",
    }
}

/// 文件大小，给人看的写法。
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// 设置界面里第 `index` 个强调色（与 `app/lib/theme/tokens.dart` 中的浅色版一致）。
fn accent_color(index: u8) -> ferry_types::Rgba {
    const ACCENTS: [u32; 5] = [0x0A6CFF, 0x00796B, 0x6A4BD8, 0xC2410C, 0xC2185B];
    let rgb = ACCENTS
        .get(usize::from(index))
        .copied()
        .unwrap_or(ACCENTS[0]);
    ferry_types::Rgba::hex(rgb, 255)
}

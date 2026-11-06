//! 常驻运行：用真实的本机通信当“设置界面”，平台换成 mock。

mod update;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ferry_engine::resident::{self, Command, Host, UpdateSource};
use ferry_engine::{ClientConfig, EdgeFeed, Events, Files, ServerAddr, connect};
use ferry_ipc::{
    Connection, Endpoint, LinkStatus, Notice, PairingPrompt, ProblemKind, Request, SettingsPatch,
    State,
};
use ferry_platform::Error as PlatformError;
use ferry_platform_mock::{
    MockCapture, MockClipboard, MockDisplays, MockInject, MockOverlay, MockUpdater,
};
use ferry_types::{Config, Display, DisplayId, Rect, Role, Side, Theme};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use crate::support::{AutoConfirm, Device};

#[derive(Clone, Default)]
struct MockHost {
    /// 服务端监听的端口；默认 0，由系统分配，实际端口从状态里的 `local.port` 读。
    port: u16,
    deny_permissions: Arc<AtomicBool>,
    autostart: Arc<AtomicBool>,
    settings_opened: Arc<AtomicU32>,
    capture: MockCapture,
    verbose: Arc<AtomicBool>,
    /// 在访达里显示过的文件、打开过的文件夹，按先后。
    shown: Arc<Mutex<Vec<PathBuf>>>,
    updater: MockUpdater,
    /// 更新清单从哪里取；没设时指向一个连不上的地址，测试不碰外网。
    update_source: Option<UpdateSource>,
    diagnostic_gate: Arc<Mutex<Option<std::sync::mpsc::Receiver<()>>>>,
    diagnostic_started: Arc<AtomicBool>,
}

impl Host for MockHost {
    type Capture = MockCapture;
    type Inject = MockInject;
    type Displays = MockDisplays;
    type Clipboard = MockClipboard;
    type Overlay = MockOverlay;
    type Updater = MockUpdater;

    fn device_name(&self) -> String {
        "测试 Mac".into()
    }
    fn check_permissions(&self, _role: Role) -> ferry_platform::Result<()> {
        if self.deny_permissions.load(Ordering::SeqCst) {
            Err(PlatformError::PermissionDenied("测试中拒绝"))
        } else {
            Ok(())
        }
    }
    fn capture(&self) -> ferry_platform::Result<MockCapture> {
        Ok(self.capture.clone())
    }
    fn inject(&self) -> ferry_platform::Result<MockInject> {
        Ok(MockInject::new())
    }
    fn displays(&self) -> MockDisplays {
        MockDisplays::new(vec![Display {
            id: DisplayId(1),
            bounds: Rect::new(0, 0, 1000, 800),
            scale_percent: 100,
            primary: true,
        }])
    }
    fn clipboard(&self) -> MockClipboard {
        MockClipboard::new()
    }

    fn overlay(&self) -> MockOverlay {
        MockOverlay::new()
    }
    fn file_committer(&self) -> Arc<dyn ferry_platform::FileCommit> {
        Arc::new(ferry_platform_mock::MockFileCommit)
    }
    fn updater(&self) -> MockUpdater {
        self.updater.clone()
    }
    fn update_source(&self) -> UpdateSource {
        self.update_source.clone().unwrap_or(UpdateSource {
            manifest_url: "http://127.0.0.1:9/update.json".into(),
            keys: Vec::new(),
        })
    }
    fn autostart(&self) -> bool {
        self.autostart.load(Ordering::SeqCst)
    }
    fn set_autostart(&self, on: bool) -> ferry_platform::Result<()> {
        self.autostart.store(on, Ordering::SeqCst);
        Ok(())
    }
    fn open_settings(&self) {
        self.settings_opened.fetch_add(1, Ordering::SeqCst);
    }
    fn open_permission_settings(&self) {}
    fn listen_port(&self) -> u16 {
        self.port
    }
    fn invite_port(&self) -> u16 {
        0
    }
    fn system(&self) -> String {
        "测试系统 1.0".into()
    }
    fn utc_offset(&self) -> i32 {
        if let Some(wait) = self.diagnostic_gate.lock().unwrap().take() {
            self.diagnostic_started.store(true, Ordering::SeqCst);
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
        0
    }
    fn set_verbose(&self, on: bool) {
        self.verbose.store(on, Ordering::SeqCst);
    }
    fn reveal(&self, path: &Path) {
        self.shown.lock().unwrap().push(path.to_owned());
    }
    fn open_folder(&self, path: &Path) {
        self.shown.lock().unwrap().push(path.to_owned());
    }
}

/// 一份常驻后台，加上连着它的“设置界面”。
struct Daemon {
    device: Device,
    host: MockHost,
    handle: resident::Handle,
    task: JoinHandle<ferry_engine::Result<()>>,
}

impl Daemon {
    async fn start(host: MockHost) -> Self {
        Self::start_with(Device::new(), host).await
    }

    /// 在已经准备好的数据目录上启动。
    async fn start_with(device: Device, host: MockHost) -> Self {
        device
            .store
            .save_config(&Config {
                role: Role::Server,
                announce: false,
                ..Config::default()
            })
            .unwrap();
        let (control, handle) = resident::control();
        let task = tokio::spawn(resident::run(device.store.clone(), host.clone(), control));
        let daemon = Self {
            device,
            host,
            handle,
            task,
        };
        daemon.endpoint().await;
        daemon
    }

    async fn endpoint(&self) -> Endpoint {
        let path = self.device.store.ipc_endpoint_path();
        for _ in 0..300 {
            if let Ok(Some(endpoint)) = Endpoint::load(&path) {
                return endpoint;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("后台没有写下 ipc.json");
    }

    async fn ui(&self) -> Ui {
        let conn = ferry_ipc::connect(&self.endpoint().await).await.unwrap();
        Ui { conn, state: None }
    }
}

/// 测试里的设置界面：记住最新的状态。
struct Ui {
    conn: Connection,
    state: Option<State>,
}

impl Ui {
    async fn send(&mut self, request: Request) {
        self.conn.send(&request).await.unwrap();
    }

    /// 读通知，直到 `check` 返回 `Some`。3 秒内等不到就失败。
    async fn wait<T>(&mut self, what: &str, mut check: impl FnMut(&Notice) -> Option<T>) -> T {
        let read = async {
            loop {
                let notice: Notice = self.conn.recv().await.unwrap().expect("后台断开了");
                if let Notice::State { state } = &notice {
                    self.state = Some((**state).clone());
                }
                if let Some(found) = check(&notice) {
                    return found;
                }
            }
        };
        timeout(Duration::from_secs(5), read)
            .await
            .unwrap_or_else(|_| panic!("等待超时：{what}"))
    }

    async fn state_where(&mut self, what: &str, pred: impl Fn(&State) -> bool) -> State {
        if let Some(state) = self.state.as_ref().filter(|s| pred(s)) {
            return state.clone();
        }
        self.wait(what, |n| match n {
            Notice::State { state } if pred(state) => Some((**state).clone()),
            _ => None,
        })
        .await
    }
}

#[tokio::test]
async fn the_ui_sees_the_state_and_can_pause_sharing() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;

    let state = ui
        .state_where("开始等待连接", |s| {
            s.link.status == LinkStatus::Waiting
        })
        .await;
    assert_eq!(state.local.name, "测试 Mac");
    assert_eq!(state.local.role, Role::Server);
    assert!(state.sharing);
    assert_eq!(state.local.fingerprint.len(), 64);

    ui.send(Request::SetSharing { on: false }).await;
    ui.wait("暂停的提示", |n| match n {
        Notice::Toast { text, phrase } if text == "已暂停共享" => {
            assert_eq!(
                phrase.as_ref().map(|p| p.key.as_str()),
                Some("sharing_paused")
            );
            Some(())
        }
        _ => None,
    })
    .await;
    ui.state_where("已暂停", |s| {
        !s.sharing && s.link.status == LinkStatus::Paused
    })
    .await;

    ui.send(Request::SetSharing { on: true }).await;
    ui.state_where("继续等待连接", |s| {
        s.sharing && s.link.status == LinkStatus::Waiting
    })
    .await;
    daemon.task.abort();
}

#[tokio::test]
async fn settings_are_saved_and_reported_back() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            side: Some(Side::Left),
            theme: Some(Theme::Dark),
            accent: Some(3),
            autostart: Some(true),
            ..SettingsPatch::default()
        },
    })
    .await;
    let state = ui
        .state_where("设置已生效", |s| s.settings.side == Side::Left)
        .await;
    assert_eq!(state.settings.theme, Theme::Dark);
    assert_eq!(state.settings.accent, 3);
    assert!(state.settings.autostart, "开机自启以系统中的登记为准");

    let saved = daemon.device.store.config().unwrap();
    assert_eq!(saved.side, Side::Left);
    assert_eq!(saved.theme, Theme::Dark);
    daemon.task.abort();
}

#[tokio::test]
async fn device_settings_are_saved_and_reported_back() {
    use ferry_types::DeviceSettings;

    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let fingerprint = "AB".repeat(32);
    let mut device = DeviceSettings::new(fingerprint.clone());
    device.alias = "书房的电脑".into();
    device.reverse_vertical = true;
    device.pointer_dpi = 50_000;
    ui.send(Request::SetDevice { device }).await;
    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            mouse_dpi: Some(800),
            ..SettingsPatch::default()
        },
    })
    .await;
    let state = ui
        .state_where("设备设置已生效", |s| {
            !s.settings.devices.is_empty() && s.settings.mouse_dpi == 800
        })
        .await;
    let saved = &state.settings.devices[0];
    assert_eq!(
        saved.fingerprint,
        fingerprint.to_lowercase(),
        "统一写成小写"
    );
    assert_eq!(saved.alias, "书房的电脑");
    assert!(saved.reverse_vertical);
    assert_eq!(
        saved.pointer_dpi,
        *DeviceSettings::POINTER_DPI.end(),
        "超出范围的收进范围内"
    );

    let config = daemon.device.store.config().unwrap();
    assert_eq!(config.devices, state.settings.devices);
    assert_eq!(config.mouse_dpi, 800);

    // 恢复默认：这一条从配置里去掉。
    ui.send(Request::SetDevice {
        device: DeviceSettings::new(fingerprint),
    })
    .await;
    ui.state_where("恢复默认", |s| s.settings.devices.is_empty())
        .await;
    assert!(daemon.device.store.config().unwrap().devices.is_empty());
    daemon.task.abort();
}

#[tokio::test]
async fn first_pairing_is_confirmed_in_the_ui() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let state = ui
        .state_where("开始等待连接", |s| {
            s.link.status == LinkStatus::Waiting
        })
        .await;
    let port = state.local.port.expect("等待连接时报告监听的端口");

    // 一台全新的客户端连过来，需要配对。
    let pc = Device::new();
    let pc_confirm = AutoConfirm::answering(true);
    let client = tokio::spawn(connect(
        ClientConfig {
            server: ServerAddr::Fixed(([127, 0, 0, 1], port).into()),
            name: "pc".into(),
            clipboard: true,

            clipboard_limit: Default::default(),
            command_key: false,
            store: pc.store.clone(),
            events: Events::none(),
            files: Files::new(
                &pc.store,
                Events::none(),
                Arc::new(ferry_platform_mock::MockFileCommit),
            ),
            edge: EdgeFeed::none(),
            devices: Vec::new(),
            commands: None,
            invite_port: None,
        },
        MockInject::new(),
        MockDisplays::new(vec![Display {
            id: DisplayId(1),
            bounds: Rect::new(0, 0, 1920, 1080),
            scale_percent: 150,
            primary: true,
        }]),
        MockClipboard::new(),
        pc_confirm.clone(),
    ));

    let prompt: PairingPrompt = ui
        .wait("配对请求", |n| match n {
            Notice::Pairing { prompt } => Some(prompt.clone()),
            _ => None,
        })
        .await;
    assert_eq!(prompt.peer_name, "pc");
    assert_eq!(prompt.code.len(), 7, "形如 123 456：{}", prompt.code);
    ui.send(Request::Execute {
        session: prompt.session.clone(),
        id: 1,
        request: Box::new(Request::PairReply {
            id: prompt.id,
            accept: true,
        }),
    })
    .await;
    ui.wait("配对对话框关闭", |n| match n {
        Notice::PairingClosed { id } if *id == prompt.id => Some(()),
        _ => None,
    })
    .await;

    let state = ui
        .state_where("连上了", |s| s.link.status == LinkStatus::Connected)
        .await;
    let peer = state.link.peer.expect("连上时有对方");
    assert_eq!(peer.name, "pc");
    assert_eq!(peer.addr, "127.0.0.1");
    // 两边显示同一个配对码。
    assert_eq!(pc_confirm.requests()[0].code.to_string(), prompt.code);
    let state = ui
        .state_where(
            "已配对列表里有它且在线，并且知道它的版本",
            |s| {
                s.peers
                    .iter()
                    .any(|p| p.name == "pc" && p.online && p.version == env!("CARGO_PKG_VERSION"))
            },
        )
        .await;

    // 从界面移除它：断开，并从信任列表删掉。
    let fingerprint = state.peers[0].fingerprint.clone();
    ui.send(Request::Forget { fingerprint }).await;
    ui.state_where("移除后列表为空", |s| s.peers.is_empty())
        .await;
    assert!(daemon.device.store.peers().unwrap().is_empty());

    client.abort();
    daemon.task.abort();
}

#[tokio::test]
async fn missing_permission_is_reported_and_retried() {
    let host = MockHost {
        deny_permissions: Arc::new(AtomicBool::new(true)),
        ..MockHost::default()
    };
    let daemon = Daemon::start(host.clone()).await;
    let mut ui = daemon.ui().await;
    let state = ui
        .state_where("报告缺少权限", |s| s.problem.is_some())
        .await;
    assert_eq!(state.problem.unwrap().kind, ProblemKind::Permission);
    assert_eq!(state.link.status, LinkStatus::Stopped);

    // 用户去系统设置里授权之后，几秒内自动恢复。
    host.deny_permissions.store(false, Ordering::SeqCst);
    ui.state_where("授权后开始等待连接", |s| {
        s.problem.is_none() && s.link.status == LinkStatus::Waiting
    })
    .await;
    daemon.task.abort();
}

#[tokio::test]
async fn a_second_launch_hands_over_to_the_running_one() {
    let daemon = Daemon::start(MockHost::default()).await;
    assert!(resident::hand_over_to_running(&daemon.device.store).await);
    let opened = daemon.host.settings_opened.clone();
    crate::support::eventually("请运行中的那份打开设置界面", || {
        opened.load(Ordering::SeqCst) == 1
    })
    .await;

    // 没有在运行的后台时，什么也不做。
    let other = Device::new();
    assert!(!resident::hand_over_to_running(&other.store).await);
    daemon.task.abort();
}

#[tokio::test]
async fn quitting_removes_the_endpoint_file() {
    let daemon = Daemon::start(MockHost::default()).await;
    let path = daemon.device.store.ipc_endpoint_path();
    assert!(path.exists());
    daemon.handle.send(Command::Quit);
    let result = timeout(Duration::from_secs(3), daemon.task).await.unwrap();
    assert!(result.unwrap().is_ok());
    assert!(!path.exists(), "退出后界面应当知道后台已经不在");
}

#[tokio::test]
async fn quitting_tells_the_ui_to_close_too() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    daemon.handle.send(Command::Quit);
    ui.wait("后台要退出了", |n| (*n == Notice::Quit).then_some(()))
        .await;
    let result = timeout(Duration::from_secs(3), daemon.task).await.unwrap();
    assert!(result.unwrap().is_ok());
}

#[tokio::test]
async fn the_quit_command_stops_the_running_one_and_waits() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    let path = daemon.device.store.ipc_endpoint_path();

    assert_eq!(resident::quit_running(&daemon.device.store).await, Ok(true));
    assert!(!path.exists(), "返回时后台已经退出");
    ui.wait("界面也收到了", |n| (*n == Notice::Quit).then_some(()))
        .await;
    let result = timeout(Duration::from_secs(3), daemon.task).await.unwrap();
    assert!(result.unwrap().is_ok());

    // 没有在运行的后台时，什么也不做。
    assert_eq!(
        resident::quit_running(&daemon.device.store).await,
        Ok(false)
    );
}

#[tokio::test]
async fn pairing_with_no_ui_open_opens_the_settings() {
    let host = MockHost::default();
    let daemon = Daemon::start(host.clone()).await;
    // 先用界面看一眼监听的端口，再关掉界面。
    let port = {
        let mut ui = daemon.ui().await;
        let state = ui
            .state_where("开始等待连接", |s| {
                s.link.status == LinkStatus::Waiting
            })
            .await;
        state.local.port.expect("等待连接时报告监听的端口")
    };
    // 等后台处理完界面断开，否则它会以为还有界面在看。
    sleep(Duration::from_millis(200)).await;
    let pc = Device::new();
    let client = tokio::spawn(connect(
        ClientConfig {
            server: ServerAddr::Fixed(([127, 0, 0, 1], port).into()),
            name: "pc".into(),
            clipboard: false,

            clipboard_limit: Default::default(),
            command_key: false,
            store: pc.store.clone(),
            events: Events::none(),
            files: Files::new(
                &pc.store,
                Events::none(),
                Arc::new(ferry_platform_mock::MockFileCommit),
            ),
            edge: EdgeFeed::none(),
            devices: Vec::new(),
            commands: None,
            invite_port: None,
        },
        MockInject::new(),
        MockDisplays::default(),
        MockClipboard::new(),
        AutoConfirm::answering(true),
    ));
    let opened = host.settings_opened.clone();
    crate::support::eventually("没有界面时打开设置界面", || {
        opened.load(Ordering::SeqCst) >= 1
    })
    .await;

    // 稍后打开的界面会收到仍在等待的配对请求。
    let mut ui = daemon.ui().await;
    ui.wait("补发配对请求", |n| match n {
        Notice::Pairing { .. } => Some(()),
        _ => None,
    })
    .await;
    client.abort();
    daemon.task.abort();
}

#[tokio::test]
async fn the_listening_port_is_reported_and_cleared_when_paused() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let state = ui
        .state_where("开始等待连接", |s| {
            s.link.status == LinkStatus::Waiting
        })
        .await;
    assert!(
        state.local.port.is_some_and(|p| p != 0),
        "报告系统分配的实际端口"
    );

    ui.send(Request::SetSharing { on: false }).await;
    ui.state_where("暂停后不再监听", |s| s.local.port.is_none())
        .await;
    daemon.task.abort();
}

#[tokio::test]
async fn switching_the_role_restarts_as_the_other_side() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("先以服务端等待连接", |s| {
        s.link.status == LinkStatus::Waiting && s.local.port.is_some()
    })
    .await;

    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            role: Some(Role::Client),
            ..SettingsPatch::default()
        },
    })
    .await;
    let state = ui
        .state_where("改为客户端，不再监听", |s| {
            s.local.role == Role::Client && s.local.port.is_none()
        })
        .await;
    assert_ne!(state.link.status, LinkStatus::Waiting, "不再等别人来连");
    assert_eq!(state.local.platform, std::env::consts::OS);
    assert_eq!(daemon.device.store.config().unwrap().role, Role::Client);

    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            role: Some(Role::Server),
            ..SettingsPatch::default()
        },
    })
    .await;
    ui.state_where("改回服务端后重新监听", |s| {
        s.local.role == Role::Server && s.local.port.is_some()
    })
    .await;
    daemon.task.abort();
}

// ── 运行日志与诊断文件 ─────────────────────────

/// 往运行日志里写几行：现在的格式，带标记的设备名、文件名和用户目录。
fn write_log(daemon: &Daemon, lines: &[String]) {
    let path = daemon.device.store.log_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    for line in lines {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&path, text).unwrap();
}

fn run_log(n: &Notice) -> Option<(Vec<ferry_ipc::RunLogLine>, u64, bool, u64)> {
    match n {
        Notice::RunLog {
            lines,
            next,
            fresh,
            lines_total,
            ..
        } => Some((lines.clone(), *next, *fresh, *lines_total)),
        _ => None,
    }
}

#[tokio::test]
async fn the_run_log_is_read_from_the_end_then_only_what_is_new() {
    use ferry_ipc::RunLogLevel;
    use ferry_types::mark;

    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    write_log(
        &daemon,
        &[
            "   0.096268792s  INFO 以前的格式，没有时间".to_owned(),
            format!(
                "2026-11-02 06:12:04.437  INFO 已连接 {} TLS 1.3 · 192.168.2.18",
                mark::device("书房的电脑")
            ),
            "2026-11-02 06:20:02.775  WARN 连接中断：网络错误".to_owned(),
        ],
    );

    ui.send(Request::ReadRunLog {
        since: None,
        cursor: None,
        request_id: None,
    })
    .await;
    let (lines, next, fresh, _) = ui.wait("运行日志", run_log).await;
    assert!(fresh);
    let ours: Vec<_> = lines.iter().rev().take(3).rev().collect();
    assert_eq!(ours[0].at, "");
    assert_eq!(ours[0].text, "以前的格式，没有时间");
    assert_eq!(ours[1].at, "06:12:04");
    assert_eq!(ours[1].level, RunLogLevel::Info);
    assert_eq!(
        ours[1].text, "已连接 书房的电脑 TLS 1.3 · 192.168.2.18",
        "界面上不带标记"
    );
    assert_eq!(ours[2].level, RunLogLevel::Warn);

    write_log(
        &daemon,
        &["2026-11-02 06:21:00.001 ERROR 心跳超时".to_owned()],
    );
    ui.send(Request::ReadRunLog {
        since: Some(next),
        cursor: None,
        request_id: None,
    })
    .await;
    let (lines, _, fresh, _) = ui.wait("新写的运行日志", run_log).await;
    assert!(!fresh, "接在后面");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].level, RunLogLevel::Error);
    assert_eq!(lines[0].text, "心跳超时");
    daemon.task.abort();
}

#[tokio::test]
async fn diagnostics_replace_names_files_and_the_home_folder_with_codes() {
    use ferry_types::mark;

    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let state = ui.state_where("首次状态", |_| true).await;
    let local = state.local.name.clone();
    let home = ferry_store::home_dir().unwrap();
    let received = home.join("Downloads").join("证书.p12");
    write_log(
        &daemon,
        &[
            format!(
                "2026-11-02 06:12:03.104  INFO ScreenFerry 已启动 {}",
                mark::device(&local)
            ),
            format!(
                "2026-11-02 06:18:31.240  INFO 文件收好了 at={}",
                mark::path(&received)
            ),
            format!(
                "2026-11-02 06:19:00.000  WARN 读不到 {}",
                home.join("Desktop").display()
            ),
            format!("   0.096268792s  INFO 以前的版本写的 发送 私人.pdf 到 {local}"),
        ],
    );

    ui.send(Request::ExportDiagnostics).await;
    let path = ui
        .wait("导出诊断文件", |n| match n {
            Notice::DiagnosticsExported { path, .. } => Some(PathBuf::from(path)),
            _ => None,
        })
        .await;
    assert!(path.starts_with(daemon.device.store.downloads_dir()));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("ScreenFerry 诊断文件"));
    assert!(text.contains("测试系统 1.0"));
    assert!(text.contains("1 台：1000×800 @100%（主）"));
    assert!(text.contains("ScreenFerry 已启动 本机"));
    assert!(text.contains("文件 1.p12"));
    let home_text = home.display().to_string();
    assert!(text.contains("以前的版本写的 1 行没有带上"));
    for secret in [local.as_str(), "证书", "私人", home_text.as_str()] {
        assert!(!text.contains(secret), "诊断文件里不应出现 {secret}");
    }

    // 摘要只带最后 200 行。
    ui.send(Request::CopyDiagnostics).await;
    let summary = ui
        .wait("诊断摘要", |n| match n {
            Notice::DiagnosticsSummary { text } => Some(text.clone()),
            _ => None,
        })
        .await;
    assert!(summary.starts_with("ScreenFerry 诊断摘要"));
    assert!(summary.contains("文件 1.p12"));

    ui.send(Request::RevealDiagnostics).await;
    ui.send(Request::OpenLogsFolder).await;
    let shown = daemon.host.shown.clone();
    crate::support::eventually("在访达里显示", || shown.lock().unwrap().len() == 2).await;
    assert_eq!(shown.lock().unwrap()[0], path);
    assert_eq!(shown.lock().unwrap()[1], daemon.device.store.logs_dir());
    daemon.task.abort();
}

#[tokio::test]
async fn verbose_logging_can_be_switched_on_and_off() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |s| s.local.verbose_since.is_none())
        .await;
    assert_eq!(ui.state.as_ref().unwrap().local.system, "测试系统 1.0");

    ui.send(Request::SetVerbose { on: true }).await;
    ui.state_where("详细记录打开", |s| s.local.verbose_since.is_some())
        .await;
    assert!(daemon.host.verbose.load(Ordering::SeqCst));

    ui.send(Request::SetVerbose { on: false }).await;
    ui.state_where("详细记录关闭", |s| s.local.verbose_since.is_none())
        .await;
    assert!(!daemon.host.verbose.load(Ordering::SeqCst));
    daemon.task.abort();
}

#[tokio::test]
async fn the_language_is_saved_and_log_entries_say_what_happened() {
    use ferry_types::Language;

    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let entries = ui
        .wait("已有的日志", |n| match n {
            Notice::History { entries } => Some(entries.clone()),
            _ => None,
        })
        .await;
    let started = entries
        .iter()
        .find_map(|e| e.phrase.as_ref().filter(|p| p.key == "started"))
        .expect("启动那条日志带着 phrase");
    assert_eq!(started.args["role"], "server");
    assert!(!started.args["name"].is_empty());

    let state = ui.state_where("首次状态", |_| true).await;
    assert_eq!(state.settings.language, Language::System);
    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            language: Some(Language::En),
            ..SettingsPatch::default()
        },
    })
    .await;
    ui.state_where("改成英文", |s| s.settings.language == Language::En)
        .await;
    assert_eq!(
        daemon.device.store.config().unwrap().language,
        Language::En,
        "存进 config.toml"
    );
    daemon.task.abort();
}

#[tokio::test]
async fn stale_session_commands_cannot_change_the_new_daemon() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let before = ui.state_where("启动", |s| s.sharing).await;
    ui.send(Request::Execute {
        session: format!("{}-stale", before.session),
        id: 42,
        request: Box::new(Request::SetSharing { on: false }),
    })
    .await;
    ui.wait("拒绝旧会话", |n| {
        matches!(
            n,
            Notice::CommandResult {
                id: 42,
                ok: false,
                ..
            }
        )
        .then_some(())
    })
    .await;
    assert!(ui.state.as_ref().unwrap().sharing);
    daemon.task.abort();
}

#[tokio::test]
async fn failed_config_save_returns_error_after_the_unchanged_state() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let before = ui.state_where("启动", |s| s.sharing).await;
    let path = daemon.device.store.root().join("config.toml");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    ui.send(Request::Execute {
        session: before.session.clone(),
        id: 43,
        request: Box::new(Request::UpdateSettings {
            settings: SettingsPatch {
                name: Some("未保存".into()),
                ..SettingsPatch::default()
            },
        }),
    })
    .await;
    ui.wait("保存失败回执", |n| {
        matches!(
            n,
            Notice::CommandResult {
                id: 43,
                ok: false,
                ..
            }
        )
        .then_some(())
    })
    .await;
    assert_eq!(
        ui.state.as_ref().unwrap().settings.name,
        before.settings.name
    );
    daemon.task.abort();
}

#[tokio::test]
async fn two_concurrent_daemons_publish_only_one_endpoint() {
    let device = Device::new();
    device
        .store
        .save_config(&Config {
            role: Role::Server,
            announce: false,
            auto_update: false,
            ..Config::default()
        })
        .unwrap();
    let host = MockHost::default();
    let (a_control, a_handle) = resident::control();
    let (b_control, b_handle) = resident::control();
    let a = tokio::spawn(resident::run(device.store.clone(), host.clone(), a_control));
    let b = tokio::spawn(resident::run(device.store.clone(), host.clone(), b_control));
    crate::support::eventually("另一份已转交", || a.is_finished() || b.is_finished()).await;
    assert_ne!(a.is_finished(), b.is_finished());
    let endpoint = Endpoint::load(&device.store.ipc_endpoint_path())
        .unwrap()
        .unwrap();
    assert!(ferry_ipc::connect(&endpoint).await.is_ok());
    a_handle.send(Command::Quit);
    b_handle.send(Command::Quit);
    timeout(Duration::from_secs(5), async {
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn invitation_results_stay_with_the_ui_even_when_request_ids_match() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut first = daemon.ui().await;
    let mut second = daemon.ui().await;
    let session = first
        .state_where("第一界面", |s| s.local.port.is_some())
        .await
        .session;
    second
        .state_where("第二界面", |s| s.local.port.is_some())
        .await;
    // 空白在解析入口立即拒绝；不同原文仍能辨认是哪一个请求。
    for (ui, address) in [(&mut first, " "), (&mut second, "\t")] {
        ui.send(Request::Execute {
            session: session.clone(),
            id: 7,
            request: Box::new(Request::Invite {
                address: address.into(),
            }),
        })
        .await;
    }
    for (ui, expected) in [(&mut first, " "), (&mut second, "\t")] {
        let (request_id, address, error) = ui
            .wait("只收到自己的邀请结果", |n| match n {
                Notice::Dialed {
                    request_id,
                    address,
                    error,
                    ..
                } => Some((*request_id, address.clone(), *error)),
                _ => None,
            })
            .await;
        assert_eq!(request_id, Some(7));
        assert_eq!(address, expected);
        assert_eq!(error, Some(ferry_ipc::DialError::BadAddress));
    }
    daemon.task.abort();
}

#[tokio::test]
async fn a_slow_diagnostic_does_not_block_pause_commands() {
    let host = MockHost::default();
    let (release, wait) = std::sync::mpsc::channel();
    *host.diagnostic_gate.lock().unwrap() = Some(wait);
    let daemon = Daemon::start(host.clone()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("启动", |s| s.sharing).await;
    ui.send(Request::CopyDiagnostics).await;
    crate::support::eventually("诊断线程开始", || {
        host.diagnostic_started.load(Ordering::SeqCst)
    })
    .await;
    ui.send(Request::SetSharing { on: false }).await;
    timeout(
        Duration::from_secs(1),
        ui.state_where("诊断仍在等待时可以暂停", |s| !s.sharing),
    )
    .await
    .unwrap();
    release.send(()).unwrap();
    ui.wait("诊断完成", |n| {
        matches!(n, Notice::DiagnosticsSummary { .. }).then_some(())
    })
    .await;
    daemon.task.abort();
}

#[tokio::test]
async fn opaque_log_cursor_detects_replacement_and_preserves_partial_lines() {
    use std::io::Write;
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    write_log(&daemon, &["2026-11-02 06:12:04.437 INFO first".into()]);
    ui.send(Request::ReadRunLog {
        since: None,
        cursor: None,
        request_id: Some(1),
    })
    .await;
    let cursor = ui
        .wait("游标", |n| match n {
            Notice::RunLog {
                cursor,
                request_id: Some(1),
                error: None,
                ..
            } => Some(cursor.clone()),
            _ => None,
        })
        .await;
    let path = daemon.device.store.log_file();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"2026-11-02 06:12:05.437 INFO partial")
        .unwrap();
    ui.send(Request::ReadRunLog {
        since: None,
        cursor: Some(cursor.clone()),
        request_id: Some(2),
    })
    .await;
    let same = ui
        .wait("末行未完成", |n| match n {
            Notice::RunLog {
                cursor,
                request_id: Some(2),
                lines,
                fresh,
                error: None,
                ..
            } => {
                assert!(lines.is_empty());
                assert!(!fresh);
                Some(cursor.clone())
            }
            _ => None,
        })
        .await;
    assert_eq!(same, cursor);
    file.write_all(b" complete\n").unwrap();
    ui.send(Request::ReadRunLog {
        since: None,
        cursor: Some(cursor),
        request_id: Some(3),
    })
    .await;
    let cursor = ui
        .wait("完整行仅一次", |n| match n {
            Notice::RunLog {
                cursor,
                request_id: Some(3),
                lines,
                error: None,
                ..
            } => {
                assert_eq!(lines.len(), 1);
                assert_eq!(lines[0].text, "partial complete");
                Some(cursor.clone())
            }
            _ => None,
        })
        .await;
    drop(file);
    std::fs::rename(&path, path.with_extension("previous")).unwrap();
    std::fs::write(
        &path,
        format!(
            "2026-11-02 06:12:06.437 INFO replacement {}\n",
            "x".repeat(300)
        ),
    )
    .unwrap();
    ui.send(Request::ReadRunLog {
        since: None,
        cursor: Some(cursor),
        request_id: Some(4),
    })
    .await;
    ui.wait("轮转到更大的文件也重置", |n| match n {
        Notice::RunLog {
            request_id: Some(4),
            lines,
            fresh,
            error: None,
            ..
        } => {
            assert!(*fresh);
            assert_eq!(lines.len(), 1);
            assert!(lines[0].text.starts_with("replacement"));
            Some(())
        }
        _ => None,
    })
    .await;
    daemon.task.abort();
}

#[tokio::test]
async fn performance_controls_persist_and_are_returned_in_authoritative_state() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            file_rate_limit_mbps: Some(12),
            coalesce_motion: Some(true),
            ..SettingsPatch::default()
        },
    })
    .await;
    ui.state_where("性能配置已应用", |s| {
        s.settings.file_rate_limit_mbps == 12 && s.settings.coalesce_motion
    })
    .await;
    let stored = daemon.device.store.config().unwrap();
    assert_eq!(stored.file_rate_limit_mbps, 12);
    assert!(stored.coalesce_motion);
    daemon.task.abort();
}

#[tokio::test]
async fn incremental_log_pages_are_bounded_and_never_skip_complete_lines() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    write_log(&daemon, &["2026-11-02 06:12:04.437 INFO start".into()]);
    ui.send(Request::ReadRunLog {
        since: None,
        cursor: None,
        request_id: Some(80),
    })
    .await;
    let cursor = ui
        .wait("初始游标", |n| match n {
            Notice::RunLog {
                cursor,
                request_id: Some(80),
                error: None,
                ..
            } => Some(cursor.clone()),
            _ => None,
        })
        .await;
    let data: Vec<String> = (0..2005)
        .map(|n| format!("2026-11-02 06:12:05.437 INFO row{n}"))
        .collect();
    write_log(&daemon, &data);
    let mut cursor = Some(cursor);
    let mut received = Vec::new();
    for id in 81..86 {
        ui.send(Request::ReadRunLog {
            since: None,
            cursor,
            request_id: Some(id),
        })
        .await;
        let (token, more, lines) = ui
            .wait("有界分页", |n| match n {
                Notice::RunLog {
                    cursor,
                    request_id: Some(current),
                    lines,
                    more,
                    fresh,
                    error: None,
                    ..
                } if *current == id => {
                    assert!(!fresh);
                    assert!(lines.len() <= 1000);
                    assert!(serde_json::to_vec(n).unwrap().len() < ferry_ipc::MAX_LINE);
                    Some((cursor.clone(), *more, lines.clone()))
                }
                _ => None,
            })
            .await;
        cursor = Some(token);
        received.extend(lines.into_iter().map(|l| l.text));
        if !more {
            break;
        }
    }
    assert_eq!(
        received,
        (0..2005).map(|n| format!("row{n}")).collect::<Vec<_>>()
    );
    daemon.task.abort();
}

#[tokio::test]
async fn diagnostic_exports_keep_background_log_loss_markers() {
    use std::io::Write;
    struct RecoveringFile {
        file: std::fs::File,
        fail: bool,
    }
    impl Write for RecoveringFile {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.fail) {
                return Err(std::io::Error::other("disk full"));
            }
            self.file.write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.file.flush()
        }
    }
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    std::fs::create_dir_all(daemon.device.store.logs_dir()).unwrap();
    let file = std::fs::File::create(daemon.device.store.log_file()).unwrap();
    let (writer, guard) =
        ferry_store::LogWriter::new(RecoveringFile { file, fail: true }, 4).unwrap();
    writer.record(b"2026-11-02 06:12:03.104 INFO not written\n");
    writer.record(b"2026-11-02 06:12:04.104 INFO recovered\n");
    tokio::task::spawn_blocking(move || drop(guard))
        .await
        .unwrap();
    ui.send(Request::CopyDiagnostics).await;
    ui.wait("摘要保留丢失提示", |notice| match notice {
        Notice::DiagnosticsSummary { text } => {
            assert!(text.contains("已丢弃 1 条日志"));
            Some(())
        }
        _ => None,
    })
    .await;
    ui.send(Request::ExportDiagnostics).await;
    let path = ui
        .wait("完整导出", |notice| match notice {
            Notice::DiagnosticsExported { path, .. } => Some(path.clone()),
            _ => None,
        })
        .await;
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("已丢弃 1 条日志")
    );
    daemon.task.abort();
}

#[tokio::test]
async fn large_diagnostic_summaries_fit_in_one_ipc_message() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    let lines = (0..40)
        .map(|n| {
            format!(
                "2026-11-02 06:12:03.104 INFO row{n} {}",
                "\u{0001}".repeat(60 * 1024)
            )
        })
        .collect::<Vec<_>>();
    write_log(&daemon, &lines);
    ui.send(Request::CopyDiagnostics).await;
    ui.wait("摘要保持有界", |notice| match notice {
        Notice::DiagnosticsSummary { text } => {
            assert!(text.contains("row39"), "优先保留最近日志");
            assert!(text.contains("诊断摘要已截短"));
            assert!(serde_json::to_vec(notice).unwrap().len() < ferry_ipc::MAX_LINE);
            Some(())
        }
        _ => None,
    })
    .await;
    daemon.task.abort();
}

#[tokio::test]
async fn peer_performance_is_removed_on_disconnect_and_restarts_with_a_new_window() {
    use ferry_trust::Peer;
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let initial = ui.state_where("后台监听", |s| s.local.port.is_some()).await;
    let pc = Device::new();
    let server_id = daemon
        .device
        .store
        .identity("server")
        .unwrap()
        .fingerprint();
    let peer_id = pc.store.identity("metrics peer").unwrap().fingerprint();
    let fingerprint = peer_id.to_hex();
    let mut trusted = daemon.device.store.peers().unwrap();
    trusted.add(Peer::new("metrics peer", peer_id));
    daemon.device.store.save_peers(&trusted).unwrap();
    let mut trusted = pc.store.peers().unwrap();
    trusted.add(Peer::new("server", server_id));
    pc.store.save_peers(&trusted).unwrap();
    let spawn = || {
        tokio::spawn(connect(
            ClientConfig {
                server: ServerAddr::Fixed(([127, 0, 0, 1], initial.local.port.unwrap()).into()),
                name: "metrics peer".into(),
                clipboard: false,
                clipboard_limit: Default::default(),
                command_key: false,
                store: pc.store.clone(),
                events: Events::none(),
                files: Files::new(
                    &pc.store,
                    Events::none(),
                    Arc::new(ferry_platform_mock::MockFileCommit),
                ),
                edge: EdgeFeed::none(),
                devices: Vec::new(),
                commands: None,
                invite_port: None,
            },
            MockInject::new(),
            MockDisplays::new(vec![crate::support::display(1920, 1080, 100)]),
            MockClipboard::new(),
            AutoConfirm::answering(true),
        ))
    };
    let client = spawn();
    ui.state_where("连接建立", |s| {
        s.peers
            .iter()
            .any(|p| p.fingerprint == fingerprint && p.online)
    })
    .await;
    // 直接等待真实采样，避免虚拟tokio时间与引擎std::Instant窗口混用。
    let first = timeout(Duration::from_secs(8), async {
        loop {
            if let Some(Notice::State { state }) = ui.conn.recv::<Notice>().await.unwrap()
                && let Some(metrics) = state
                    .performance
                    .peers
                    .iter()
                    .find(|p| p.fingerprint.as_deref() == Some(&fingerprint))
                && metrics.metrics.rtt.count > 0
            {
                return metrics.clone();
            }
        }
    })
    .await
    .expect("真实心跳必须产生非空的性能窗口");
    assert!(first.metrics.rtt.count > 0);
    assert!(first.metrics.window_ms > 0);
    client.abort();
    let _ = client.await;
    ui.wait("断开快照移除旧指标", |notice| match notice {
        Notice::State { state }
            if state
                .peers
                .iter()
                .any(|p| p.fingerprint == fingerprint && !p.online) =>
        {
            assert!(
                !state
                    .performance
                    .peers
                    .iter()
                    .any(|p| p.fingerprint.as_deref() == Some(&fingerprint))
            );
            Some(())
        }
        _ => None,
    })
    .await;
    let client = spawn();
    let reconnected = ui
        .wait("重连首个快照没有旧窗口", |notice| match notice {
            Notice::State { state }
                if state
                    .peers
                    .iter()
                    .any(|p| p.fingerprint == fingerprint && p.online) =>
            {
                assert!(
                    !state
                        .performance
                        .peers
                        .iter()
                        .any(|p| p.fingerprint.as_deref() == Some(&fingerprint))
                );
                Some(state.link.since.unwrap())
            }
            _ => None,
        })
        .await;
    let second = timeout(Duration::from_secs(8), async {
        loop {
            if let Some(Notice::State { state }) = ui.conn.recv::<Notice>().await.unwrap()
                && let Some(metrics) = state
                    .performance
                    .peers
                    .iter()
                    .find(|p| p.fingerprint.as_deref() == Some(&fingerprint))
                && metrics.metrics.rtt.count > 0
            {
                return metrics.clone();
            }
        }
    })
    .await
    .expect("重连必须生成新的非空性能窗口");
    assert!(second.measured_at >= reconnected);
    assert!(second.measured_at > first.measured_at);
    client.abort();
    let _ = client.await;
    daemon.handle.send(Command::Quit);
    timeout(Duration::from_secs(3), daemon.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn quitting_after_ui_churn_notifies_every_remaining_ui() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut disconnected = Vec::new();
    let mut remaining = Vec::new();
    for index in 0..12 {
        let mut ui = daemon.ui().await;
        ui.state_where("界面已注册并收到完整状态", |_| true).await;
        if index % 3 == 0 {
            remaining.push(ui);
        } else {
            disconnected.push(ui);
        }
    }
    // 同一执行片段中关闭旧连接并请求退出，不靠sleep猜测Left处理时机。
    drop(disconnected);
    daemon.handle.send(Command::Quit);
    for mut ui in remaining {
        timeout(Duration::from_secs(3), async {
            let mut quit = false;
            while let Some(notice) = ui.conn.recv::<Notice>().await.unwrap() {
                quit |= notice == Notice::Quit;
            }
            assert!(quit, "每个仍连接的界面都必须在EOF之前收到Quit");
        })
        .await
        .expect("所有当前界面应正常关闭");
    }
    let endpoint = daemon.device.store.ipc_endpoint_path();
    timeout(Duration::from_secs(3), daemon.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(!endpoint.exists());
}

#[tokio::test]
async fn installed_daemon_reports_quic_availability_and_preserves_tcp_for_existing_users() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let state = ui
        .state_where("传输能力", |state| {
            state.link.status == LinkStatus::Waiting
        })
        .await;
    assert_eq!(
        state.local.quic_available,
        cfg!(feature = "quic-experiment")
    );
    assert_eq!(
        state.settings.input_transport,
        ferry_types::InputTransport::Tcp
    );
    assert_eq!(
        state.link.input_transport, None,
        "未连接时不能把偏好说成实际协议"
    );
    daemon.task.abort();
}

#[tokio::test]
async fn installed_transport_change_is_saved_and_restarts_or_explicitly_rejected() {
    use ferry_types::InputTransport;
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    let before = ui
        .state_where("启动", |state| state.link.status == LinkStatus::Waiting)
        .await;
    ui.send(Request::Execute {
        session: before.session.clone(),
        id: 510,
        request: Box::new(Request::UpdateSettings {
            settings: SettingsPatch {
                input_transport: Some(InputTransport::QuicReliable),
                ..SettingsPatch::default()
            },
        }),
    })
    .await;
    let (ok, message) = ui
        .wait("切换输入协议的结果", |notice| match notice {
            Notice::CommandResult {
                id: 510,
                ok,
                message,
                ..
            } => Some((*ok, message.clone())),
            _ => None,
        })
        .await;
    if cfg!(feature = "quic-experiment") {
        assert!(ok, "{message}");
        let after = ui
            .state_where("新协议监听已启动", |state| {
                state.settings.input_transport == InputTransport::QuicReliable
                    && state.link.status == LinkStatus::Waiting
            })
            .await;
        assert_ne!(
            after.local.port, before.local.port,
            "使用port0的引擎已重新绑定监听端口"
        );
        assert_eq!(
            daemon.device.store.config().unwrap().input_transport,
            InputTransport::QuicReliable
        );
        let tls = ferry_net::Tls::new(&ferry_trust::Identity::generate("transport probe").unwrap())
            .unwrap();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], after.local.port.unwrap()));
        let (mut stream, _) = tls
            .connect_with(addr, ferry_proto::Channel::Pair, ferry_net::Transport::Quic)
            .await
            .unwrap();
        let welcome = ferry_net::initiate(
            &mut stream,
            ferry_proto::Hello::new(
                ferry_proto::Channel::Pair,
                ferry_proto::Capabilities::QUIC_MOTION,
                0,
                "transport probe",
            ),
        )
        .await
        .unwrap();
        assert!(stream.get_ref().is_quic(), "安装模式真正启动UDP/QUIC监听");
        assert!(
            !welcome
                .capabilities
                .contains(ferry_proto::Capabilities::QUIC_MOTION),
            "可靠模式不协商DATAGRAM"
        );
    } else {
        assert!(!ok);
        assert!(message.contains("QUIC"));
        assert_eq!(
            daemon.device.store.config().unwrap().input_transport,
            InputTransport::Tcp
        );
    }
    daemon.task.abort();
}

#[tokio::test]
async fn installed_client_reports_actual_tcp_and_auto_fallback_separately_from_preference() {
    use ferry_types::InputTransport;
    let server = Daemon::start(MockHost::default()).await;
    let mut server_ui = server.ui().await;
    let listening = server_ui
        .state_where("TCP服务端", |state| state.local.port.is_some())
        .await;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], listening.local.port.unwrap()));
    let client = Daemon::start(MockHost::default()).await;
    let mut client_ui = client.ui().await;
    let _ = client_ui
        .state_where("客户端初始", |state| state.local.port.is_some())
        .await;
    let server_fp = server
        .device
        .store
        .identity("server")
        .unwrap()
        .fingerprint();
    let client_fp = client
        .device
        .store
        .identity("client")
        .unwrap()
        .fingerprint();
    for (store, fingerprint, name) in [
        (&server.device.store, client_fp, "client"),
        (&client.device.store, server_fp, "server"),
    ] {
        let mut peers = ferry_trust::PeerList::new();
        peers.add(ferry_trust::Peer {
            name: name.into(),
            fingerprint,
            addr: None,
        });
        store.save_peers(&peers).unwrap();
    }
    let preference = if cfg!(feature = "quic-experiment") {
        InputTransport::Auto
    } else {
        InputTransport::Tcp
    };
    client_ui
        .send(Request::UpdateSettings {
            settings: SettingsPatch {
                role: Some(Role::Client),
                server: Some(addr.to_string()),
                input_transport: Some(preference),
                ..SettingsPatch::default()
            },
        })
        .await;
    let connected = client_ui
        .state_where("实际TCP协议可见", |state| {
            state.link.input_transport.as_deref() == Some("tcp")
        })
        .await;
    assert_eq!(connected.settings.input_transport, preference);
    assert_eq!(
        connected.link.fallback_reason.as_deref(),
        cfg!(feature = "quic-experiment").then_some("quic_unreachable")
    );
    let peer = connected.peers.iter().find(|peer| peer.online).unwrap();
    assert_eq!(peer.input_transport.as_deref(), Some("tcp"));
    assert_eq!(peer.fallback_reason, connected.link.fallback_reason);
    let server_state = server_ui
        .state_where("服务端实际TCP协议", |state| {
            state.link.input_transport.as_deref() == Some("tcp")
        })
        .await;
    assert_eq!(
        server_state.link.fallback_reason, None,
        "服务端不能猜对方为何选择TCP"
    );
    client_ui.send(Request::SetSharing { on: false }).await;
    let paused = client_ui
        .state_where("断开清空协议", |state| {
            state.link.status == LinkStatus::Paused
        })
        .await;
    assert_eq!(paused.link.input_transport, None);
    assert!(
        paused
            .peers
            .iter()
            .all(|peer| peer.input_transport.is_none() && peer.fallback_reason.is_none())
    );
    client.task.abort();
    server.task.abort();
}

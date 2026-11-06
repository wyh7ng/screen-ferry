//! 版本更新：本机的小 HTTP 服务器当发布站点，用临时生成的发布密钥签清单。

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Cursor, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ferry_engine::resident::UpdateSource;
use ferry_ipc::{Notice, Request, SettingsPatch, UpdateError, UpdateStatus};
use ferry_store::{Installing, UpdateState};
use ferry_update::{Asset, Manifest, Notes, SCHEMA};

use super::{Daemon, MockHost};

/// 一个按路径发文件的 HTTP 服务器。
struct Site {
    base: String,
    files: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    requests: Arc<AtomicUsize>,
}

impl Site {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let files: Arc<Mutex<BTreeMap<String, Vec<u8>>>> = Arc::default();
        let served = files.clone();
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                count.fetch_add(1, Ordering::SeqCst);
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let body = served.lock().unwrap().get(&path).cloned();
                let (status, body) = match body {
                    Some(body) => ("200 OK", body),
                    None => ("404 Not Found", Vec::new()),
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
            }
        });
        Self {
            base,
            files,
            requests,
        }
    }

    fn put(&self, path: &str, body: Vec<u8>) {
        self.files.lock().unwrap().insert(path.to_owned(), body);
    }
}

/// 临时生成的发布密钥。
struct Key {
    public: String,
    secret: minisign::SecretKey,
}

impl Key {
    fn new() -> Self {
        let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        Self {
            public: pair.pk.to_base64(),
            secret: pair.sk,
        }
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        minisign::sign(None, &self.secret, Cursor::new(data), None, None)
            .unwrap()
            .into_string()
            .into_bytes()
    }
}

const PACKAGE: &[u8] = b"pretend this is ScreenFerry 9.0.0";

fn sha256(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// 在站点上发布 9.0.0：清单、签名、更新包。`expires_in` 是过期时间离现在多少秒。
fn publish(site: &Site, key: &Key, expires_in: i64) {
    let now = now_secs();
    let manifest = Manifest {
        schema: SCHEMA,
        version: "9.0.0".into(),
        published: Manifest::format_time(now - 60),
        expires: Manifest::format_time(now + expires_in),
        protocol: ferry_proto::PROTOCOL_VERSION,
        notes: Notes {
            zh: "新功能".into(),
            en: "New things".into(),
        },
        assets: [(
            "test".to_owned(),
            Asset {
                url: format!("{}/ScreenFerry-9.0.0-test.zip", site.base),
                size: PACKAGE.len() as u64,
                sha256: sha256(PACKAGE),
            },
        )]
        .into(),
    };
    let data = serde_json::to_vec(&manifest).unwrap();
    site.put("/update.json.minisig", key.sign(&data));
    site.put("/update.json", data);
    site.put("/ScreenFerry-9.0.0-test.zip", PACKAGE.to_vec());
}

/// 自动检查关着（不自己下载），清单来自 `site`、用 `key` 校验。
async fn daemon(site: &Site, key: &Key, host: MockHost) -> Daemon {
    let host = MockHost {
        update_source: Some(UpdateSource {
            manifest_url: format!("{}/update.json", site.base),
            keys: vec![key.public.clone()],
        }),
        ..host
    };
    let daemon = Daemon::start(host).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::UpdateSettings {
        settings: SettingsPatch {
            auto_update: Some(false),
            ..SettingsPatch::default()
        },
    })
    .await;
    ui.state_where("关掉自动检查", |s| !s.settings.auto_update)
        .await;
    daemon
}

#[tokio::test]
async fn a_newer_signed_release_is_found_downloaded_and_checked() {
    let (site, key) = (Site::start(), Key::new());
    publish(&site, &key, 180 * 86_400);
    let daemon = daemon(&site, &key, MockHost::default()).await;
    let mut ui = daemon.ui().await;

    ui.send(Request::CheckUpdate).await;
    let state = ui
        .state_where("发现新版本", |s| {
            s.update.status == UpdateStatus::Available
        })
        .await;
    let latest = state.update.latest.as_ref().unwrap();
    assert_eq!(latest.version, "9.0.0");
    assert_eq!(latest.notes_en, "New things");
    assert_eq!(latest.size, PACKAGE.len() as u64);
    assert!(!latest.breaks_peers);
    assert!(state.update.can_install);
    assert!(state.update.checked_at.is_some());

    ui.send(Request::DownloadUpdate).await;
    ui.state_where("下载好并核对过", |s| {
        s.update.status == UpdateStatus::Ready
    })
    .await;
    let saved = daemon
        .device
        .store
        .updates_dir()
        .join("ScreenFerry-9.0.0-test.zip");
    assert_eq!(std::fs::read(saved).unwrap(), PACKAGE);
    assert!(
        daemon.device.store.update_state().seen_published.is_some(),
        "记下见过的清单，防回滚"
    );
    daemon.task.abort();
}

#[tokio::test]
async fn a_package_that_does_not_match_the_manifest_is_deleted() {
    let (site, key) = (Site::start(), Key::new());
    publish(&site, &key, 180 * 86_400);
    site.put(
        "/ScreenFerry-9.0.0-test.zip",
        b"pretend this is evil!!!!!!!!!!!!!".to_vec(),
    );
    let daemon = daemon(&site, &key, MockHost::default()).await;
    let mut ui = daemon.ui().await;

    ui.send(Request::CheckUpdate).await;
    ui.state_where("发现新版本", |s| {
        s.update.status == UpdateStatus::Available
    })
    .await;
    ui.send(Request::DownloadUpdate).await;
    let state = ui
        .state_where("核对失败", |s| s.update.status == UpdateStatus::Failed)
        .await;
    assert_eq!(state.update.error, Some(UpdateError::VerifyFailed));
    assert!(
        !daemon
            .device
            .store
            .updates_dir()
            .join("ScreenFerry-9.0.0-test.zip")
            .exists()
    );
    daemon.task.abort();
}

#[tokio::test]
async fn untrusted_and_expired_manifests_are_reported() {
    let (site, ours, theirs) = (Site::start(), Key::new(), Key::new());
    publish(&site, &theirs, 180 * 86_400);
    let daemon = daemon(&site, &ours, MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::CheckUpdate).await;
    let state = ui
        .state_where("签名对不上", |s| {
            s.update.status == UpdateStatus::Failed
        })
        .await;
    assert_eq!(state.update.error, Some(UpdateError::VerifyFailed));
    assert!(state.update.latest.is_none());

    publish(&site, &ours, -60);
    ui.send(Request::CheckUpdate).await;
    let state = ui
        .state_where("清单过期", |s| {
            s.update.error == Some(UpdateError::Stale)
        })
        .await;
    assert!(
        state.update.latest.is_none(),
        "过期的清单不能说有新版本，也不能说已是最新"
    );
    daemon.task.abort();
}

#[tokio::test]
async fn without_release_keys_nothing_can_be_verified() {
    let daemon = Daemon::start(MockHost::default()).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::CheckUpdate).await;
    let state = ui
        .state_where("没有发布公钥", |s| {
            s.update.status == UpdateStatus::Failed
        })
        .await;
    assert_eq!(state.update.error, Some(UpdateError::NoKeys));
    daemon.task.abort();
}

#[tokio::test]
async fn a_blocked_platform_offers_a_manual_download_instead() {
    let (site, key) = (Site::start(), Key::new());
    publish(&site, &key, 180 * 86_400);
    let host = MockHost::default();
    host.updater.block("这份是临时签名的开发版本");
    let daemon = daemon(&site, &key, host).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::CheckUpdate).await;
    let state = ui
        .state_where("发现新版本", |s| {
            s.update.status == UpdateStatus::Available
        })
        .await;
    assert!(!state.update.can_install);
    daemon.task.abort();
}

#[tokio::test]
async fn installing_restarts_and_rolls_back_when_the_new_version_cannot_start() {
    let (site, key) = (Site::start(), Key::new());
    publish(&site, &key, 180 * 86_400);
    let host = MockHost::default();
    // 启动新版本失败：立即换回旧版本、再启动它，不用等。
    host.updater.cancel_launch();
    let daemon = daemon(&site, &key, host.clone()).await;
    let mut ui = daemon.ui().await;
    ui.send(Request::CheckUpdate).await;
    ui.state_where("发现新版本", |s| {
        s.update.status == UpdateStatus::Available
    })
    .await;
    ui.send(Request::DownloadUpdate).await;
    ui.state_where("下载好", |s| s.update.status == UpdateStatus::Ready)
        .await;

    ui.send(Request::InstallUpdate).await;
    ui.wait("后台为安装退出，界面跟着关闭", |n| {
        (*n == Notice::Quit).then_some(())
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(5), daemon.task)
        .await
        .expect("后台退出了")
        .unwrap()
        .unwrap();
    assert_eq!(
        host.updater.calls(),
        ["prepare 9.0.0", "launch", "rollback", "launch"]
    );
    let installing = daemon.device.store.update_state().installing.unwrap();
    assert_eq!(installing.from, env!("CARGO_PKG_VERSION"));
    assert_eq!(installing.to, "9.0.0");
    assert!(installing.open_settings, "安装前界面开着，装好后重新打开");
}

#[tokio::test]
async fn after_an_update_the_new_version_says_so_and_reopens_the_settings() {
    let host = MockHost::default();
    let device = crate::support::Device::new();
    device
        .store
        .save_update_state(&UpdateState {
            installing: Some(Installing {
                from: "0.0.9".into(),
                to: env!("CARGO_PKG_VERSION").into(),
                open_settings: true,
            }),
            ..UpdateState::default()
        })
        .unwrap();
    let daemon = Daemon::start_with(device, host.clone()).await;
    let mut ui = daemon.ui().await;
    let phrase = ui
        .wait("更新完成的提示", |n| match n {
            Notice::Toast {
                phrase: Some(p), ..
            } if p.key == "update_installed" => Some(p.clone()),
            _ => None,
        })
        .await;
    assert_eq!(phrase.args["to"], env!("CARGO_PKG_VERSION"));
    assert!(host.settings_opened.load(Ordering::SeqCst) >= 1);
    assert!(daemon.device.store.update_state().installing.is_none());
    daemon.task.abort();
}

/// 快进到自动检查的窗口，然后恢复真实时间供阻塞 HTTP 线程完成。
async fn first_automatic_check() {
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(181)).await;
    tokio::time::resume();
}

#[tokio::test]
async fn an_offline_automatic_check_does_not_claim_the_app_is_current() {
    let (site, key) = (Site::start(), Key::new());
    let host = MockHost {
        update_source: Some(UpdateSource {
            manifest_url: format!("{}/update.json", site.base),
            keys: vec![key.public],
        }),
        ..MockHost::default()
    };
    let daemon = Daemon::start(host).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    first_automatic_check().await;
    crate::support::eventually("自动尝试检查", || {
        site.requests.load(Ordering::SeqCst) > 0
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    ui.send(Request::SetVerbose { on: true }).await;
    let state = ui
        .state_where("检查失败仍然未知", |s| {
            s.local.verbose_since.is_some()
        })
        .await;
    assert_eq!(state.update.status, UpdateStatus::Idle);
    assert_eq!(state.update.checked_at, None);
    assert_eq!(site.requests.load(Ordering::SeqCst), 1, "失败后不立刻重试");
    daemon.task.abort();
}

#[tokio::test]
async fn automatic_signature_failures_back_off() {
    let (site, ours, theirs) = (Site::start(), Key::new(), Key::new());
    publish(&site, &theirs, 180 * 86_400);
    let host = MockHost {
        update_source: Some(UpdateSource {
            manifest_url: format!("{}/update.json", site.base),
            keys: vec![ours.public],
        }),
        ..MockHost::default()
    };
    let daemon = Daemon::start(host).await;
    let mut ui = daemon.ui().await;
    ui.state_where("首次状态", |_| true).await;
    first_automatic_check().await;
    ui.state_where("自动验签失败", |s| {
        s.update.error == Some(UpdateError::VerifyFailed)
    })
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    assert_eq!(
        site.requests.load(Ordering::SeqCst),
        2,
        "仅请求一次清单和签名"
    );
    daemon.task.abort();
}

#[tokio::test]
async fn snapshots_use_cached_install_capability() {
    let host = MockHost::default();
    let daemon = Daemon::start(host.clone()).await;
    let mut ui = daemon.ui().await;
    ui.state_where("平台能力准备好了", |s| s.update.can_install)
        .await;
    let checks = host.updater.capability_checks();
    for on in [true, false, true] {
        ui.send(Request::SetVerbose { on }).await;
        ui.state_where("状态更新", |s| s.local.verbose_since.is_some() == on)
            .await;
    }
    assert_eq!(
        host.updater.capability_checks(),
        checks,
        "快照不再执行系统能力检查"
    );
    daemon.task.abort();
}

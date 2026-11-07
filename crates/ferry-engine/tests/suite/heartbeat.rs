//! 真实会话的心跳与平台阶段统计，不把成功写入当成对端已经处理。

use std::time::Duration;

use ferry_engine::{Event, Events};

use crate::support::{Rig, Setup};

#[tokio::test]
async fn both_peers_measure_heartbeat_writes_and_cursor_switches() {
    let (server_events, mut server_rx) = Events::channel();
    let (client_events, mut client_rx) = Events::channel();
    let rig = Rig::with(Setup {
        mac_events: server_events,
        pc_events: client_events,
        ..Setup::default()
    })
    .await;
    rig.enter().await;
    rig.leave().await;

    let server = async {
        let (mut local, mut peer) = (None, None);
        while local.is_none() || peer.is_none() {
            if let Event::Metrics {
                peer: source,
                metrics,
            } = server_rx.recv().await.unwrap()
            {
                if source.is_some() {
                    peer = Some(metrics);
                } else {
                    local = Some(metrics);
                }
            }
        }
        (local.unwrap(), peer.unwrap())
    };
    let client = async {
        loop {
            if let Event::Metrics {
                peer: Some(_),
                metrics,
            } = client_rx.recv().await.unwrap()
            {
                break metrics;
            }
        }
    };
    let ((local, server), client) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, client)
    })
    .await
    .expect("两端在正常5秒窗口内发布统计");

    assert!(local.cursor_switch.count >= 2, "控制端记录抓取与归还");
    assert!(
        client.cursor_switch.count >= 3,
        "被控端记录初始隐藏、进入和离开"
    );
    assert!(local.display_query.count > 0 && client.display_query.count > 0);
    assert!(server.heartbeat_send.count > 0 && client.heartbeat_send.count > 0);
    assert!(server.rtt.count > 0 && client.rtt.count > 0);
}

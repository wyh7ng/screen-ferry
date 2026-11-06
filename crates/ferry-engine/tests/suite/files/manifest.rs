use std::fs;

use super::{send_when_ready, write};
use crate::support::{Rig, TempDir, eventually};

#[tokio::test]
async fn repeated_source_paths_are_deduplicated_without_losing_distinct_same_names() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let a = source.path().join("a/item.txt");
    let b = source.path().join("b/item.txt");
    write(&a, b"A");
    write(&b, b"B");
    send_when_ready(&rig.pc_files, vec![b.clone(), a.clone(), b, a]).await;
    let target = rig.mac.store.downloads_dir();
    eventually("两个唯一源均收到", || {
        target.join("item (2).txt").exists()
    })
    .await;
    assert_eq!(fs::read(target.join("item.txt")).unwrap(), b"A");
    assert_eq!(fs::read(target.join("item (2).txt")).unwrap(), b"B");
    assert!(!target.join("item (3).txt").exists());
}

#[tokio::test]
async fn preparation_reports_multiple_failures_and_sends_nothing() {
    let rig = Rig::start().await;
    let source = TempDir::new();
    let seed = source.path().join("seed.txt");
    write(&seed, b"ready");
    send_when_ready(&rig.pc_files, vec![seed]).await;
    let valid = source.path().join("valid.txt");
    write(&valid, b"valid");
    let peer = rig.pc_files.connected()[0];
    let error = rig
        .pc_files
        .send(
            peer,
            vec![
                valid,
                source.path().join("secret-one"),
                source.path().join("secret-two"),
            ],
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("2处"), "{error}");
    assert!(!error.contains("secret"));
    assert!(!rig.mac.store.downloads_dir().join("valid.txt").exists());
}

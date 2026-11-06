use ferry_platform::FileCommit;
use ferry_platform_macos::MacFileCommit;
use std::fs;
use std::sync::{Arc, Barrier};

fn dir() -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "ferry-native-commit-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn simultaneous_native_commits_never_replace_existing_content() {
    let dir = dir();
    let target = dir.join("received");
    let barrier = Arc::new(Barrier::new(2));
    let threads: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
        .into_iter()
        .enumerate()
        .map(|(i, bytes)| {
            let source = dir.join(format!("source-{i}"));
            fs::write(&source, bytes).unwrap();
            let target = target.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                MacFileCommit.rename_no_replace(&source, &target)
            })
        })
        .collect();
    let mut succeeded = 0;
    for worker in threads {
        match worker.join().unwrap() {
            Ok(()) => succeeded += 1,
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists),
        }
    }
    assert_eq!(succeeded, 1);
    let content = fs::read(&target).unwrap();
    assert!(content == b"first" || content == b"second");
    assert_eq!(
        [dir.join("source-0"), dir.join("source-1")]
            .iter()
            .filter(|p| p.exists())
            .count(),
        1
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_directory_commit_preserves_an_existing_directory() {
    let dir = dir();
    let source = dir.join("source");
    let target = dir.join("target");
    fs::create_dir(&source).unwrap();
    fs::create_dir(&target).unwrap();
    fs::write(source.join("new"), b"new").unwrap();
    fs::write(target.join("old"), b"old").unwrap();
    assert_eq!(
        MacFileCommit
            .rename_no_replace(&source, &target)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read(target.join("old")).unwrap(), b"old");
    assert!(source.join("new").exists());
    let free = dir.join("free");
    MacFileCommit.rename_no_replace(&source, &free).unwrap();
    assert_eq!(fs::read(free.join("new")).unwrap(), b"new");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stable_identity_follows_the_open_file_across_append_and_rename() {
    use std::io::Write;
    let dir = dir();
    let source = dir.join("source");
    let target = dir.join("target");
    fs::write(&source, b"a").unwrap();
    let file = fs::File::open(&source).unwrap();
    let identity = MacFileCommit.identity_open_file(&file).unwrap();
    assert_eq!(identity, MacFileCommit.identity(&source).unwrap());
    fs::OpenOptions::new()
        .append(true)
        .open(&source)
        .unwrap()
        .write_all(b"more")
        .unwrap();
    assert_eq!(identity, MacFileCommit.identity_open_file(&file).unwrap());
    MacFileCommit.rename_no_replace(&source, &target).unwrap();
    assert_eq!(identity, MacFileCommit.identity(&target).unwrap());
    fs::write(&source, b"new-file").unwrap();
    assert_ne!(identity, MacFileCommit.identity(&source).unwrap());
    drop(file);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn journal_replacement_is_atomic_and_replaces_only_the_record() {
    let dir = dir();
    let old = dir.join("intent.json");
    let next = dir.join("intent.tmp");
    fs::write(&old, b"old").unwrap();
    fs::write(&next, b"new").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&next)
        .unwrap()
        .sync_all()
        .unwrap();
    MacFileCommit.replace_record(&next, &old).unwrap();
    assert_eq!(fs::read(&old).unwrap(), b"new");
    assert!(!next.exists());
    fs::remove_dir_all(dir).unwrap();
}

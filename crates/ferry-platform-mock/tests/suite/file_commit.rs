use std::fs;
use std::io::Write;
use std::path::PathBuf;

use ferry_platform::FileCommit;
use ferry_platform_mock::MockFileCommit;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ferry-mock-identity-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn distinct_files_keep_distinct_stable_identities_across_append_and_rename() {
    let dir = TempDir::new();
    let first = dir.0.join("first");
    let second = dir.0.join("second");
    fs::write(&first, b"first").unwrap();
    fs::write(&second, b"second").unwrap();
    let handle = fs::File::open(&first).unwrap();
    let identity = MockFileCommit.identity_open_file(&handle).unwrap();
    assert_eq!(identity, MockFileCommit.identity(&first).unwrap());
    assert_ne!(identity, MockFileCommit.identity(&second).unwrap());
    fs::OpenOptions::new()
        .append(true)
        .open(&first)
        .unwrap()
        .write_all(b"appended")
        .unwrap();
    assert_eq!(
        identity,
        MockFileCommit.identity_open_file(&handle).unwrap()
    );
    let renamed = dir.0.join("renamed");
    MockFileCommit.rename_no_replace(&first, &renamed).unwrap();
    assert_eq!(identity, MockFileCommit.identity(&renamed).unwrap());
    assert_eq!(
        identity,
        MockFileCommit.identity_open_file(&handle).unwrap()
    );
}

#[cfg(target_os = "windows")]
#[test]
fn identical_creation_times_do_not_make_distinct_windows_files_the_same_object() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::Storage::FileSystem::SetFileTime;

    let dir = TempDir::new();
    let first = dir.0.join("first");
    let second = dir.0.join("second");
    fs::write(&first, b"first").unwrap();
    fs::write(&second, b"second").unwrap();
    let time = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 30_000_000,
    };
    for path in [&first, &second] {
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        // SAFETY: file 的句柄与 time 在整个调用期间有效；只设置创建时间。
        unsafe { SetFileTime(HANDLE(file.as_raw_handle()), Some(&time), None, None) }.unwrap();
    }
    assert_eq!(
        fs::metadata(&first).unwrap().created().unwrap(),
        fs::metadata(&second).unwrap().created().unwrap()
    );
    assert_ne!(
        MockFileCommit.identity(&first).unwrap(),
        MockFileCommit.identity(&second).unwrap()
    );
    assert_ne!(
        MockFileCommit
            .identity_open_file(&fs::File::open(first).unwrap())
            .unwrap(),
        MockFileCommit
            .identity_open_file(&fs::File::open(second).unwrap())
            .unwrap()
    );
}

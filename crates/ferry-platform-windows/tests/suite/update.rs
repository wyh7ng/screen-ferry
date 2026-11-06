use ferry_platform_windows::parse_proxy_server;

#[test]
fn a_single_proxy_server_is_used_for_everything() {
    assert_eq!(
        parse_proxy_server("127.0.0.1:7890").as_deref(),
        Some("http://127.0.0.1:7890")
    );
}

#[test]
fn per_scheme_settings_prefer_https() {
    assert_eq!(
        parse_proxy_server("http=10.0.0.1:8080;https=127.0.0.1:7890;socks=127.0.0.1:7891")
            .as_deref(),
        Some("http://127.0.0.1:7890")
    );
    assert_eq!(
        parse_proxy_server("http=10.0.0.1:8080").as_deref(),
        Some("http://10.0.0.1:8080")
    );
    assert_eq!(parse_proxy_server("socks=127.0.0.1:7891"), None);
    assert_eq!(parse_proxy_server(""), None);
}

#[test]
fn installation_rechecks_the_locked_file_and_holds_it_against_replacement() {
    use ferry_platform_windows::locked_package;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    let path = std::env::temp_dir().join(format!(
        "screenferry-package-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    fs::write(&path, b"abc").unwrap();
    let held = locked_package(&path, 3, hash).unwrap();
    assert!(fs::write(&path, b"xyz").is_err(), "验证后的句柄阻止写入");
    assert!(fs::remove_file(&path).is_err(), "验证后的句柄阻止替换");
    drop(held);
    fs::write(&path, b"xyz").unwrap();
    assert!(
        locked_package(&path, 3, hash).is_err(),
        "安装前被换掉的同长度文件也被拒绝"
    );
    fs::write(&path, b"abc").unwrap();
    assert!(locked_package(&path, 4, hash).is_err());
    fs::remove_file(path).unwrap();
}

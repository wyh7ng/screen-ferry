use ferry_platform::Displays;
use ferry_platform_macos::MacDisplays;

#[test]
#[ignore = "需要唤醒的 macOS 显示器；实机桌面环境显式运行"]
fn reports_one_primary_display_with_real_size() {
    let displays = MacDisplays.displays().unwrap();
    assert!(!displays.is_empty());
    assert_eq!(displays.iter().filter(|d| d.primary).count(), 1);
    for d in &displays {
        assert!(d.bounds.width > 0 && d.bounds.height > 0, "{d:?}");
        assert_eq!(d.scale_percent, 100, "macOS 的坐标已是逻辑单位：{d:?}");
    }
    println!("{displays:#?}");
}

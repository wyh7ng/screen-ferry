use ferry_types::{ClipboardContent, ClipboardFormat};

#[test]
fn text_content_holds_one_format() {
    let content = ClipboardContent::text("你好");
    assert_eq!(content.as_text(), Some("你好"));
    assert_eq!(content.total_len(), "你好".len());
    assert_eq!(content.iter().count(), 1);
}

#[test]
fn inserting_the_same_format_replaces_it() {
    let mut content = ClipboardContent::text("旧");
    content.insert(ClipboardFormat::TEXT, b"new".to_vec());
    content.insert(ClipboardFormat::HTML, b"<b>new</b>".to_vec());

    let formats: Vec<_> = content.iter().map(|(f, _)| f).collect();
    assert_eq!(formats, [ClipboardFormat::TEXT, ClipboardFormat::HTML]);
    assert_eq!(content.as_text(), Some("new"));
}

#[test]
fn invalid_utf8_is_not_text() {
    let mut content = ClipboardContent::new();
    content.insert(ClipboardFormat::TEXT, vec![0xFF, 0xFE]);
    assert_eq!(content.as_text(), None);
    assert!(!content.is_empty());
}

#[test]
fn cloned_contents_share_bytes_and_format_removal_is_independent() {
    use std::sync::Arc;
    let mut original = ClipboardContent::text("plain");
    original.insert(ClipboardFormat::PNG, vec![7; 1024]);
    let image = original.get_shared(ClipboardFormat::PNG).unwrap();
    let mut peer = original.clone();
    assert!(Arc::ptr_eq(
        &image,
        &peer.get_shared(ClipboardFormat::PNG).unwrap()
    ));
    let removed = peer.remove_shared(ClipboardFormat::PNG).unwrap();
    assert!(Arc::ptr_eq(&image, &removed));
    assert!(peer.get(ClipboardFormat::PNG).is_none());
    assert_eq!(original.get(ClipboardFormat::PNG), Some(&vec![7; 1024][..]));
    let mut owned = original.remove(ClipboardFormat::PNG).unwrap();
    owned[0] = 8;
    assert_eq!(image[0], 7, "兼容的可变Vec API也不能改变共享原图");
}

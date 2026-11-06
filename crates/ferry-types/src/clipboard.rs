//! 剪贴板数据格式与内容。

use std::collections::BTreeMap;
use std::sync::Arc;

/// 剪贴板数据格式。以后可能新增格式，所以用编号而不是封闭枚举。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClipboardFormat(pub u8);

impl ClipboardFormat {
    /// UTF-8 纯文本，换行统一为 `\n`。
    pub const TEXT: Self = Self(1);
    /// UTF-8 HTML 片段。
    pub const HTML: Self = Self(2);
    /// RTF 富文本。
    pub const RTF: Self = Self(3);
    /// PNG 图片。
    pub const PNG: Self = Self(4);
}

/// 一份剪贴板内容：同一内容的若干种格式，每种格式至多一份。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipboardContent {
    items: BTreeMap<ClipboardFormat, Arc<[u8]>>,
}

impl ClipboardContent {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只有纯文本的内容。
    pub fn text(text: impl Into<String>) -> Self {
        let mut content = Self::new();
        content.insert(ClipboardFormat::TEXT, text.into().into_bytes());
        content
    }

    /// 放入一种格式，替换已有的同格式数据。
    pub fn insert(&mut self, format: ClipboardFormat, data: Vec<u8>) {
        self.items.insert(format, Arc::from(data));
    }

    pub fn get(&self, format: ClipboardFormat) -> Option<&[u8]> {
        self.items.get(&format).map(AsRef::as_ref)
    }

    /// 取出一种格式，兼容需要独占可变字节的调用方。
    pub fn remove(&mut self, format: ClipboardFormat) -> Option<Vec<u8>> {
        self.remove_shared(format).map(|data| data.to_vec())
    }

    /// 共享只读字节；克隆内容和多设备通告都不复制整张图片。
    pub fn get_shared(&self, format: ClipboardFormat) -> Option<Arc<[u8]>> {
        self.items.get(&format).cloned()
    }

    /// 只从当前内容移除格式，其他内容及缓存仍持有相同的只读字节。
    pub fn remove_shared(&mut self, format: ClipboardFormat) -> Option<Arc<[u8]>> {
        self.items.remove(&format)
    }

    /// 纯文本格式的内容；没有或不是合法 UTF-8 时为 `None`。
    pub fn as_text(&self) -> Option<&str> {
        std::str::from_utf8(self.get(ClipboardFormat::TEXT)?).ok()
    }

    /// 按格式编号从小到大遍历。
    pub fn iter(&self) -> impl Iterator<Item = (ClipboardFormat, &[u8])> {
        self.items.iter().map(|(f, d)| (*f, d.as_ref()))
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 所有格式的总字节数。
    pub fn total_len(&self) -> usize {
        self.items.values().map(|data| data.len()).sum()
    }
}

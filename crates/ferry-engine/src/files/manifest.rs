//! 发送方：把用户给的路径展开成清单。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::UNIX_EPOCH;

use ferry_core::file_name::{MAX_DEPTH, clean_name, numbered};
use ferry_proto::limits::MAX_PATH_LEN;
use ferry_proto::{EntryKind, FileEntry, FileOffer};
use ferry_types::TransferId;

/// 一次最多发送的条目数（文件加文件夹）。
pub(crate) const MAX_ENTRIES: usize = 100_000;

/// 系统自己生成的文件，对方用不上，不发送。
const SKIPPED: [&str; 3] = [".DS_Store", "Thumbs.db", "desktop.ini"];

/// 一次发送的完整清单。
pub(crate) struct Plan {
    pub(crate) id: TransferId,
    /// 第一个顶层项目的名字。
    pub(crate) name: String,
    pub(crate) items: u32,
    pub(crate) files: u32,
    pub(crate) bytes: u64,
    /// 按发送顺序排列，文件夹在它包含的条目之前。
    pub(crate) entries: Vec<FileEntry>,
    /// 与 `entries` 一一对应的本机路径。
    pub(crate) sources: Vec<PathBuf>,
}

impl Plan {
    pub(crate) fn offer(&self) -> FileOffer {
        FileOffer {
            id: self.id,
            name: self.name.clone(),
            items: self.items,
            files: self.files,
            bytes: self.bytes,
        }
    }
}

/// 展开 `paths`：文件原样，文件夹递归展开。
///
/// 子项按名字排序，同样的内容两次展开得到同样的清单，续传才对得上。
/// 名字按两个系统都能用的规则清洗，同一层里清洗后重名（不分大小写）的自动编号。
/// 顶层的符号链接跟随一次，文件夹里的符号链接跳过，避免循环和意外带走别处的文件。
pub(crate) fn build(
    id: TransferId,
    paths: &[PathBuf],
    cancelled: &AtomicBool,
) -> Result<Plan, String> {
    if paths.len() > MAX_ENTRIES {
        return Err(format!("一次最多发送 {MAX_ENTRIES} 个文件和文件夹"));
    }
    let mut plan = Plan {
        id,
        name: String::new(),
        items: 0,
        files: 0,
        bytes: 0,
        entries: Vec::new(),
        sources: Vec::new(),
    };
    let mut names = Names::default();
    let mut scan = Scan {
        visited: paths.len(),
        ..Scan::default()
    };
    // 相同源路径只发送一次；稳定排序让选择顺序不同也得到相同清单与编号。
    let mut ordered: Vec<_> = paths.iter().collect();
    ordered.sort();
    ordered.dedup();
    let mut sources = HashSet::new();
    for path in ordered {
        check_cancelled(cancelled)?;
        let raw = path
            .file_name()
            .ok_or_else(|| "不能发送整个磁盘".to_owned())?
            .to_string_lossy();
        let canonical = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(_) => {
                scan.failed("无法读取所选文件，请检查文件是否存在以及读取权限".into())?;
                continue;
            }
        };
        if !sources.insert(canonical) {
            continue;
        }
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(_) => {
                scan.failed("无法读取所选文件，请检查文件是否存在以及读取权限".into())?;
                continue;
            }
        };
        let name = names.take(&clean_name(&raw));
        let before = plan.entries.len();
        if let Err(reason) = add(
            &mut plan,
            path,
            &meta,
            name.clone(),
            1,
            cancelled,
            &mut scan,
        ) {
            check_cancelled(cancelled)?;
            if scan.count >= 32 || scan.visited > MAX_ENTRIES {
                return Err(reason);
            }
            scan.failed(reason)?;
        }
        if plan.entries.len() > before {
            plan.items += 1;
            if plan.name.is_empty() {
                plan.name = name;
            }
        }
    }
    if !scan.failures.is_empty() {
        return Err(scan.summary());
    }
    if plan.entries.is_empty() {
        return Err("没有可以发送的文件".into());
    }
    Ok(plan)
}

fn add(
    plan: &mut Plan,
    source: &Path,
    meta: &Metadata,
    rel: String,
    depth: usize,
    cancelled: &AtomicBool,
    scan: &mut Scan,
) -> Result<(), String> {
    check_cancelled(cancelled)?;
    if plan.entries.len() >= MAX_ENTRIES {
        return Err(format!("一次最多发送 {MAX_ENTRIES} 个文件和文件夹"));
    }
    if rel.len() > MAX_PATH_LEN || depth > MAX_DEPTH {
        return Err("所选内容的路径太长或层级太深".into());
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

    if meta.is_dir() {
        plan.entries.push(FileEntry {
            path: rel.clone(),
            kind: EntryKind::Dir,
            size: 0,
            modified,
        });
        plan.sources.push(source.to_owned());

        let entries =
            fs::read_dir(source).map_err(|_| "无法读取所选文件夹，请检查读取权限".to_owned())?;
        let mut children = Vec::new();
        for entry in entries {
            check_cancelled(cancelled)?;
            scan.visited += 1;
            if scan.visited > MAX_ENTRIES {
                return Err("扫描条目数超过上限，请缩小发送范围".into());
            }
            match entry {
                Ok(entry) => children.push(entry.file_name()),
                Err(_) => scan.failed("读取文件夹内容失败，请重新发送".into())?,
            }
        }
        children.sort();
        let mut names = Names::default();
        for child in children {
            let raw = child.to_string_lossy();
            if SKIPPED.contains(&raw.as_ref()) {
                continue;
            }
            let path = source.join(&child);
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => {
                    scan.failed("所选内容在整理期间发生变化或无法读取，请重新发送".into())?;
                    continue;
                }
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            let name = names.take(&clean_name(&raw));
            if let Err(reason) = add(
                plan,
                &path,
                &meta,
                format!("{rel}/{name}"),
                depth + 1,
                cancelled,
                scan,
            ) {
                check_cancelled(cancelled)?;
                if scan.count >= 32 || scan.visited > MAX_ENTRIES {
                    return Err(reason);
                }
                scan.failed(reason)?;
            }
        }
    } else if meta.is_file() {
        plan.entries.push(FileEntry {
            path: rel,
            kind: EntryKind::File,
            size: meta.len(),
            modified,
        });
        plan.sources.push(source.to_owned());
        plan.files += 1;
        plan.bytes = plan
            .bytes
            .checked_add(meta.len())
            .ok_or("文件总大小超过支持范围")?;
    }
    // 设备、管道等特殊文件跳过。
    Ok(())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err("已取消整理文件".into())
    } else {
        Ok(())
    }
}

#[derive(Default)]
struct Names {
    taken: HashSet<String>,
    next: HashMap<String, u32>,
}

impl Names {
    fn take(&mut self, name: &str) -> String {
        let key = name.to_lowercase();
        if self.taken.insert(key.clone()) {
            return name.to_owned();
        }
        let next = self.next.entry(key).or_insert(2);
        loop {
            let candidate = numbered(name, *next);
            *next += 1;
            if self.taken.insert(candidate.to_lowercase()) {
                return candidate;
            }
        }
    }
}

#[derive(Default)]
struct Scan {
    visited: usize,
    failures: BTreeMap<String, usize>,
    count: usize,
}

impl Scan {
    fn failed(&mut self, reason: String) -> Result<(), String> {
        self.count += 1;
        *self.failures.entry(reason).or_default() += 1;
        if self.count >= 32 || self.visited > MAX_ENTRIES {
            Err(self.summary())
        } else {
            Ok(())
        }
    }

    fn summary(&self) -> String {
        format!(
            "文件准备失败（{}处，达到32处或扫描上限即停止）；未发送任何内容：{}",
            self.count,
            self.failures
                .iter()
                .map(|(reason, count)| format!("{reason}（{count}处）"))
                .collect::<Vec<_>>()
                .join("；")
        )
    }
}

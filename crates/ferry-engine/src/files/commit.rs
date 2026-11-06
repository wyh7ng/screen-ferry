//! 文件提交意图与恢复。先记录目标、卷内身份和内容摘要，再原子不覆盖改名。
//!
//! 恢复时仅认身份与内容均匹配的目标；用户移动、替换或修改过的文件一律保留并报错。
//! fsync/写穿缩小掉电窗口，但不承诺所有文件系统都提供跨目录掉电事务。

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ferry_core::file_name::numbered;
use ferry_platform::FileCommit;
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};

use super::{Cancellation, Stop};

pub(super) const INTENT: &str = "commit-intent.json";
pub(super) const RECEIPT: &str = "committed.json";
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Intent {
    version: u32,
    manifest: String,
    items: Vec<Item>,
}

#[derive(Serialize, Deserialize)]
struct Item {
    name: String,
    source: PathBuf,
    target: PathBuf,
    identity: String,
    digest: String,
    published: bool,
    number: u32,
    #[serde(default)]
    staging: Option<Stage>,
}

#[derive(Serialize, Deserialize)]
struct Stage {
    path: PathBuf,
    identity: String,
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    version: u32,
    manifest: String,
    saved: Vec<PathBuf>,
    completed: u64,
}

pub(super) fn recover(
    work: &Path,
    downloads: &Path,
    manifest: &str,
    platform: &dyn FileCommit,
    cancel: &Cancellation,
) -> Result<Option<Vec<PathBuf>>, Stop> {
    let Some(bytes) = read(&work.join(INTENT))? else {
        return Ok(None);
    };
    let mut intent: Intent = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
    load_items(work, &mut intent)?;
    if intent.version != 1 || intent.manifest != manifest {
        return Err(conflict());
    }
    for item in &intent.items {
        let source_ok = item.source == work.join("data").join(&item.name)
            || item.source.parent().is_some_and(|dir| {
                dir.parent() == Some(downloads)
                    && dir
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(".screenferry-"))
            }) && item.source.file_name().is_some_and(|n| n == "item");
        if !source_ok
            || item.target.parent() != Some(downloads)
            || !single_name(Path::new(&item.name))
            || item
                .target
                .file_name()
                .is_none_or(|name| !single_name(Path::new(name)))
        {
            return Err(conflict());
        }
        if let Some(stage) = &item.staging
            && (stage.path.parent() != Some(downloads)
                || stage
                    .path
                    .file_name()
                    .is_none_or(|n| !n.to_string_lossy().starts_with(".screenferry-")))
        {
            return Err(conflict());
        }
    }
    finish(work, downloads, intent, platform, cancel, true).map(Some)
}

pub(super) fn publish(
    data: &Path,
    tops: Vec<String>,
    downloads: &Path,
    work: &Path,
    manifest: String,
    platform: &dyn FileCommit,
    cancel: &Cancellation,
) -> Result<Vec<PathBuf>, Stop> {
    fs::create_dir_all(downloads).map_err(|_| failed())?;
    let mut items = Vec::with_capacity(tops.len());
    for name in tops {
        cancel.check()?;
        let source = data.join(&name);
        let identity = platform.identity(&source).map_err(|_| failed())?;
        let digest = tree_digest(&source, platform, cancel, true)?;
        items.push(Item {
            target: downloads.join(&name),
            name,
            source,
            identity,
            digest,
            published: false,
            number: 1,
            staging: None,
        });
    }
    let intent = Intent {
        version: 1,
        manifest,
        items,
    };
    atomic_record(&work.join(INTENT), &intent, platform)?;
    // 进度目录的父项也必须尽可能落盘，才能在重启后找到意图。
    if let Some(parent) = work.parent() {
        sync_dir(platform, parent)?;
    }
    finish(work, downloads, intent, platform, cancel, false)
}

fn finish(
    work: &Path,
    downloads: &Path,
    mut intent: Intent,
    platform: &dyn FileCommit,
    cancel: &Cancellation,
    recovering: bool,
) -> Result<Vec<PathBuf>, Stop> {
    for i in 0..intent.items.len() {
        cancel.check()?;
        let mut source_verified = !recovering;
        loop {
            let item = &intent.items[i];
            match platform.identity(&item.target) {
                Ok(identity) if identity == item.identity => {
                    if tree_digest(&item.target, platform, cancel, false)? != item.digest {
                        return Err(conflict());
                    }
                    cancel.published();
                    intent.items[i].published = true;
                    break;
                }
                Ok(_) if item.published => return Err(conflict()),
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(conflict()),
                Err(_) if item.published => return Err(conflict()),
                _ => {}
            }
            // 改名成功后源应消失。目标不匹配且源已不在，不能新建第二份来掩盖用户修改。
            if platform.identity(&item.source).map_err(|_| conflict())? != item.identity
                || !source_verified
                    && tree_digest(&item.source, platform, cancel, false)? != item.digest
            {
                return Err(conflict());
            }
            source_verified = true;
            match platform.rename_no_replace(&item.source, &item.target) {
                Ok(()) => {
                    cancel.published();
                    sync_dir(platform, downloads)?;
                    if let Some(parent) = item.source.parent() {
                        sync_dir(platform, parent)?;
                    }
                    intent.items[i].published = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    let item = &mut intent.items[i];
                    item.number = item.number.checked_add(1).ok_or_else(failed)?;
                    item.target = downloads.join(numbered(&item.name, item.number));
                    write_item(work, i, item, platform)?;
                }
                Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {
                    if item.source.parent().and_then(Path::parent) == Some(downloads) {
                        return Err(failed());
                    }
                    // 复制前持久登记私有容器，复制中崩溃后能够识别并重新准备它。
                    let staging = if let Some(stage) = &item.staging {
                        if platform.identity(&stage.path).map_err(|_| conflict())? != stage.identity
                        {
                            return Err(conflict());
                        }
                        let leftover = stage.path.join("item");
                        if let Ok(meta) = fs::symlink_metadata(&leftover) {
                            if meta.is_dir() && !meta.file_type().is_symlink() {
                                fs::remove_dir_all(&leftover).map_err(|_| failed())?;
                            } else {
                                fs::remove_file(&leftover).map_err(|_| failed())?;
                            }
                        }
                        stage.path.clone()
                    } else {
                        let path = downloads.join(format!(
                            ".screenferry-{}",
                            super::new_id().map_err(|_| failed())?
                        ));
                        fs::create_dir(&path).map_err(|_| failed())?;
                        let identity = platform.identity(&path).map_err(|_| failed())?;
                        sync_dir(platform, downloads)?;
                        intent.items[i].staging = Some(Stage {
                            path: path.clone(),
                            identity,
                        });
                        write_item(work, i, &intent.items[i], platform)?;
                        path
                    };
                    let staged = staging.join("item");
                    let item = &intent.items[i];
                    copy_tree(&item.source, &staged, platform, cancel)?;
                    let identity = platform.identity(&staged).map_err(|_| failed())?;
                    if tree_digest(&staged, platform, cancel, false)? != item.digest {
                        return Err(failed());
                    }
                    sync_dir(platform, &staging)?;
                    sync_dir(platform, downloads)?;
                    intent.items[i].source = staged;
                    intent.items[i].identity = identity;
                    // 记录切换到目标卷的身份之后，才能把 staging 公开。
                    write_item(work, i, &intent.items[i], platform)?;
                }
                Err(_) => {
                    return Err(Stop::Fail(
                        if intent.items.iter().any(|item| item.published) {
                            "部分文件已保存到下载文件夹；内容与恢复记录均保留，请重连后继续"
                        } else {
                            "文件提交未完成；内容与恢复记录均保留，请重连后继续"
                        }
                        .into(),
                    ));
                }
            }
        }
    }
    let saved: Vec<_> = intent
        .items
        .iter()
        .map(|item| item.target.clone())
        .collect();
    let receipt = Receipt {
        version: 1,
        manifest: intent.manifest,
        saved: saved.clone(),
        completed: now(),
    };
    atomic_record(&work.join(RECEIPT), &receipt, platform)?;
    // 只删除工作缓存和已用空的 staging 容器，不按公开的 target 路径删除任何内容。
    let _ = fs::remove_dir_all(work.join("data"));
    for item in intent.items {
        if let Some(stage) = item.staging {
            let _ = fs::remove_dir(stage.path);
        }
    }
    fs::remove_file(work.join(INTENT)).map_err(|_| failed())?;
    let _ = fs::remove_dir_all(work.join("commit-items"));
    sync_dir(platform, work)?;
    Ok(saved)
}

fn write_item(
    work: &Path,
    index: usize,
    item: &Item,
    platform: &dyn FileCommit,
) -> Result<(), Stop> {
    let dir = work.join("commit-items");
    fs::create_dir_all(&dir).map_err(|_| failed())?;
    atomic_record(&dir.join(format!("{index}.json")), item, platform)?;
    sync_dir(platform, work)
}

fn load_items(work: &Path, intent: &mut Intent) -> Result<(), Stop> {
    for (index, item) in intent.items.iter_mut().enumerate() {
        if let Some(bytes) = read(&work.join("commit-items").join(format!("{index}.json")))? {
            let replacement: Item = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
            if replacement.name != item.name || replacement.digest != item.digest {
                return Err(conflict());
            }
            *item = replacement;
        }
    }
    Ok(())
}

pub(super) fn receipt(work: &Path, manifest: &str) -> Result<Option<Vec<PathBuf>>, Stop> {
    let Some(bytes) = read(&work.join(RECEIPT))? else {
        return Ok(None);
    };
    // 第一批修复留下的回执仍可读。
    if let Ok((found, saved)) = serde_json::from_slice::<(String, Vec<PathBuf>)>(&bytes) {
        return if found == manifest {
            cleanup_finished(work);
            Ok(Some(saved))
        } else {
            Err(conflict())
        };
    }
    let receipt: Receipt = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
    if receipt.version != 1 || receipt.manifest != manifest {
        return Err(conflict());
    }
    cleanup_finished(work);
    Ok(Some(receipt.saved))
}

fn cleanup_finished(work: &Path) {
    // 回执已持久化，即使进程在删除意图前退出，也不能永远把完成项留在未完成集合。
    if let Ok(Some(bytes)) = read(&work.join(INTENT))
        && let Ok(mut intent) = serde_json::from_slice::<Intent>(&bytes)
        && load_items(work, &mut intent).is_ok()
    {
        for item in intent.items {
            if let Some(dir) = item.source.parent()
                && dir.parent() == item.target.parent()
                && dir
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(".screenferry-"))
            {
                let _ = fs::remove_dir(dir); // 仅清理空容器。
            }
        }
    }
    let _ = fs::remove_dir_all(work.join("data"));
    let _ = fs::remove_dir_all(work.join("commit-items"));
    let _ = fs::remove_file(work.join(INTENT));
}

fn single_name(path: &Path) -> bool {
    let mut parts = path.components();
    matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
}

pub(super) fn completed_at(work: &Path) -> Option<u64> {
    let bytes = read(&work.join(RECEIPT)).ok()??;
    serde_json::from_slice::<Receipt>(&bytes)
        .ok()
        .map(|r| r.completed)
        .or_else(|| {
            serde_json::from_slice::<(String, Vec<PathBuf>)>(&bytes).ok()?;
            fs::metadata(work.join(RECEIPT))
                .ok()?
                .modified()
                .ok()?
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|d| d.as_secs())
        })
}

/// 仅在确认还没有发布任何目标时撤销私有意图和临时复制；从不删除公开目标。
pub(super) fn abandon(work: &Path, platform: &dyn FileCommit) -> Result<bool, Stop> {
    let Some(bytes) = read(&work.join(INTENT))? else {
        return Ok(true);
    };
    let mut intent: Intent = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
    load_items(work, &mut intent)?;
    for item in &intent.items {
        if item.published
            || platform
                .identity(&item.target)
                .is_ok_and(|id| id == item.identity)
            || platform.identity(&item.source).is_err()
        {
            return Ok(false);
        }
    }
    for item in intent.items {
        if let Some(stage) = item.staging {
            if stage
                .path
                .file_name()
                .is_none_or(|n| !n.to_string_lossy().starts_with(".screenferry-"))
                || stage.path.parent() != item.target.parent()
                || platform.identity(&stage.path).map_err(|_| conflict())? != stage.identity
            {
                return Err(conflict());
            }
            fs::remove_dir_all(stage.path).map_err(|_| failed())?;
            continue;
        }
        if !item.source.starts_with(work) {
            let Some(dir) = item.source.parent() else {
                return Err(conflict());
            };
            if dir
                .file_name()
                .is_none_or(|n| !n.to_string_lossy().starts_with(".screenferry-"))
                || item.source.file_name().is_none_or(|n| n != "item")
                || platform.identity(&item.source).map_err(|_| conflict())? != item.identity
            {
                return Err(conflict());
            }
            fs::remove_dir_all(dir).map_err(|_| failed())?;
        }
    }
    fs::remove_file(work.join(INTENT)).map_err(|_| failed())?;
    Ok(true)
}

fn atomic_record(
    path: &Path,
    value: &impl Serialize,
    platform: &dyn FileCommit,
) -> Result<(), Stop> {
    let bytes = serde_json::to_vec(value).map_err(|_| failed())?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(failed());
    }
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).map_err(|_| failed())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| failed())?;
    drop(file);
    platform.replace_record(&tmp, path).map_err(|_| failed())?;
    sync_dir(platform, path.parent().ok_or_else(failed)?)
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, Stop> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failed()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| failed())?;
    if bytes.len() as u64 > MAX_RECORD_BYTES {
        return Err(conflict());
    }
    Ok(Some(bytes))
}

fn sync_dir(platform: &dyn FileCommit, path: &Path) -> Result<(), Stop> {
    match platform.sync_directory(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::Unsupported => Ok(()),
        Err(_) => Err(failed()),
    }
}

fn tree_digest(
    path: &Path,
    platform: &dyn FileCommit,
    cancel: &Cancellation,
    sync: bool,
) -> Result<String, Stop> {
    fn visit(
        path: &Path,
        hash: &mut Context,
        platform: &dyn FileCommit,
        cancel: &Cancellation,
        sync: bool,
        count: &mut usize,
    ) -> Result<(), Stop> {
        cancel.check()?;
        *count += 1;
        if *count > super::manifest::MAX_ENTRIES {
            return Err(conflict());
        }
        let meta = fs::symlink_metadata(path).map_err(|_| conflict())?;
        if meta.file_type().is_symlink() {
            return Err(conflict());
        }
        if meta.is_dir() {
            hash.update(&[0]);
            let mut children = Vec::new();
            for child in fs::read_dir(path).map_err(|_| conflict())? {
                cancel.check()?;
                if children.len() + *count > super::manifest::MAX_ENTRIES {
                    return Err(conflict());
                }
                children.push(child.map_err(|_| conflict())?);
            }
            children.sort_by_key(|entry| entry.file_name());
            hash.update(&(children.len() as u64).to_le_bytes());
            for child in children {
                let name = child.file_name();
                let name = name.to_str().ok_or_else(conflict)?;
                hash.update(&(name.len() as u64).to_le_bytes());
                hash.update(name.as_bytes());
                visit(&child.path(), hash, platform, cancel, sync, count)?;
            }
            if sync {
                sync_dir(platform, path)?;
            }
        } else if meta.is_file() {
            hash.update(&[1]);
            hash.update(&meta.len().to_le_bytes());
            // Windows FlushFileBuffers 需要可写句柄；这里只对本程序的接收缓存请求同步。
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(sync)
                .open(path)
                .map_err(|_| conflict())?;
            let mut buf = vec![0; 64 * 1024];
            loop {
                cancel.check()?;
                let n = file.read(&mut buf).map_err(|_| conflict())?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
            }
            if sync {
                file.sync_all().map_err(|_| failed())?;
            }
        } else {
            return Err(conflict());
        }
        Ok(())
    }
    let mut hash = Context::new(&SHA256);
    visit(path, &mut hash, platform, cancel, sync, &mut 0)?;
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn copy_tree(
    from: &Path,
    to: &Path,
    platform: &dyn FileCommit,
    cancel: &Cancellation,
) -> Result<(), Stop> {
    cancel.check()?;
    let meta = fs::symlink_metadata(from).map_err(|_| failed())?;
    if meta.file_type().is_symlink() {
        return Err(conflict());
    }
    if meta.is_dir() {
        fs::create_dir(to).map_err(|_| failed())?;
        for child in fs::read_dir(from).map_err(|_| failed())? {
            let child = child.map_err(|_| failed())?;
            copy_tree(&child.path(), &to.join(child.file_name()), platform, cancel)?;
        }
        sync_dir(platform, to)?;
    } else if meta.is_file() {
        let mut source = fs::File::open(from).map_err(|_| failed())?;
        let mut target = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)
            .map_err(|_| failed())?;
        let mut buf = vec![0; 64 * 1024];
        loop {
            cancel.check()?;
            let n = source.read(&mut buf).map_err(|_| failed())?;
            if n == 0 {
                break;
            }
            target.write_all(&buf[..n]).map_err(|_| failed())?;
        }
        if let Ok(modified) = meta.modified() {
            let _ = target.set_modified(modified);
        }
        target.sync_all().map_err(|_| failed())?;
    } else {
        return Err(conflict());
    }
    Ok(())
}

fn failed() -> Stop {
    Stop::Fail("无法持久记录文件提交；内容与恢复记录已保留，请检查磁盘后重连".into())
}
fn conflict() -> Stop {
    Stop::Fail("已提交或待提交文件被移动、替换或修改；为避免覆盖已保留内容，请人工检查".into())
}
pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

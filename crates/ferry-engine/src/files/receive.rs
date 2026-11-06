//! 接收方在文件连接上的流程：收清单 → 算续传点 → 边收边写、逐个校验 → 放到下载文件夹。
//!
//! 收到的内容先写进 `partial/<传输号>/data/`，每校验完一个文件就把进度记进 `state`。
//! 全部收好后才把顶层项目移到下载文件夹，用户不会看到写了一半的文件。

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use ferry_core::file_name::clean_path;
use ferry_net::Framed;
use ferry_platform::FileCommit;
use ferry_proto::limits::MAX_CHUNK_LEN;
use ferry_proto::{EntryKind, FileEntry, FileOffer, FileResume, Message};
use ring::digest::{Context, SHA256};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::manifest::MAX_ENTRIES;
use super::send::{recv, send};
use super::{Cancellation, Progress, Stop, commit};

/// 两条清单消息、两段内容之间最多等多久。
const WAIT: Duration = Duration::from_secs(60);
/// 限速可低至1字节/秒且预算由最多64笔任务共享，内容等待不能把正常排队当断线。
const CONTENT_WAIT: Duration = Duration::from_secs(300);

/// 进度文件的格式标记。
const STATE_TAG: &str = "screenferry-partial-1";

pub(super) async fn run<S>(
    framed: &mut Framed<S>,
    offer: &FileOffer,
    partial: &Path,
    downloads: &Path,
    progress: &mut Progress,
    commit: Arc<dyn FileCommit>,
    cancel: &Cancellation,
) -> Result<Vec<PathBuf>, Stop>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let entries = receive_list(framed, cancel).await?;
    let (paths, tops) = check_paths(&entries)?;
    let bytes = entries
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .try_fold(0u64, |total, entry| total.checked_add(entry.size))
        .ok_or_else(|| Stop::Fail("文件总大小超过支持范围".into()))?;
    if bytes != offer.bytes
        || entries.iter().filter(|e| e.kind == EntryKind::File).count() != offer.files as usize
        || tops.len() != offer.items as usize
    {
        return Err(Stop::Fail("文件清单与通告的数量或大小不符".into()));
    }

    let work = partial.join(offer.id.to_string());
    let data = work.join("data");
    let fingerprint = fingerprint(&entries);
    let recovered = {
        let (work, downloads, fingerprint, platform, cancel) = (
            work.clone(),
            downloads.to_owned(),
            fingerprint.clone(),
            commit.clone(),
            cancel.clone(),
        );
        tokio::task::spawn_blocking(move || match commit::receipt(&work, &fingerprint)? {
            Some(saved) => Ok(Some(saved)),
            None => commit::recover(&work, &downloads, &fingerprint, &*platform, &cancel),
        })
        .await
        .map_err(|_| Stop::Fail("恢复文件提交时出错".into()))??
    };
    if let Some(saved) = recovered {
        cancel.commit()?;
        let index = u32::try_from(entries.len()).expect("条目数有上限");
        send(
            framed,
            &Message::FileResume(FileResume { index, offset: 0 }),
            cancel,
        )
        .await?;
        send(framed, &Message::FileDone, cancel).await?;
        return Ok(saved);
    }
    let mut completed = match read_state(&work, &fingerprint).await {
        Some(completed) => completed.min(entries.len()),
        None => {
            // 没收过，或者清单变了：从头开始。
            let _ = tokio::fs::remove_dir_all(&work).await;
            0
        }
    };
    // 缓存可被清理；不能仅凭进度文件跳过已经不在或变短的文件。
    for (entry, path) in entries[..completed].iter().zip(&paths) {
        if entry.kind == EntryKind::File
            && tokio::fs::metadata(data.join(path))
                .await
                .map_or(true, |meta| !meta.is_file() || meta.len() != entry.size)
        {
            completed = 0;
            break;
        }
    }
    let offset = match entries.get(completed) {
        Some(entry) if entry.kind == EntryKind::File => {
            tokio::fs::metadata(data.join(&paths[completed]))
                .await
                .map_or(0, |m| m.len().min(entry.size))
        }
        _ => 0,
    };

    // 先建好所有文件夹，空文件夹也能原样带过去。
    for (entry, path) in entries.iter().zip(&paths) {
        cancel.check()?;
        if entry.kind == EntryKind::Dir {
            create_dir(&data.join(path)).await?;
        }
    }
    create_dir(&data).await?;
    // 收第一个文件之前就记下进度：大文件收到一半断开，下次也认得出这份清单，接着往下收。
    write_state(&work, &fingerprint, completed).await?;

    let index = u32::try_from(completed).expect("条目数有上限");
    send(
        framed,
        &Message::FileResume(FileResume { index, offset }),
        cancel,
    )
    .await?;
    let before: u64 = entries[..completed]
        .iter()
        .filter(|e| e.kind == EntryKind::File)
        .map(|e| e.size)
        .sum();
    progress.start(before + offset);

    for i in completed..entries.len() {
        let entry = &entries[i];
        if entry.kind == EntryKind::Dir {
            continue;
        }
        let target = data.join(&paths[i]);
        if let Some(parent) = target.parent() {
            create_dir(parent).await?;
        }
        let from = if i == completed { offset } else { 0 };
        receive_file(framed, i, entry, &target, from, progress, cancel).await?;
        write_state(&work, &fingerprint, i + 1).await?;
    }

    let saved = place(
        data.clone(),
        tops,
        Placement {
            downloads: downloads.to_owned(),
            work,
            fingerprint,
            commit,
            cancel: cancel.clone(),
        },
    )
    .await?;
    // 完成回执保留下来；最终确认丢失后，同一个任务只能重放确认。
    let _ = tokio::fs::remove_dir_all(data).await;
    // 本机已成功提交，确认发不出去也不能把它变回“中断”。发送方重试时按回执重放。
    let _ = send(framed, &Message::FileDone, cancel).await;
    Ok(saved)
}

async fn receive_list<S>(
    framed: &mut Framed<S>,
    cancel: &Cancellation,
) -> Result<Vec<FileEntry>, Stop>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut entries = Vec::new();
    loop {
        let Message::FileList(list) = recv(framed, WAIT, cancel).await? else {
            return Err(Stop::Fail("对方没有先发清单".into()));
        };
        if list.entries.is_empty() && !list.last {
            return Err(Stop::Fail("清单没有取得进展".into()));
        }
        entries.extend(list.entries);
        if entries.len() > MAX_ENTRIES {
            return Err(Stop::Fail("清单里的条目太多".into()));
        }
        if list.last {
            return Ok(entries);
        }
    }
}

/// 校验并清洗清单里的路径，返回每个条目的本机相对路径，以及按出现顺序排列的顶层名字。
fn check_paths(entries: &[FileEntry]) -> Result<(Vec<PathBuf>, Vec<String>), Stop> {
    let mut paths = Vec::with_capacity(entries.len());
    let mut tops: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut top_seen = HashSet::new();
    for entry in entries {
        let parts = clean_path(&entry.path).ok_or_else(|| Stop::Fail("清单里有非法路径".into()))?;
        if !seen.insert(parts.join("/").to_lowercase()) {
            return Err(Stop::Fail("清单里有重复的路径".into()));
        }
        if top_seen.insert(parts[0].clone()) {
            tops.push(parts[0].clone());
        }
        paths.push(parts.iter().collect());
    }
    if paths.is_empty() {
        return Err(Stop::Fail("清单是空的".into()));
    }
    Ok((paths, tops))
}

/// 收一个文件从 `from` 开始的内容，校验整个文件的 SHA-256。
async fn receive_file<S>(
    framed: &mut Framed<S>,
    index: usize,
    entry: &FileEntry,
    target: &Path,
    from: u64,
    progress: &mut Progress,
    cancel: &Cancellation,
) -> Result<(), Stop>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let fail = |_e: io::Error| Stop::Fail("无法读写接收缓存，请检查空间和写入权限".into());
    let mut hash = Context::new(&SHA256);
    let mut file = if from > 0 {
        // 续传：已收到的部分先读一遍算校验和，再接着往后写。
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(target)
            .await
            .map_err(fail)?;
        file.set_len(from).await.map_err(fail)?;
        let mut buf = vec![0u8; MAX_CHUNK_LEN];
        let mut read = 0;
        while read < from {
            cancel.check()?;
            let want = usize::try_from(from - read).map_or(buf.len(), |r| r.min(buf.len()));
            let n = file.read(&mut buf[..want]).await.map_err(fail)?;
            if n == 0 {
                return Err(Stop::Fail("已收到的文件不完整".into()));
            }
            hash.update(&buf[..n]);
            read += n as u64;
        }
        file
    } else {
        File::create(target).await.map_err(fail)?
    };

    let mut got = from;
    while got < entry.size {
        let Message::FileData { data } = recv(framed, CONTENT_WAIT, cancel).await? else {
            return Err(Stop::Fail("文件内容不完整".into()));
        };
        if data.is_empty() {
            return Err(Stop::Fail("文件内容没有取得进展".into()));
        }
        if got + data.len() as u64 > entry.size {
            return Err(Stop::Fail("收到的文件内容超过清单大小".into()));
        }
        hash.update(&data);
        file.write_all(&data).await.map_err(fail)?;
        got += data.len() as u64;
        progress.add(data.len() as u64);
    }
    file.flush().await.map_err(fail)?;

    let Message::FileEnd(end) = recv(framed, WAIT, cancel).await? else {
        return Err(Stop::Fail("文件没有结束标记".into()));
    };
    if usize::try_from(end.index).ok() != Some(index) {
        return Err(Stop::Fail("文件顺序对不上".into()));
    }
    if hash.finish().as_ref() != end.sha256 {
        return Err(Stop::Fail("文件校验不一致，内容可能损坏".into()));
    }
    if entry.modified > 0 {
        let when = UNIX_EPOCH + Duration::from_millis(entry.modified);
        let file = file.into_std().await;
        // 修改时间设不上不影响内容。
        let _ = tokio::task::spawn_blocking(move || file.set_modified(when)).await;
    }
    Ok(())
}

struct Placement {
    downloads: PathBuf,
    work: PathBuf,
    fingerprint: String,
    commit: Arc<dyn FileCommit>,
    cancel: Cancellation,
}

/// 阻塞提交等待到结束，取消不能遗留仍在写盘的分离线程。
async fn place(
    data: PathBuf,
    tops: Vec<String>,
    placement: Placement,
) -> Result<Vec<PathBuf>, Stop> {
    tokio::task::spawn_blocking(move || {
        let Placement {
            downloads,
            work,
            fingerprint,
            commit: platform,
            cancel,
        } = placement;
        commit::publish(
            &data,
            tops,
            &downloads,
            &work,
            fingerprint,
            &*platform,
            &cancel,
        )
    })
    .await
    .map_err(|_| Stop::Fail("放置收到的文件时出错".into()))?
}

async fn create_dir(dir: &Path) -> Result<(), Stop> {
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|_| Stop::Fail("无法创建接收文件夹，请检查空间和写入权限".into()))
}

/// 清单的指纹：路径、类型、大小、修改时间都相同，才能接着上次的进度收。
fn fingerprint(entries: &[FileEntry]) -> String {
    let mut hash = Context::new(&SHA256);
    for entry in entries {
        hash.update(entry.path.as_bytes());
        hash.update(&[0, u8::from(entry.kind == EntryKind::Dir)]);
        hash.update(&entry.size.to_le_bytes());
        hash.update(&entry.modified.to_le_bytes());
    }
    hash.finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 上次收到第几个条目为止。没有进度、或者清单变了时为 `None`。
async fn read_state(work: &Path, fingerprint: &str) -> Option<usize> {
    let text = tokio::fs::read_to_string(work.join("state")).await.ok()?;
    let mut parts = text.split_whitespace();
    if parts.next()? != STATE_TAG || parts.next()? != fingerprint {
        return None;
    }
    parts.next()?.parse().ok()
}

async fn write_state(work: &Path, fingerprint: &str, completed: usize) -> Result<(), Stop> {
    let text = format!("{STATE_TAG} {fingerprint} {completed}\n");
    tokio::fs::write(work.join("state"), text)
        .await
        .map_err(|_| Stop::Fail("无法记录接收进度".into()))
}

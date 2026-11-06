//! 运行日志与诊断文件（ADR 0012）。
//!
//! - 读运行日志给设置界面看：第一次取最后一段，之后只取新写的；去掉敏感内容的标记，照原样显示。
//! - 诊断文件：开头是版本、系统、设备和设置，后面是两卷运行日志；摘要只带最后 200 行，能直接贴进问题描述。
//!   设备名、文件名和用户目录换成代号（ferry-core 的 `Redactor`），局域网地址留着。
//!   以前的版本写的行没有标记、换不干净，不带上。

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};

use ferry_platform::FileCommit;
use ring::digest::{SHA256, digest};
use serde::{Deserialize, Serialize};
use std::path::Path;

use ferry_core::Redactor;
use ferry_ipc::{RunLogLevel, RunLogLine};
use ferry_types::mark;

/// 第一次读运行日志时最多取最后这么多行。
const TAIL_LINES: usize = 500;
/// 一次最多读这么多字节。新写的超过这么多时只取最后这一段，界面整个替换。
// JSON最坏转义为6倍，另限制每页行数，确保落在IPC的1MiB单行上限内。
const MAX_CHUNK: usize = 128 * 1024;
const PAGE_LINES: usize = 1000;
/// 摘要里运行日志的行数：GitHub 的问题描述最多 6.5 万字，要贴得下。
const SUMMARY_LINES: usize = 200;
/// 摘要通过一条JSON消息返回；为最坏的6倍转义和字段开销留出空间。
const SUMMARY_BYTES: usize = 64 * 1024;
const SUMMARY_TRUNCATED: &str = "（诊断摘要已截短，完整日志请导出诊断文件）";

/// 读到的一段运行日志；仅保存界面需要的有界分页数据。
#[derive(Default)]
pub(crate) struct Chunk {
    pub(crate) lines: Vec<RunLogLine>,
    pub(crate) next: u64,
    pub(crate) fresh: bool,
    pub(crate) bytes: u64,
    /// 当前游标累计读取的完整行数；首次尾读不扫描整卷。
    pub(crate) lines_total: u64,
    pub(crate) cursor: String,
    pub(crate) more: bool,
    pub(crate) error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    version: u8,
    identity: String,
    offset: u64,
    lines: u64,
    anchor: String,
    /// 超长行被分段跳过时，下一页继续丢弃它，直到换行。
    skipping: bool,
}

fn anchor(file: &mut File, end: u64) -> std::io::Result<String> {
    let start = end.saturating_sub(256);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(digest(&SHA256, &bytes)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 每次只 seek/read 至多 MAX_CHUNK 字节。文件身份与已读尾部摘要一起防止轮转/截断误用游标。
/// 不完整的末行留到下次，超长行明确跳过，避免无限重复读取同一段。
pub(crate) fn read(
    path: &Path,
    since: Option<u64>,
    cursor: Option<&str>,
    files: &dyn FileCommit,
) -> Chunk {
    match read_inner(path, since, cursor, files) {
        Ok(chunk) => chunk,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Chunk {
            fresh: true,
            ..Chunk::default()
        },
        Err(e) => Chunk {
            error: Some(format!("读取运行日志失败：{e}")),
            ..Chunk::default()
        },
    }
}

fn read_inner(
    path: &Path,
    since: Option<u64>,
    token: Option<&str>,
    files: &dyn FileCommit,
) -> std::io::Result<Chunk> {
    let mut file = File::open(path)?;
    let identity = files.identity_open_file(&file)?;
    let bytes = file.metadata()?.len();
    let prior = token
        .filter(|s| s.len() <= 2048)
        .and_then(|s| serde_json::from_str::<Cursor>(s).ok())
        .filter(|c| c.version == 1 && c.identity == identity && c.offset <= bytes)
        .filter(|c| anchor(&mut file, c.offset).is_ok_and(|a| a == c.anchor));
    let legacy = token
        .is_none()
        .then_some(since)
        .flatten()
        .filter(|s| *s <= bytes);
    let fresh = prior.is_none() && legacy.is_none();
    let start = prior
        .as_ref()
        .map(|c| c.offset)
        .or(legacy)
        .unwrap_or_else(|| bytes.saturating_sub(MAX_CHUNK as u64));
    let old_lines = prior.as_ref().map_or(0, |c| c.lines);
    let skipping = prior.as_ref().is_some_and(|c| c.skipping) || (fresh && start > 0);
    file.seek(SeekFrom::Start(start))?;
    let length = (bytes - start).min(MAX_CHUNK as u64) as usize;
    let mut data = vec![0; length];
    file.read_exact(&mut data)?;
    let from = if skipping {
        data.iter().position(|b| *b == b'\n').map(|i| i + 1)
    } else {
        Some(0)
    };
    let mut limited = false;
    let mut next = start;
    let mut skipped = skipping;
    let mut lines = Vec::new();
    let mut count = old_lines;
    if let Some(from) = from {
        skipped = false;
        let mut end = data[from..]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(from, |i| from + i + 1);
        if !fresh
            && let Some((position, _)) = data[from..end]
                .iter()
                .enumerate()
                .filter(|(_, b)| **b == b'\n')
                .nth(PAGE_LINES - 1)
        {
            let boundary = from + position + 1;
            limited = boundary < end;
            end = boundary;
        }
        let text = String::from_utf8_lossy(&data[from..end]);
        count =
            count.saturating_add(data[from..end].iter().filter(|b| **b == b'\n').count() as u64);
        lines = text.lines().filter(|l| !l.is_empty()).map(parse).collect();
        next = start + end as u64;
        if fresh && lines.len() > TAIL_LINES {
            lines.drain(..lines.len() - TAIL_LINES);
        }
        if end == from && length == MAX_CHUNK && from == 0 {
            // 生产日志writer有行长度上限；仍可安全处理旧日志或外部写入的超长行。
            next = start + length as u64;
            skipped = true;
            lines.push(RunLogLine {
                at: String::new(),
                level: RunLogLevel::Warn,
                text: "已跳过超过 128 KiB 的运行日志行".into(),
            });
        }
    } else if length == MAX_CHUNK {
        next = start + length as u64;
    }
    let more = next > start && next < bytes && (length == MAX_CHUNK || limited);
    let cursor = Cursor {
        version: 1,
        identity,
        offset: next,
        lines: count,
        anchor: anchor(&mut file, next)?,
        skipping: skipped,
    };
    Ok(Chunk {
        lines,
        next,
        fresh,
        bytes,
        lines_total: count,
        cursor: serde_json::to_string(&cursor).expect("日志游标可序列化"),
        more,
        error: None,
    })
}

/// 解析运行日志的一行，去掉敏感内容的标记。
///
/// 现在的格式是 `2026-11-02 06:12:03.104  INFO 内容`；以前的版本写的是启动后的秒数
/// （`   0.096268792s  INFO 内容`），没有时间。
fn parse(line: &str) -> RunLogLine {
    let (at, rest) = match line.get(..23) {
        Some(stamp) if is_stamp(stamp) => (stamp[11..19].to_owned(), &line[23..]),
        _ => {
            let rest = line.trim_start();
            let rest = match rest.split_once(' ') {
                Some((uptime, tail)) if is_uptime(uptime) => tail,
                _ => rest,
            };
            (String::new(), rest)
        }
    };
    let rest = rest.trim_start();
    let (level, text) = match rest.split_once(' ') {
        Some(("INFO", text)) => (RunLogLevel::Info, text),
        Some(("WARN", text)) => (RunLogLevel::Warn, text),
        Some(("ERROR", text)) => (RunLogLevel::Error, text),
        Some(("DEBUG" | "TRACE", text)) => (RunLogLevel::Debug, text),
        _ => (RunLogLevel::Info, rest),
    };
    RunLogLine {
        at,
        level,
        text: mark::strip(text.trim_start()).into_owned(),
    }
}

/// `2026-11-02 06:12:03.104`
fn is_stamp(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 23
        && b.iter().enumerate().all(|(i, &c)| match i {
            4 | 7 => c == b'-',
            10 => c == b' ',
            13 | 16 => c == b':',
            19 => c == b'.',
            _ => c.is_ascii_digit(),
        })
}

/// `0.096268792s`
fn is_uptime(text: &str) -> bool {
    text.strip_suffix('s')
        .is_some_and(|n| !n.is_empty() && n.parse::<f64>().is_ok())
}

/// 诊断文件或摘要：`header` 是开头的若干行（已经换好代号），后面跟运行日志。
/// `logs` 从旧到新；`summary` 为真时只带最后 [`SUMMARY_LINES`] 行。
pub(crate) fn report(
    title: &str,
    header: &[(&str, String)],
    logs: &[&Path],
    redactor: &mut Redactor,
    summary: bool,
) -> String {
    let mut out = format!("{title}\n\n");
    let width = header
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0);
    // 标签是汉字，用全角空格补齐，等宽字体里才对得齐。
    for (label, value) in header {
        let pad = "\u{3000}".repeat(width - label.chars().count());
        let mut rows = value.lines();
        out.push_str(&format!("{label}{pad}  {}\n", rows.next().unwrap_or("")));
        for row in rows {
            out.push_str(&format!("{}  {row}\n", "\u{3000}".repeat(width)));
        }
    }

    let mut text = String::new();
    let mut names = Vec::new();
    for path in logs {
        if let Ok(data) = fs::read(path) {
            text.push_str(&String::from_utf8_lossy(&data));
            if let Some(name) = path.file_name() {
                names.push(name.to_string_lossy().into_owned());
            }
        }
    }
    // 以前的版本写的行没有时间、也没有给设备名和文件名加标记，换不干净，不带上。
    let all = text.lines().filter(|l| !l.is_empty());
    let (lines, old): (Vec<&str>, Vec<&str>) = all.partition(|l| l.get(..23).is_some_and(is_stamp));
    let shown = if summary {
        &lines[lines.len().saturating_sub(SUMMARY_LINES)..]
    } else {
        &lines[..]
    };
    let source = if names.is_empty() {
        "没有找到运行日志".to_owned()
    } else {
        names.join("、")
    };
    out.push_str(&format!(
        "\n—— 运行日志：{source}，最后 {} 行 ——\n",
        shown.len()
    ));
    if !old.is_empty() {
        out.push_str(&format!(
            "（以前的版本写的 {} 行没有带上：它们没有给设备名、文件名加标记）\n",
            old.len()
        ));
    }
    let mut rendered = Vec::new();
    let mut total = 0usize;
    for line in shown {
        let mut line = redactor.line(line);
        if summary && line.len() > SUMMARY_BYTES / 2 {
            truncate_utf8(&mut line, SUMMARY_BYTES / 2);
            line.push_str(" …（本行已截短）");
        }
        total = total.saturating_add(line.len() + 1);
        rendered.push(line);
    }
    if summary {
        // 标题与配置最多占四分之一预算；运行日志优先保留最近完整行。
        let header_truncated = out.len() > SUMMARY_BYTES / 4;
        if header_truncated {
            truncate_utf8(&mut out, SUMMARY_BYTES / 4);
            out.push('\n');
        }
        let budget = SUMMARY_BYTES.saturating_sub(out.len() + SUMMARY_TRUNCATED.len() + 2);
        let mut start = 0;
        while total > budget && start < rendered.len() {
            total = total.saturating_sub(rendered[start].len() + 1);
            start += 1;
        }
        if header_truncated || start > 0 || shown.iter().any(|line| line.len() > SUMMARY_BYTES / 2)
        {
            out.push_str(SUMMARY_TRUNCATED);
            out.push('\n');
        }
        for line in &rendered[start..] {
            out.push_str(line);
            out.push('\n');
        }
    } else {
        for line in rendered {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

fn truncate_utf8(text: &mut String, maximum: usize) {
    let mut end = maximum.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}

/// Unix 毫秒加上时区，写成本机时间：`(2026-11-02 06:24:13 +08:00, 20261102-0624)`。
pub(crate) fn local_time(unix_ms: i64, offset_secs: i32) -> (String, String) {
    let secs = unix_ms.div_euclid(1000) + i64::from(offset_secs);
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rest / 3600, rest % 3600 / 60, rest % 60);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let off = offset_secs.unsigned_abs();
    (
        format!(
            "{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02} {sign}{:02}:{:02}",
            off / 3600,
            off % 3600 / 60
        ),
        format!("{y:04}{m:02}{d:02}-{hh:02}{mm:02}"),
    )
}

/// 1970-01-01 之后的天数换成年月日（公历）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

//! 制盘后检查只读挂载中的应用。只挂载本任务的新映像，所有退出路径都尝试卸载。

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{HELPER, Result, io, run_in, step};

pub(super) fn verify_app(root: &Path, app: &Path) -> Result {
    for path in [app.to_owned(), app.join("Contents").join(HELPER)] {
        if !path.is_dir() {
            return Err(format!("安装盘缺少应用：{}", path.display()));
        }
        run_in(
            root,
            "/usr/bin/codesign",
            &[
                "--verify",
                "--deep",
                "--strict",
                path.to_str().ok_or("应用路径不是 UTF-8")?,
            ],
        )?;
    }
    Ok(())
}

pub(super) fn verify_image(root: &Path, image: &Path, app_name: &OsStr) -> Result {
    step("只读挂载安装盘，严格验证应用与设置界面签名");
    let mut mounted = MountedImage::new(root)?;
    // 在调用前启用清理：hdiutil 即便报错，也可能已经完成部分挂载。
    mounted.attached = true;
    let attach = Command::new("/usr/bin/hdiutil")
        .args([
            "attach",
            "-readonly",
            "-nobrowse",
            "-noautoopen",
            "-mountpoint",
        ])
        .arg(&mounted.path)
        .arg(image)
        .current_dir(root)
        .output();
    let checked = match attach {
        Ok(output) if output.status.success() => verify_app(root, &mounted.path.join(app_name)),
        Ok(output) => Err(format!(
            "只读挂载安装盘失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Err(e) => {
            mounted.attached = false;
            Err(format!("无法运行 hdiutil：{e}"))
        }
    };
    let detached = mounted.detach();
    match (checked, detached) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(format!("{error}；{cleanup}")),
    }
}

struct MountedImage {
    path: PathBuf,
    attached: bool,
}

impl MountedImage {
    fn new(root: &Path) -> Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let parent = root.join("target/bundle/macos");
        fs::create_dir_all(&parent).map_err(|e| io(&parent, e))?;
        for _ in 0..100 {
            let path = parent.join(format!(
                ".dmg-verify-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        attached: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io(&path, e)),
            }
        }
        Err("无法为安装盘验证创建空闲挂载目录".into())
    }

    fn detach(&mut self) -> Result {
        if self.attached {
            // attach 失败可能根本没有挂载，不能把空目录当成残留卷，也不能误卸载其他盘。
            if !self.is_mounted()? {
                self.attached = false;
            }
        }
        if self.attached {
            let mut last = String::new();
            for force in [false, true] {
                let mut command = Command::new("/usr/bin/hdiutil");
                command.arg("detach");
                if force {
                    command.arg("-force");
                }
                match command.arg(&self.path).output() {
                    Ok(output) if output.status.success() => {
                        self.attached = false;
                        break;
                    }
                    Ok(output) => last = String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                    Err(e) => last = e.to_string(),
                }
            }
            if self.attached {
                return Err(format!(
                    "无法卸载本次安装盘验证挂载 {}：{last}；请用 hdiutil detach 手动卸载",
                    self.path.display()
                ));
            }
        }
        // 只能移除空目录，绝不递归删除可能仍在挂载的文件系统。
        fs::remove_dir(&self.path)
            .or_else(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|e| io(&self.path, e))
    }

    fn is_mounted(&self) -> Result<bool> {
        let info = Command::new("/usr/bin/hdiutil")
            .args(["info", "-plist"])
            .output()
            .map_err(|e| format!("无法查询安装盘挂载状态：{e}"))?;
        if !info.status.success() {
            return Err("无法查询安装盘挂载状态".into());
        }
        let mut convert = Command::new("/usr/bin/plutil")
            .args(["-convert", "json", "-o", "-", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("无法解析安装盘挂载状态：{e}"))?;
        let written = convert
            .stdin
            .take()
            .ok_or("plutil 没有标准输入")?
            .write_all(&info.stdout);
        let output = convert
            .wait_with_output()
            .map_err(|e| format!("读取安装盘挂载状态失败：{e}"))?;
        written.map_err(|e| format!("传递安装盘挂载状态失败：{e}"))?;
        if !output.status.success() {
            return Err("解析安装盘挂载状态失败".into());
        }
        let info: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
        let images = info
            .get("images")
            .and_then(serde_json::Value::as_array)
            .ok_or("安装盘挂载状态没有 images")?;
        Ok(images
            .iter()
            .filter_map(|image| {
                image
                    .get("system-entities")
                    .and_then(serde_json::Value::as_array)
            })
            .flatten()
            .any(|entity| {
                entity
                    .get("mount-point")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|path| Path::new(path) == self.path)
            }))
    }
}

impl Drop for MountedImage {
    fn drop(&mut self) {
        if let Err(error) = self.detach() {
            eprintln!("安装盘验证清理失败：{error}");
        }
    }
}

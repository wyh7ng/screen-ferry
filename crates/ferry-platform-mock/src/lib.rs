//! 测试用的假平台：可注入输入事件，并记录注入结果。
//!
//! 每个类型都可以克隆，克隆体共享状态：一份交给被测代码，一份留在测试里观察。
//!
//! 供上层 crate 的场景测试使用。层级：L2。

#![deny(unsafe_code)]

mod autostart;
mod capture;
mod clipboard;
mod display;
mod event_loop;
// Windows 场景测试也必须使用真实文件 ID；FFI 仅限此模块的句柄身份读取。
#[allow(unsafe_code)]
mod file_commit;
mod inject;
mod keymap;
mod overlay;
mod permission;
mod update;

pub use autostart::MockAutostart;
pub use capture::MockCapture;
pub use clipboard::MockClipboard;
pub use display::MockDisplays;
pub use event_loop::MockEventLoop;
pub use file_commit::MockFileCommit;
pub use inject::MockInject;
pub use overlay::MockOverlay;
pub use permission::MockPermissions;
pub use update::MockUpdater;

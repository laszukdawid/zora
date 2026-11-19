//! Windows plugins for zorad (docs/protocol.md). The rules are platform-neutral and tested everywhere
//! (`sessions`, `menu`); the Win32 and WASAPI parts build only on Windows.

#[cfg(windows)]
pub mod com;
pub mod menu;
pub mod sessions;
#[cfg(windows)]
pub mod tray;
#[cfg(windows)]
pub mod wasapi;

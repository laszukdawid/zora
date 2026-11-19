//! Linux plugins for zorad. The protocol logic they share lives in `zora-plugin`; this crate holds only the
//! OS side: the PipeWire dictation monitor, D-Bus notifications and the tray. Speech is `crates/zora-listen`.

#[cfg(target_os = "linux")]
pub mod desktop;
#[cfg(target_os = "linux")]
pub mod pw;

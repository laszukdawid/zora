//! Linux plugins for zorad. The protocol logic they share lives in `zora-plugin`; this crate holds only the
//! OS side: cpal capture, the sherpa-onnx model, the PipeWire dictation monitor, D-Bus notifications and the tray.

#[cfg(target_os = "linux")]
pub mod capture;
#[cfg(target_os = "linux")]
pub mod desktop;
#[cfg(target_os = "linux")]
pub mod pw;
#[cfg(target_os = "linux")]
pub mod stt;

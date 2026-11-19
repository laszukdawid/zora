//! The plugin side of docs/protocol.md, shared by the native plugins (Linux now, Windows next). Nothing here
//! touches an OS audio, D-Bus or PipeWire API; those live in the platform crates behind the traits defined here.

pub mod clock;
pub mod dictation;
pub mod listen;
pub mod mic;
pub mod proto;
pub mod tray;

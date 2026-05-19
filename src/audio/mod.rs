//! Audio redirection via native PipeWire and ironrdp-rdpsnd.
//!
//! Captures system audio using the PipeWire stream API directly (no subprocess)
//! and sends it over the RDP audio channel. The capture runs on a dedicated thread
//! since PipeWire types are !Send.

mod backend;
mod format;
mod pipewire;

pub use backend::HyprSoundFactory;

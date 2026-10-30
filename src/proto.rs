//! Frames between Rust and the Python shim. Unit U2.
//!
//! A frame is `u32 BE total length`, `u32 BE json length`, the JSON, then an optional binary blob.

use std::io::{Read, Write};

pub struct Frame {
    pub msg: serde_json::Value,
    pub blob: Vec<u8>,
}

pub fn write_frame(w: &mut impl Write, msg: &serde_json::Value, blob: &[u8]) -> std::io::Result<()> {
    todo!("U2")
}

/// `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<Option<Frame>> {
    todo!("U2")
}

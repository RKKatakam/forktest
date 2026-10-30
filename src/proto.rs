//! Frames between Rust and the Python shim. Unit U2.
//!
//! A frame is `u32 BE total length`, `u32 BE json length`, the JSON, then an optional binary blob.
//! The total counts every byte after itself: the json length field, the JSON and the blob.

use std::io::{Error, ErrorKind, Read, Write};

const MAX_FRAME: usize = 1 << 30;

pub struct Frame {
    pub msg: serde_json::Value,
    pub blob: Vec<u8>,
}

pub fn write_frame(w: &mut impl Write, msg: &serde_json::Value, blob: &[u8]) -> std::io::Result<()> {
    let json = serde_json::to_vec(msg)?;
    let total = 4 + json.len() + blob.len();
    if total > MAX_FRAME {
        return Err(Error::new(ErrorKind::InvalidInput, format!("frame of {total} bytes")));
    }
    let mut buf = Vec::with_capacity(4 + total);
    buf.extend_from_slice(&(total as u32).to_be_bytes());
    buf.extend_from_slice(&(json.len() as u32).to_be_bytes());
    buf.extend_from_slice(&json);
    buf.extend_from_slice(blob);
    w.write_all(&buf)
}

/// `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> std::io::Result<Option<Frame>> {
    let mut head = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut head[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let total = u32::from_be_bytes(head) as usize;
    if !(4..=MAX_FRAME).contains(&total) {
        return Err(Error::new(ErrorKind::InvalidData, format!("frame length {total}")));
    }
    let mut body = vec![0u8; total];
    r.read_exact(&mut body)?;
    let json_len = u32::from_be_bytes(body[..4].try_into().unwrap()) as usize;
    if json_len > total - 4 {
        return Err(Error::new(ErrorKind::InvalidData, format!("json length {json_len} in a frame of {total}")));
    }
    let msg = serde_json::from_slice(&body[4..4 + json_len])?;
    body.drain(..4 + json_len);
    Ok(Some(Frame { msg, blob: body }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip_back_to_back_then_end_cleanly() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"type": "result", "nodeid": "t::a"}), &[1, 0, 0, 0, 7, 0, 0, 0]).unwrap();
        write_frame(&mut buf, &json!({"type": "done"}), &[]).unwrap();
        let mut r = buf.as_slice();
        let a = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(a.msg["nodeid"], "t::a");
        assert_eq!(a.blob, [1, 0, 0, 0, 7, 0, 0, 0]);
        let b = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(b.msg["type"], "done");
        assert!(b.blob.is_empty());
        assert!(read_frame(&mut r).unwrap().is_none());
    }

    #[test]
    fn a_frame_cut_short_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"type": "done"}), &[]).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }
}

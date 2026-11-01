//! Frames between Rust and the Python shim. Unit U2.
//!
//! A frame is `u32 BE total length`, `u32 BE json length`, the JSON, then an optional binary blob.
//! The total counts every byte after itself: the json length field, the JSON and the blob.

use std::io::{Error, ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

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

/// Sends one byte carrying `fds` as `SCM_RIGHTS`, so the peer gets its own copies of them.
pub fn send_fds(stream: &UnixStream, fds: &[BorrowedFd<'_>]) -> std::io::Result<()> {
    let raw: Vec<i32> = fds.iter().map(|f| f.as_raw_fd()).collect();
    let bytes = std::mem::size_of_val(raw.as_slice()) as u32;
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(bytes) } as usize];
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(cmsg).cast::<i32>(), raw.len());
    }
    loop {
        match unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) } {
            1 => return Ok(()),
            -1 if Error::last_os_error().kind() == ErrorKind::Interrupted => {}
            -1 => return Err(Error::last_os_error()),
            _ => return Err(ErrorKind::WriteZero.into()),
        }
    }
}

/// Receives the byte `send_fds` sent and the `n` descriptors it carried.
pub fn recv_fds(stream: &UnixStream, n: usize) -> std::io::Result<Vec<OwnedFd>> {
    let bytes = (n * std::mem::size_of::<i32>()) as u32;
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(bytes) } as usize];
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = control.len() as _;
    let got = loop {
        match unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) } {
            -1 if Error::last_os_error().kind() == ErrorKind::Interrupted => {}
            -1 => return Err(Error::last_os_error()),
            got => break got,
        }
    };
    let mut fds = Vec::new();
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(cmsg).cast::<i32>();
                let count = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<i32>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(data.add(i).read_unaligned()));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
    }
    if got != 1 || fds.len() != n {
        return Err(Error::new(ErrorKind::InvalidData, format!("expected {n} descriptors, got {}", fds.len())));
    }
    Ok(fds)
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
    fn descriptors_pass_to_the_peer() {
        use std::io::Read;
        use std::os::fd::AsFd;
        let (a, b) = UnixStream::pair().unwrap();
        let (mut pipe_r, pipe_w) = std::io::pipe().unwrap();
        send_fds(&a, &[pipe_w.as_fd(), pipe_w.as_fd()]).unwrap();
        drop(pipe_w);
        let fds = recv_fds(&b, 2).unwrap();
        let mut w = std::fs::File::from(fds.into_iter().next().unwrap());
        w.write_all(b"hi").unwrap();
        drop(w);
        let mut got = String::new();
        pipe_r.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hi");
    }

    #[test]
    fn a_frame_cut_short_is_an_error() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &json!({"type": "done"}), &[]).unwrap();
        buf.truncate(buf.len() - 1);
        assert!(read_frame(&mut buf.as_slice()).is_err());
    }
}

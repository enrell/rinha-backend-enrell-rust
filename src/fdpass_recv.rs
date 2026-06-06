#[path = "fdpass_common.rs"]
mod common;

use common::*;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;

unsafe extern "C" {
    fn recvmsg(fd: c_int, msg: *mut Msghdr, flags: c_int) -> isize;
}

pub fn recv_fd(stream: &UnixStream) -> io::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = Iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = [0u8; CONTROL_BYTES];
    let mut msg = Msghdr {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.as_mut_ptr().cast(),
        msg_controllen: control.len(),
        msg_flags: 0,
    };

    let received = unsafe { recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }
    if received == 0 {
        return Ok(None);
    }
    if msg.msg_controllen < cmsg_len(FD_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing fd control message",
        ));
    }

    let cmsg = control.as_ptr().cast::<Cmsghdr>();
    unsafe {
        if (*cmsg).cmsg_level != SOL_SOCKET
            || (*cmsg).cmsg_type != SCM_RIGHTS
            || (*cmsg).cmsg_len < cmsg_len(FD_BYTES)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid fd control message",
            ));
        }
        Ok(Some(
            ptr::read_unaligned(cmsg_data(cmsg.cast_mut()).cast::<c_int>()) as RawFd,
        ))
    }
}

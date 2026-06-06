#[path = "fdpass_common.rs"]
mod common;

use common::*;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::ptr;

const MSG_CTRUNC: c_int = 0x08;
const MSG_CMSG_CLOEXEC: c_int = 0x40000000;

unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
    fn recvmsg(fd: c_int, msg: *mut Msghdr, flags: c_int) -> isize;
}

pub fn recv_fd(stream: &UnixStream) -> io::Result<Option<OwnedFd>> {
    let mut byte = [0u8; 1];
    let mut iov = Iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = ControlBuffer([0u8; CONTROL_BYTES]);
    let mut msg = Msghdr {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.0.as_mut_ptr().cast(),
        msg_controllen: control.0.len(),
        msg_flags: 0,
    };

    let received = loop {
        msg.msg_controllen = control.0.len();
        msg.msg_flags = 0;

        let received = unsafe { recvmsg(stream.as_raw_fd(), &mut msg, MSG_CMSG_CLOEXEC) };
        if received >= 0 {
            break received;
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    };

    if received == 0 {
        return Ok(None);
    }
    if msg.msg_flags & MSG_CTRUNC != 0 {
        unsafe {
            close_rights_fds(
                control.0.as_mut_ptr().cast::<Cmsghdr>(),
                msg.msg_controllen.min(control.0.len()),
            );
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "truncated fd control message",
        ));
    }
    if msg.msg_controllen < cmsg_len(FD_BYTES) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing fd control message",
        ));
    }

    let cmsg = control.0.as_ptr().cast::<Cmsghdr>();
    unsafe {
        let hdr = ptr::read_unaligned(cmsg);
        if hdr.cmsg_level != SOL_SOCKET || hdr.cmsg_type != SCM_RIGHTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid fd control message",
            ));
        }
        if hdr.cmsg_len != cmsg_len(FD_BYTES) {
            close_rights_fds(cmsg.cast_mut(), msg.msg_controllen.min(control.0.len()));
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "expected exactly one fd",
            ));
        }

        let fd = ptr::read_unaligned(cmsg_data(cmsg.cast_mut()).cast::<c_int>());
        Ok(Some(OwnedFd::from_raw_fd(fd)))
    }
}

unsafe fn close_rights_fds(cmsg: *mut Cmsghdr, available_len: usize) {
    if available_len <= cmsg_len(0) {
        return;
    }

    let hdr = unsafe { ptr::read_unaligned(cmsg) };
    if hdr.cmsg_level != SOL_SOCKET || hdr.cmsg_type != SCM_RIGHTS {
        return;
    }
    if hdr.cmsg_len <= cmsg_len(0) {
        return;
    }

    let data_len = hdr.cmsg_len.min(available_len) - cmsg_len(0);
    let fd_count = data_len / FD_BYTES;
    let data = unsafe { cmsg_data(cmsg).cast::<c_int>() };
    for idx in 0..fd_count {
        let fd = unsafe { ptr::read_unaligned(data.add(idx)) };
        unsafe { close(fd) };
    }
}

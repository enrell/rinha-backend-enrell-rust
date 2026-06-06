#[path = "fdpass_common.rs"]
mod common;

use common::*;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::ptr;

unsafe extern "C" {
    fn sendmsg(fd: c_int, msg: *const Msghdr, flags: c_int) -> isize;
}

pub fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = Iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control = ControlBuffer([0u8; CONTROL_BYTES]);

    unsafe {
        let cmsg = control.0.as_mut_ptr().cast::<Cmsghdr>();
        (*cmsg).cmsg_len = cmsg_len(FD_BYTES);
        (*cmsg).cmsg_level = SOL_SOCKET;
        (*cmsg).cmsg_type = SCM_RIGHTS;
        ptr::write_unaligned(cmsg_data(cmsg).cast::<c_int>(), fd as c_int);
    }

    let msg = Msghdr {
        msg_name: ptr::null_mut(),
        msg_namelen: 0,
        msg_iov: &mut iov,
        msg_iovlen: 1,
        msg_control: control.0.as_mut_ptr().cast(),
        msg_controllen: control.0.len(),
        msg_flags: 0,
    };

    loop {
        let sent = unsafe { sendmsg(stream.as_raw_fd(), &msg, 0) };
        if sent >= 0 {
            return Ok(());
        }

        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

use std::ffi::{c_int, c_void};
use std::mem;

pub const SOL_SOCKET: c_int = 1;
pub const SCM_RIGHTS: c_int = 1;
pub const FD_BYTES: usize = mem::size_of::<c_int>();
pub const CMSGHDR_BYTES: usize = mem::size_of::<Cmsghdr>();
pub const CONTROL_BYTES: usize = cmsg_space(FD_BYTES);

#[repr(C)]
pub struct Iovec {
    pub iov_base: *mut c_void,
    pub iov_len: usize,
}

#[repr(C)]
pub struct Msghdr {
    pub msg_name: *mut c_void,
    pub msg_namelen: u32,
    pub msg_iov: *mut Iovec,
    pub msg_iovlen: usize,
    pub msg_control: *mut c_void,
    pub msg_controllen: usize,
    pub msg_flags: c_int,
}

#[repr(C)]
pub struct Cmsghdr {
    pub cmsg_len: usize,
    pub cmsg_level: c_int,
    pub cmsg_type: c_int,
}

pub const fn cmsg_align(len: usize) -> usize {
    (len + mem::size_of::<usize>() - 1) & !(mem::size_of::<usize>() - 1)
}

pub const fn cmsg_len(len: usize) -> usize {
    cmsg_align(CMSGHDR_BYTES) + len
}

pub const fn cmsg_space(len: usize) -> usize {
    cmsg_align(CMSGHDR_BYTES) + cmsg_align(len)
}

pub unsafe fn cmsg_data(cmsg: *mut Cmsghdr) -> *mut u8 {
    unsafe { cmsg.cast::<u8>().add(cmsg_align(CMSGHDR_BYTES)) }
}

#[path = "../fdpass_send.rs"]
mod fdpass_send;

use std::ffi::{c_int, c_void};
use std::mem;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

const AF_INET: c_int = 2;
const SOCK_STREAM: c_int = 1;
const IPPROTO_TCP: c_int = 6;
const TCP_NODELAY: c_int = 1;
const SO_REUSEADDR: c_int = 2;
const SOL_SOCKET: c_int = 1;
const LISTEN_BACKLOG: c_int = 65536;
const SOCK_CLOEXEC: c_int = 0x80000;
const SOCK_NONBLOCK: c_int = 0x800;

#[repr(C)]
struct SockAddrIn {
    sin_family: u16,
    sin_port: u16,
    sin_addr: u32,
    sin_zero: [u8; 8],
}

unsafe extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn bind(sockfd: c_int, addr: *const c_void, addrlen: u32) -> c_int;
    fn listen(sockfd: c_int, backlog: c_int) -> c_int;
    fn accept4(sockfd: c_int, addr: *mut c_void, addrlen: *mut u32, flags: c_int) -> c_int;
    fn setsockopt(sockfd: c_int, level: c_int, optname: c_int, optval: *const c_void, optlen: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn usleep(usec: u32) -> c_int;
}

struct Backend {
    path: String,
    stream: UnixStream,
}

fn main() {
    let listen = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9999".into());
    let port: u16 = listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(9999);

    let paths: Vec<String> = std::env::var("BACKEND_SOCKS")
        .unwrap_or_else(|_| "/sockets/api1.sock,/sockets/api2.sock".into())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    assert!(!paths.is_empty(), "BACKEND_SOCKS must list at least one socket");

    let listener = listen_tcp(port);
    let mut backends: Vec<Backend> = paths
        .into_iter()
        .map(|path| Backend {
            stream: connect_backend(&path),
            path,
        })
        .collect();

    let next = AtomicUsize::new(0);
    let n_back = backends.len();

    loop {
        let fd = unsafe {
            accept4(
                listener,
                ptr::null_mut(),
                ptr::null_mut(),
                SOCK_CLOEXEC | SOCK_NONBLOCK,
            )
        };
        if fd < 0 {
            continue;
        }
        set_nodelay(fd);

        let mut idx = next.fetch_add(1, Ordering::Relaxed) % n_back;
        let start = idx;

        loop {
            let backend = &mut backends[idx];
            match fdpass_send::send_fd(&backend.stream, fd) {
                Ok(()) => break,
                Err(_) => {
                    backend.stream = reconnect_backend(&backend.path);
                }
            }
            idx = (idx + 1) % n_back;
            if idx == start {
                unsafe { close(fd) };
                break;
            }
        }
    }
}

fn listen_tcp(port: u16) -> c_int {
    let fd = unsafe { socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        panic!("socket: {}", std::io::Error::last_os_error());
    }
    let one = 1i32;
    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            ptr::addr_of!(one).cast(),
            mem::size_of::<i32>() as u32,
        );
    }
    let addr = SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: 0,
        sin_zero: [0; 8],
    };
    if unsafe { bind(fd, ptr::addr_of!(addr).cast(), mem::size_of::<SockAddrIn>() as u32) } < 0 {
        panic!("bind: {}", std::io::Error::last_os_error());
    }
    if unsafe { listen(fd, LISTEN_BACKLOG) } < 0 {
        panic!("listen: {}", std::io::Error::last_os_error());
    }
    fd
}

fn connect_backend(path: &str) -> UnixStream {
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return stream;
        }
        std::hint::spin_loop();
    }
}

fn reconnect_backend(path: &str) -> UnixStream {
    let mut attempts = 0u32;
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return stream;
        }
        attempts = attempts.wrapping_add(1);
        if attempts > 1000 {
            attempts = 0;
            unsafe { usleep(1) };
        } else {
            std::hint::spin_loop();
        }
    }
}

fn set_nodelay(fd: c_int) {
    let one = 1i32;
    unsafe {
        setsockopt(
            fd,
            IPPROTO_TCP,
            TCP_NODELAY,
            ptr::addr_of!(one).cast(),
            mem::size_of::<i32>() as u32,
        );
    }
}

#[path = "../fdpass_send.rs"]
mod fdpass_send;

use std::ffi::{c_int, c_void};
use std::mem;
use std::net::Ipv4Addr;
use std::os::unix::net::UnixStream;
use std::ptr;
use std::thread;
use std::time::Duration;

const AF_INET: c_int = 2;
const SOCK_STREAM: c_int = 1;
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
}

struct Backend {
    path: String,
    stream: UnixStream,
}

fn main() {
    let listen = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:9999".into());
    let (host, port) = parse_listen_addr(&listen);

    let paths: Vec<String> = std::env::var("BACKEND_SOCKS")
        .unwrap_or_else(|_| "/sockets/api1.sock,/sockets/api2.sock".into())
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    assert!(!paths.is_empty(), "BACKEND_SOCKS must list at least one socket");

    let listener = listen_tcp(host, port);
    let mut backends: Vec<Backend> = paths
        .into_iter()
        .map(|path| Backend {
            stream: connect_backend(&path),
            path,
        })
        .collect();

    let n_back = backends.len();
    let mut next = 0usize;

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
        let mut idx = next;
        next += 1;
        if next == n_back {
            next = 0;
        }
        let start = idx;
        let mut sent = false;

        loop {
            let backend = &mut backends[idx];
            match fdpass_send::send_fd(&backend.stream, fd) {
                Ok(()) => {
                    sent = true;
                    break;
                }
                Err(_) => {
                    backend.stream = reconnect_backend(&backend.path);
                }
            }
            idx += 1;
            if idx == n_back {
                idx = 0;
            }
            if idx == start {
                break;
            }
        }

        unsafe { close(fd) };

        if !sent {
            continue;
        }
    }
}

fn parse_listen_addr(listen: &str) -> (u32, u16) {
    let (host, port) = listen.rsplit_once(':').unwrap_or(("0.0.0.0", listen));
    let port = port.parse().unwrap_or(9999);
    let addr = if host.is_empty() || host == "0.0.0.0" {
        0
    } else {
        let octets = host
            .parse::<Ipv4Addr>()
            .unwrap_or_else(|_| panic!("LISTEN_ADDR must use an IPv4 host: {}", listen))
            .octets();
        u32::from_ne_bytes(octets)
    };
    (addr, port)
}

fn listen_tcp(addr: u32, port: u16) -> c_int {
    let fd = unsafe { socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0) };
    if fd < 0 {
        panic!("socket: {}", std::io::Error::last_os_error());
    }
    let one = 1i32;
    let rc = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            ptr::addr_of!(one).cast(),
            mem::size_of::<i32>() as u32,
        )
    };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { close(fd) };
        panic!("setsockopt SO_REUSEADDR: {}", err);
    }
    let addr = SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: addr,
        sin_zero: [0; 8],
    };
    if unsafe { bind(fd, ptr::addr_of!(addr).cast(), mem::size_of::<SockAddrIn>() as u32) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { close(fd) };
        panic!("bind: {}", err);
    }
    if unsafe { listen(fd, LISTEN_BACKLOG) } < 0 {
        let err = std::io::Error::last_os_error();
        unsafe { close(fd) };
        panic!("listen: {}", err);
    }
    fd
}

fn connect_backend(path: &str) -> UnixStream {
    connect_backend_with_backoff(path)
}

fn reconnect_backend(path: &str) -> UnixStream {
    connect_backend_with_backoff(path)
}

fn connect_backend_with_backoff(path: &str) -> UnixStream {
    let mut attempts = 0u32;
    loop {
        if let Ok(stream) = UnixStream::connect(path) {
            return stream;
        }
        attempts = attempts.saturating_add(1);
        if attempts < 100 {
            std::hint::spin_loop();
        } else if attempts < 10_000 {
            thread::yield_now();
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

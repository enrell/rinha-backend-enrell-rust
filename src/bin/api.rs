#[path = "../fdpass_recv.rs"]
mod fdpass_recv;

#[path = "../index.rs"]
mod index;

#[path = "../search.rs"]
mod search;

use index::{quantize_vec, DIM};
use search::{Index, SearchStats};

use std::ffi::{c_int, c_void};
use std::fs;
use std::io::ErrorKind;
use std::mem;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Pre-computed HTTP responses (6 possible fraud scores: 0/5 .. 5/5)
// ---------------------------------------------------------------------------
const READY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";

// Body: {"approved":true,"fraud_score":0.0}  = 35 bytes
const RESP_0: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}";
// Body: {"approved":true,"fraud_score":0.2}  = 35 bytes
const RESP_1: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}";
// Body: {"approved":true,"fraud_score":0.4}  = 35 bytes
const RESP_2: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\nConnection: keep-alive\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}";
// Body: {"approved":false,"fraud_score":0.6} = 36 bytes
const RESP_3: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}";
// Body: {"approved":false,"fraud_score":0.8} = 36 bytes
const RESP_4: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}";
// Body: {"approved":false,"fraud_score":1.0} = 36 bytes
const RESP_5: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\nConnection: keep-alive\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}";

const RESPONSES: [&[u8]; 6] = [RESP_0, RESP_1, RESP_2, RESP_3, RESP_4, RESP_5];

// ---------------------------------------------------------------------------
// Epoll / networking constants
// ---------------------------------------------------------------------------
const BUF_CAP: usize = 4 * 1024;
const EPOLL_EVENTS: usize = 1024;
const EPOLL_CLOEXEC: c_int = 0x80000;
const EPOLL_CTL_ADD: c_int = 1;
const EPOLL_CTL_MOD: c_int = 3;
const EPOLL_CTL_DEL: c_int = 2;
const EPOLLIN: u32 = 0x001;
const EPOLLOUT: u32 = 0x004;
const EPOLLERR: u32 = 0x008;
const EPOLLHUP: u32 = 0x010;
const EPOLLRDHUP: u32 = 0x2000;
const IPPROTO_TCP: c_int = 6;
const TCP_NODELAY: c_int = 1;
const TCP_QUICKACK: c_int = 12;
const MAX_CLIENTS: usize = 128;
const TAG_LISTENER: u64 = 1 << 63;
const TAG_CONTROL: u64 = TAG_LISTENER | (1 << 62);

// ---------------------------------------------------------------------------
// Global index
// ---------------------------------------------------------------------------
static mut INDEX: *const Index = ptr::null();
static INDEX_STATS: AtomicBool = AtomicBool::new(false);

fn get_index() -> &'static Index {
    unsafe { &*INDEX }
}

// ---------------------------------------------------------------------------
// Epoll FFI
// ---------------------------------------------------------------------------
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

impl EpollEvent {
    const fn new(events: u32, data: u64) -> Self {
        Self { events, data }
    }

    const fn empty() -> Self {
        Self { events: 0, data: 0 }
    }
}

struct Client {
    fd: RawFd,
    buf: [u8; BUF_CAP],
    len: usize,
    pending: Option<&'static [u8]>,
    pending_off: usize,
    last_events: u32,
}

unsafe extern "C" {
    fn epoll_create1(flags: c_int) -> c_int;
    fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut EpollEvent) -> c_int;
    fn epoll_wait(epfd: c_int, events: *mut EpollEvent, maxevents: c_int, timeout: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    fn close(fd: c_int) -> c_int;
    fn setsockopt(sockfd: c_int, level: c_int, optname: c_int, optval: *const c_void, optlen: u32) -> c_int;
}

// ===========================================================================
// main
// ===========================================================================
fn main() {
    // Load the pre-built index
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "/app/data".to_string());
    INDEX_STATS.store(env_true("INDEX_STATS"), Ordering::Relaxed);
    unsafe { INDEX = Box::into_raw(Box::new(Index::load(&data_dir))); }
    eprintln!("Index loaded: {} vectors", get_index().count);

    let mut pool: Vec<Client> = Vec::with_capacity(MAX_CLIENTS);
    for _ in 0..MAX_CLIENTS {
        pool.push(Client {
            fd: -1,
            buf: [0u8; BUF_CAP],
            len: 0,
            pending: None,
            pending_off: 0,
            last_events: 0,
        });
    }
    let pool: &'static mut [Client] = pool.leak();
    let mut free_list: Vec<u16> = (0..MAX_CLIENTS as u16).rev().collect();

    if let Ok(path) = std::env::var("FD_PASS_PATH") {
        run_fd_pass(&path, pool, &mut free_list);
        return;
    }
    eprintln!("FD_PASS_PATH required");
    std::process::exit(1);
}

fn run_fd_pass(path: &str, pool: &mut [Client], free_list: &mut Vec<u16>) {
    let socket_path = Path::new(path);
    if let Some(parent) = socket_path.parent() {
        fs::create_dir_all(parent).ok();
    }
    let _ = fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).expect("bind unix socket");
    listener.set_nonblocking(true).expect("nonblocking listener");
    let listener_fd = listener.as_raw_fd();

    let epoll = unsafe { epoll_create1(EPOLL_CLOEXEC) };
    if epoll < 0 {
        panic!("epoll_create1: {}", std::io::Error::last_os_error());
    }

    let _ = epoll_add(epoll, listener_fd, TAG_LISTENER, EPOLLIN);
    let mut control: Option<UnixStream> = None;
    let mut events = [EpollEvent::empty(); EPOLL_EVENTS];

    loop {
        let n = unsafe { epoll_wait(epoll, events.as_mut_ptr(), events.len() as c_int, -1) };
        if n <= 0 {
            continue;
        }

        for i in 0..n as usize {
            let ev = events[i];
            let data = ev.data;
            let bits = ev.events;

            if data == TAG_LISTENER {
                accept_control(epoll, &listener, &mut control);
                continue;
            }

            if data == TAG_CONTROL {
                if bits & (EPOLLERR | EPOLLHUP | EPOLLRDHUP) != 0 {
                    control = None;
                    continue;
                }
                if let Some(ctrl) = control.as_ref() {
                    recv_fds(epoll, ctrl, bits, pool, free_list);
                }
                continue;
            }

            let idx = data as usize;
            if idx >= MAX_CLIENTS {
                continue;
            }

            if bits & (EPOLLERR | EPOLLHUP | EPOLLRDHUP) != 0 {
                drop_client(epoll, idx, pool, free_list);
                continue;
            }

            let keep = unsafe { handle_client(&mut pool[idx], bits) };
            if keep {
                let fd = pool[idx].fd;
                let new_events = client_events(&pool[idx]);
                if new_events != pool[idx].last_events {
                    pool[idx].last_events = new_events;
                    epoll_mod(epoll, fd, idx as u64, new_events);
                }
            } else {
                drop_client(epoll, idx, pool, free_list);
            }
        }
    }
}

fn accept_control(epoll: RawFd, listener: &UnixListener, control: &mut Option<UnixStream>) {
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = stream.set_nonblocking(true);
                let fd = stream.as_raw_fd();
                if let Some(old) = control.take() {
                    epoll_del(epoll, old.as_raw_fd());
                }
                let _ = epoll_add(epoll, fd, TAG_CONTROL, EPOLLIN | EPOLLRDHUP);
                *control = Some(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

fn recv_fds(epoll: RawFd, control: &UnixStream, bits: u32, pool: &mut [Client], free_list: &mut Vec<u16>) {
    if bits & EPOLLIN == 0 {
        return;
    }
    loop {
        match fdpass_recv::recv_fd(control) {
            Ok(Some(fd)) => {
                let raw_fd = fd.as_raw_fd();
                tune_client(raw_fd);
                let Some(idx) = free_list.pop() else {
                    return;
                };
                let raw_fd = fd.into_raw_fd();
                let slot = &mut pool[idx as usize];
                slot.fd = raw_fd;
                slot.len = 0;
                slot.pending = None;
                slot.pending_off = 0;
                slot.last_events = EPOLLIN | EPOLLRDHUP;
                if !epoll_add(epoll, raw_fd, idx as u64, EPOLLIN | EPOLLRDHUP) {
                    close_fd(raw_fd);
                    slot.fd = -1;
                    free_list.push(idx);
                }
            }
            Ok(None) => return,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(_) => return,
        }
    }
}

// ===========================================================================
// HTTP request handling
// ===========================================================================
unsafe fn handle_client(c: &mut Client, bits: u32) -> bool {
    if c.pending.is_some() {
        if bits & EPOLLOUT != 0 && !flush(c) {
            return false;
        }
        if c.pending.is_some() {
            return true;
        }
    }

    if bits & EPOLLIN != 0 {
        loop {
            let space = BUF_CAP.saturating_sub(c.len);
            if space == 0 {
                return false;
            }
            let n = read_fd(c.fd, unsafe { c.buf.as_mut_ptr().add(c.len) }, space);
            if n == 0 {
                return false;
            }
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == ErrorKind::WouldBlock {
                    break;
                }
                return false;
            }
            c.len += n as usize;
        }
    }

    loop {
        let Some((total, body_start)) = request_total(&c.buf[..c.len]) else {
            break;
        };
        if c.len < total {
            break;
        }

        let response = route(&c.buf[..total], body_start);
        compact(c, total);

        if !write_all(c, response) {
            return true;
        }
    }

    true
}

fn route(req: &[u8], body_start: usize) -> &'static [u8] {
    if req.len() >= 10 && req.starts_with(b"GET /ready") {
        return READY;
    }
    if req.len() >= 17 && req.starts_with(b"POST /fraud-score") {
        if body_start < req.len() {
            let body = &req[body_start..];
            return handle_fraud_score(body);
        }
        return RESP_0; // fallback: approve if no body
    }
    NOT_FOUND
}

// ===========================================================================
// Fraud score pipeline
// ===========================================================================
fn handle_fraud_score(body: &[u8]) -> &'static [u8] {
    let tx = match parse_transaction(body) {
        Some(tx) => tx,
        None => return RESP_0,
    };

    let vec = vectorize(&tx);
    let qvec = quantize_vec(&vec);
    let fraud_count = if INDEX_STATS.load(Ordering::Relaxed) {
        let mut stats = SearchStats::default();
        let start = Instant::now();
        let fraud_count = get_index().search_exact_with_stats(&qvec, &mut stats);
        let search_ns = start.elapsed().as_nanos();
        eprintln!(
            "search_stats key={} fraud_count={} search_ns={} partitions_considered={} partitions_searched={} partitions_pruned={} empty_partitions={} nodes_visited={} nodes_pruned={} leaves_scanned={} vectors_scanned={} stage8_pruned={} full_scanned={}",
            stats.partition_key,
            fraud_count,
            search_ns,
            stats.partitions_considered,
            stats.partitions_searched,
            stats.partitions_pruned,
            stats.empty_partitions,
            stats.nodes_visited,
            stats.nodes_pruned,
            stats.leaves_scanned,
            stats.vectors_scanned,
            stats.stage8_pruned,
            stats.full_scanned,
        );
        fraud_count
    } else {
        get_index().search_exact(&qvec)
    };

    RESPONSES[fraud_count as usize]
}

fn env_true(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes"),
        Err(_) => false,
    }
}

// ===========================================================================
// Transaction struct
// ===========================================================================
struct Transaction {
    amount: f64,
    installments: f64,
    requested_at: [u8; 20], // "YYYY-MM-DDTHH:MM:SSZ"
    requested_at_len: usize,
    avg_amount: f64,       // customer
    tx_count_24h: f64,
    known_merchants_raw: [[u8; 16]; 32],
    known_merchants_len: [usize; 32], // length of each merchant id
    known_merchants_count: usize,
    merchant_id: [u8; 16],
    merchant_id_len: usize,
    mcc: [u8; 8],
    mcc_len: usize,
    merchant_avg_amount: f64,
    is_online: bool,
    card_present: bool,
    km_from_home: f64,
    has_last_tx: bool,
    last_timestamp: [u8; 20],
    last_timestamp_len: usize,
    km_from_last: f64,
}

impl Transaction {
    fn new() -> Self {
        Transaction {
            amount: 0.0,
            installments: 1.0,
            requested_at: [0u8; 20],
            requested_at_len: 0,
            avg_amount: 1.0, // avoid div by zero
            tx_count_24h: 0.0,
            known_merchants_raw: [[0u8; 16]; 32],
            known_merchants_len: [0usize; 32],
            known_merchants_count: 0,
            merchant_id: [0u8; 16],
            merchant_id_len: 0,
            mcc: [0u8; 8],
            mcc_len: 0,
            merchant_avg_amount: 0.0,
            is_online: false,
            card_present: true,
            km_from_home: 0.0,
            has_last_tx: false,
            last_timestamp: [0u8; 20],
            last_timestamp_len: 0,
            km_from_last: 0.0,
        }
    }
}

// ===========================================================================
// JSON parser — hand-rolled, zero-alloc
// ===========================================================================

/// Find the byte offset right after `key` (including the colon) in `data`.
/// Searches for `"key":` pattern and returns position after the colon.
fn find_key(data: &[u8], key: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + key.len() + 3 < data.len() {
        // look for `"key"`
        if data[i] == b'"' && data[i + 1..].starts_with(key) && data[i + 1 + key.len()] == b'"' {
            // skip past `"key"` then find `:`
            let mut p = i + 1 + key.len() + 1; // past closing quote
            while p < data.len() && data[p] == b' ' {
                p += 1;
            }
            if p < data.len() && data[p] == b':' {
                p += 1;
                // skip whitespace after colon
                while p < data.len() && (data[p] == b' ' || data[p] == b'\n' || data[p] == b'\r' || data[p] == b'\t') {
                    p += 1;
                }
                return Some(p);
            }
        }
        i += 1;
    }
    None
}

/// Parse a floating-point number from JSON. Returns (value, end_position).
fn parse_json_float(data: &[u8], pos: usize) -> (f64, usize) {
    let mut i = pos;
    let negative = if i < data.len() && data[i] == b'-' {
        i += 1;
        true
    } else {
        false
    };

    let mut int_part: f64 = 0.0;
    while i < data.len() && data[i].is_ascii_digit() {
        int_part = int_part * 10.0 + (data[i] - b'0') as f64;
        i += 1;
    }

    let mut frac_part: f64 = 0.0;
    if i < data.len() && data[i] == b'.' {
        i += 1;
        let mut divisor: f64 = 10.0;
        while i < data.len() && data[i].is_ascii_digit() {
            frac_part += (data[i] - b'0') as f64 / divisor;
            divisor *= 10.0;
            i += 1;
        }
    }

    let val = int_part + frac_part;
    if negative { (-val, i) } else { (val, i) }
}

/// Parse a quoted string from JSON. Returns the string content as bytes and end position.
/// Expects `data[pos]` == `"`.
fn parse_json_string<'a>(data: &'a [u8], pos: usize) -> (&'a [u8], usize) {
    if pos >= data.len() || data[pos] != b'"' {
        return (&[], pos);
    }
    let start = pos + 1;
    let mut i = start;
    while i < data.len() && data[i] != b'"' {
        i += 1;
    }
    (&data[start..i], if i < data.len() { i + 1 } else { i })
}

/// Parse a JSON boolean. Returns (value, end_position).
fn parse_json_bool(data: &[u8], pos: usize) -> (bool, usize) {
    if pos + 4 <= data.len() && &data[pos..pos + 4] == b"true" {
        (true, pos + 4)
    } else if pos + 5 <= data.len() && &data[pos..pos + 5] == b"false" {
        (false, pos + 5)
    } else {
        (false, pos)
    }
}

/// Check if the value at `pos` is JSON `null`.
fn is_json_null(data: &[u8], pos: usize) -> bool {
    pos + 4 <= data.len() && &data[pos..pos + 4] == b"null"
}

/// Find the position of a section key like "customer", "merchant", etc.
fn find_section(data: &[u8], section: &[u8]) -> Option<usize> {
    find_key(data, section, 0)
}

fn parse_transaction(body: &[u8]) -> Option<Transaction> {
    let mut tx = Transaction::new();

    // --- transaction section ---
    let tx_section = find_section(body, b"transaction")?;

    if let Some(p) = find_key(body, b"amount", tx_section) {
        let (val, _) = parse_json_float(body, p);
        tx.amount = val;
    }

    if let Some(p) = find_key(body, b"installments", tx_section) {
        let (val, _) = parse_json_float(body, p);
        tx.installments = val;
    }

    if let Some(p) = find_key(body, b"requested_at", tx_section) {
        let (s, _) = parse_json_string(body, p);
        let len = s.len().min(20);
        tx.requested_at[..len].copy_from_slice(&s[..len]);
        tx.requested_at_len = len;
    }

    // --- customer section ---
    let cust_section = find_section(body, b"customer")?;

    if let Some(p) = find_key(body, b"avg_amount", cust_section) {
        let (val, _) = parse_json_float(body, p);
        tx.avg_amount = if val > 0.0 { val } else { 1.0 };
    }

    if let Some(p) = find_key(body, b"tx_count_24h", cust_section) {
        let (val, _) = parse_json_float(body, p);
        tx.tx_count_24h = val;
    }

    // known_merchants array
    if let Some(p) = find_key(body, b"known_merchants", cust_section) {
        // p should point to `[`
        if p < body.len() && body[p] == b'[' {
            let mut i = p + 1;
            let mut count = 0usize;
            while i < body.len() && count < 32 {
                // skip whitespace
                while i < body.len() && (body[i] == b' ' || body[i] == b',' || body[i] == b'\n' || body[i] == b'\r' || body[i] == b'\t') {
                    i += 1;
                }
                if i >= body.len() || body[i] == b']' {
                    break;
                }
                let (s, end) = parse_json_string(body, i);
                let slen = s.len().min(16);
                tx.known_merchants_raw[count][..slen].copy_from_slice(&s[..slen]);
                tx.known_merchants_len[count] = slen;
                count += 1;
                i = end;
            }
            tx.known_merchants_count = count;
        }
    }

    // --- merchant section ---
    let merch_section = find_section(body, b"merchant")?;

    if let Some(p) = find_key(body, b"id", merch_section) {
        let (s, _) = parse_json_string(body, p);
        let slen = s.len().min(16);
        tx.merchant_id[..slen].copy_from_slice(&s[..slen]);
        tx.merchant_id_len = slen;
    }

    if let Some(p) = find_key(body, b"mcc", merch_section) {
        let (s, _) = parse_json_string(body, p);
        let slen = s.len().min(8);
        tx.mcc[..slen].copy_from_slice(&s[..slen]);
        tx.mcc_len = slen;
    }

    // merchant avg_amount — must search after merch_section to avoid customer's
    if let Some(p) = find_key(body, b"avg_amount", merch_section) {
        let (val, _) = parse_json_float(body, p);
        tx.merchant_avg_amount = val;
    }

    // --- terminal section ---
    let term_section = find_section(body, b"terminal")?;

    if let Some(p) = find_key(body, b"is_online", term_section) {
        let (val, _) = parse_json_bool(body, p);
        tx.is_online = val;
    }

    if let Some(p) = find_key(body, b"card_present", term_section) {
        let (val, _) = parse_json_bool(body, p);
        tx.card_present = val;
    }

    if let Some(p) = find_key(body, b"km_from_home", term_section) {
        let (val, _) = parse_json_float(body, p);
        tx.km_from_home = val;
    }

    // --- last_transaction section (may be null) ---
    if let Some(p) = find_key(body, b"last_transaction", 0) {
        if is_json_null(body, p) {
            tx.has_last_tx = false;
        } else {
            tx.has_last_tx = true;

            if let Some(tp) = find_key(body, b"timestamp", p) {
                let (s, _) = parse_json_string(body, tp);
                let len = s.len().min(20);
                tx.last_timestamp[..len].copy_from_slice(&s[..len]);
                tx.last_timestamp_len = len;
            }

            if let Some(kp) = find_key(body, b"km_from_current", p) {
                let (val, _) = parse_json_float(body, kp);
                tx.km_from_last = val;
            }
        }
    }

    Some(tx)
}

// ===========================================================================
// Vectorization — 14 dimensions
// ===========================================================================
fn vectorize(tx: &Transaction) -> [f64; DIM] {
    let mut v = [0.0f64; DIM];

    let (hour, dow) = if tx.requested_at_len >= 13 {
        parse_datetime(&tx.requested_at[..tx.requested_at_len])
    } else {
        (12, 3) // fallback: noon on Wednesday
    };

    v[0] = clamp01(tx.amount / 10000.0);
    v[1] = clamp01(tx.installments / 12.0);
    v[2] = clamp01((tx.amount / tx.avg_amount) / 10.0);
    v[3] = hour as f64 / 23.0;
    v[4] = dow as f64 / 6.0;

    if tx.has_last_tx && tx.requested_at_len >= 16 && tx.last_timestamp_len >= 16 {
        let mins = minutes_between(
            &tx.requested_at[..tx.requested_at_len],
            &tx.last_timestamp[..tx.last_timestamp_len],
        );
        v[5] = clamp01(mins / 1440.0);
        v[6] = clamp01(tx.km_from_last / 1000.0);
    } else {
        v[5] = -1.0;
        v[6] = -1.0;
    }

    v[7] = clamp01(tx.km_from_home / 1000.0);
    v[8] = clamp01(tx.tx_count_24h / 20.0);
    v[9] = if tx.is_online { 1.0 } else { 0.0 };
    v[10] = if tx.card_present { 1.0 } else { 0.0 };
    v[11] = if is_unknown_merchant(tx) { 1.0 } else { 0.0 };
    v[12] = mcc_risk(&tx.mcc[..tx.mcc_len]);
    v[13] = clamp01(tx.merchant_avg_amount / 10000.0);

    v
}

#[inline(always)]
fn clamp01(x: f64) -> f64 {
    if x < 0.0 { 0.0 } else if x > 1.0 { 1.0 } else { x }
}

// ===========================================================================
// MCC risk lookup (hardcoded from resources/mcc_risk.json)
// ===========================================================================
fn mcc_risk(mcc: &[u8]) -> f64 {
    match mcc {
        b"5411" => 0.15,
        b"5812" => 0.30,
        b"5912" => 0.20,
        b"5944" => 0.45,
        b"7801" => 0.80,
        b"7802" => 0.75,
        b"7995" => 0.85,
        b"4511" => 0.35,
        b"5311" => 0.25,
        b"5999" => 0.50,
        _ => 0.50,
    }
}

// ===========================================================================
// Unknown merchant check
// ===========================================================================
fn is_unknown_merchant(tx: &Transaction) -> bool {
    let mid = &tx.merchant_id[..tx.merchant_id_len];
    for i in 0..tx.known_merchants_count {
        let km = &tx.known_merchants_raw[i][..tx.known_merchants_len[i]];
        if km == mid {
            return false;
        }
    }
    true
}

// ===========================================================================
// Datetime parsing
// ===========================================================================

/// Parse ISO 8601 datetime and return (hour, day_of_week).
/// Format: "YYYY-MM-DDTHH:MM:SSZ"
/// day_of_week: 0=Mon, 6=Sun
fn parse_datetime(s: &[u8]) -> (u32, u32) {
    let year = parse_u32(&s[0..4]);
    let month = parse_u32(&s[5..7]);
    let day = parse_u32(&s[8..10]);
    let hour = parse_u32(&s[11..13]);
    let dow = day_of_week(year, month, day);
    (hour, dow)
}

/// Tomohiko Sakamoto's algorithm, adjusted to return 0=Mon, 6=Sun.
fn day_of_week(mut y: u32, m: u32, d: u32) -> u32 {
    const T: [u32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 {
        y -= 1;
    }
    // Sakamoto returns 0=Sunday. Adjust: (result + 6) % 7 → 0=Mon, 6=Sun
    let sakamoto = (y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7;
    (sakamoto + 6) % 7
}

/// Compute minutes between two ISO 8601 timestamps.
/// Returns (requested_at_minutes - last_timestamp_minutes).
fn minutes_between(requested_at: &[u8], last_timestamp: &[u8]) -> f64 {
    let t1 = parse_epoch_minutes(requested_at);
    let t2 = parse_epoch_minutes(last_timestamp);
    (t1 - t2) as f64
}

fn parse_epoch_minutes(s: &[u8]) -> i64 {
    let year = parse_u32(&s[0..4]) as i64;
    let month = parse_u32(&s[5..7]) as i64;
    let day = parse_u32(&s[8..10]) as i64;
    let hour = parse_u32(&s[11..13]) as i64;
    let min = parse_u32(&s[14..16]) as i64;

    let days = days_since_epoch(year, month, day);
    days * 1440 + hour * 60 + min
}

/// Days since Unix epoch using the civil calendar algorithm.
fn days_since_epoch(y: i64, m: i64, d: i64) -> i64 {
    let mut y = y;
    let mut m = m;
    if m <= 2 {
        y -= 1;
        m += 12;
    }
    365 * y + y / 4 - y / 100 + y / 400 + (153 * (m - 3) + 2) / 5 + d - 719469
}

fn parse_u32(s: &[u8]) -> u32 {
    let mut v = 0u32;
    for &b in s {
        v = v * 10 + (b - b'0') as u32;
    }
    v
}

// ===========================================================================
// Existing infrastructure (write_all, flush, compact, etc.)
// ===========================================================================
fn write_all(c: &mut Client, resp: &'static [u8]) -> bool {
    let mut off = 0usize;
    while off < resp.len() {
        let n = write_fd(c.fd, &resp[off..]);
        if n == 0 {
            return false;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                c.pending = Some(resp);
                c.pending_off = off;
                return false;
            }
            return false;
        }
        off += n as usize;
    }
    true
}

fn flush(c: &mut Client) -> bool {
    let Some(resp) = c.pending else {
        return true;
    };
    let mut off = c.pending_off;
    while off < resp.len() {
        let n = write_fd(c.fd, &resp[off..]);
        if n == 0 {
            return false;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::WouldBlock || err.kind() == ErrorKind::Interrupted {
                c.pending_off = off;
                return true;
            }
            return false;
        }
        off += n as usize;
    }
    c.pending = None;
    c.pending_off = 0;
    true
}

fn compact(c: &mut Client, consumed: usize) {
    if consumed >= c.len {
        c.len = 0;
        return;
    }
    c.buf.copy_within(consumed..c.len, 0);
    c.len -= consumed;
}

fn request_total(buf: &[u8]) -> Option<(usize, usize)> {
    let mut i = 0usize;
    while i + 3 < buf.len() {
        if buf[i] == b'\r' && buf[i + 1] == b'\n' && buf[i + 2] == b'\r' && buf[i + 3] == b'\n' {
            let header_end = i + 4;
            let body = content_length_fast(&buf[..header_end]);
            return Some((header_end + body, header_end));
        }
        i += 1;
    }
    None
}

fn content_length_fast(header: &[u8]) -> usize {
    let needle = b"content-length:";
    let mut i = 0usize;
    while i + needle.len() <= header.len() {
        if header[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            let mut p = i + needle.len();
            while p < header.len() && header[p] == b' ' {
                p += 1;
            }
            let mut v = 0usize;
            while p < header.len() && header[p].is_ascii_digit() {
                v = v * 10 + (header[p] - b'0') as usize;
                p += 1;
            }
            return v;
        }
        i += 1;
    }
    0
}

fn client_events(c: &Client) -> u32 {
    let mut ev = EPOLLIN | EPOLLRDHUP;
    if c.pending.is_some() {
        ev |= EPOLLOUT;
    }
    ev
}

fn drop_client(epoll: RawFd, idx: usize, pool: &mut [Client], free_list: &mut Vec<u16>) {
    let client = &mut pool[idx];
    if client.fd >= 0 {
        epoll_del(epoll, client.fd);
        close_fd(client.fd);
    }
    client.fd = -1;
    free_list.push(idx as u16);
}

fn tune_client(fd: RawFd) {
    let one = 1i32;
    unsafe {
        setsockopt(
            fd,
            IPPROTO_TCP,
            TCP_NODELAY,
            ptr::addr_of!(one).cast(),
            mem::size_of::<i32>() as u32,
        );
        setsockopt(
            fd,
            IPPROTO_TCP,
            TCP_QUICKACK,
            ptr::addr_of!(one).cast(),
            mem::size_of::<i32>() as u32,
        );
    }
}

fn read_fd(fd: RawFd, buf: *mut u8, len: usize) -> isize {
    unsafe { read(fd, buf.cast(), len) }
}

fn write_fd(fd: RawFd, buf: &[u8]) -> isize {
    unsafe { write(fd, buf.as_ptr().cast(), buf.len()) }
}

fn epoll_add(epoll: RawFd, fd: RawFd, data: u64, events: u32) -> bool {
    let mut ev = EpollEvent::new(events, data);
    if unsafe { epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
        return false;
    }
    true
}

fn epoll_mod(epoll: RawFd, fd: RawFd, data: u64, events: u32) {
    let mut ev = EpollEvent::new(events, data);
    if unsafe { epoll_ctl(epoll, EPOLL_CTL_MOD, fd, &mut ev) } < 0 {
        close_fd(fd);
    }
}

fn epoll_del(epoll: RawFd, fd: RawFd) {
    let mut ev = EpollEvent::empty();
    unsafe { epoll_ctl(epoll, EPOLL_CTL_DEL, fd, &mut ev) };
}

fn close_fd(fd: RawFd) {
    unsafe { close(fd) };
}

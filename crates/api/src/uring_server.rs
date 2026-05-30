//! io_uring reactor for the FD-handoff API hot path (Linux only) — Stage 7 bet.
//!
//! Goal (see docs/perf-bottlenecks.md): cut per-request syscalls + wakeup latency
//! below what epoll can do. Uses a single io_uring with SINGLE_ISSUER +
//! DEFER_TASKRUN and registers NAPI busy-poll (IORING_REGISTER_NAPI, kernel
//! >= 6.9) via a raw register call (the safe crate API doesn't expose it),
//! with graceful fallback. Scoring is byte-identical to the epoll path — this
//! only owns I/O. Gated by API_FD_URING=1 (default off; epoll stays default).
//!
//! First cut: owned per-connection buffers + plain Recv/Send (re-armed), control
//! fd drained via a multishot Poll + blocking recvmsg. Multishot recv + provided
//! buffer rings are a follow-up optimization (docs/io-uring-design.md).

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;
use std::time::Instant;

use io_uring::types::BufRingEntry;
use io_uring::{cqueue, opcode, types, IoUring};
use std::sync::atomic::{AtomicU16, Ordering};

use crate::{
    handoff_response_for_bucket, parse_handoff_request, score_body, score_body_fast_bucket,
    ApiParser, AppState, HandoffRoute, HANDOFF_BUFFER_BYTES, HTTP_BAD_REQUEST,
    HTTP_PAYLOAD_TOO_LARGE, HTTP_READY_OK,
};

// ---- NAPI busy-poll registration (raw; not in the 0.7 safe API) -------------

const IORING_REGISTER_NAPI: libc::c_uint = 27;

#[repr(C)]
#[derive(Clone, Copy)]
struct IoUringNapi {
    busy_poll_to: u32,
    prefer_busy_poll: u8,
    pad: [u8; 3],
    resv: u64,
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

/// Register NAPI busy-poll on the ring. Graceful: logs + continues on failure
/// (kernel < 6.9), exactly like the EPIOCSPARAMS path in epoll_server.rs.
fn configure_napi(ring: &IoUring) {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| "unknown".to_string());
    let usecs = env_u32("API_BUSY_POLL_US", 50);
    let prefer = env_u32("API_PREFER_BUSY_POLL", 1).min(1) as u8;
    if usecs == 0 && prefer == 0 {
        eprintln!("io_uring NAPI busy-poll disabled; kernel {}", kernel.trim());
        return;
    }
    let napi = IoUringNapi {
        busy_poll_to: usecs,
        prefer_busy_poll: prefer,
        pad: [0; 3],
        resv: 0,
    };
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            ring.as_raw_fd() as libc::c_long,
            IORING_REGISTER_NAPI as libc::c_long,
            &napi as *const IoUringNapi as libc::c_long,
            1usize as libc::c_long,
        )
    };
    if rc == 0 {
        eprintln!(
            "io_uring NAPI busy-poll enabled: usecs={} prefer={} (kernel {})",
            usecs,
            prefer,
            kernel.trim()
        );
    } else {
        let err = io::Error::last_os_error();
        eprintln!(
            "IORING_REGISTER_NAPI unavailable ({}); plain io_uring. kernel {} (NAPI needs >= 6.9)",
            err,
            kernel.trim()
        );
    }
}

// ---- user_data encoding: high 8 bits = op, low 32 bits = fd -----------------

const OP_CTRL_POLL: u64 = 1;
const OP_RECV: u64 = 2;
const OP_SEND: u64 = 3;

#[inline]
fn ud(op: u64, fd: RawFd) -> u64 {
    (op << 56) | (fd as u32 as u64)
}
#[inline]
fn ud_op(user_data: u64) -> u64 {
    user_data >> 56
}
#[inline]
fn ud_fd(user_data: u64) -> RawFd {
    (user_data & 0xffff_ffff) as u32 as RawFd
}

// ---- connection state -------------------------------------------------------

struct Conn {
    buf: Box<[u8]>,
    len: usize,
    /// Pending response (static bytes) + offset while a Send is in flight.
    pending: Option<(&'static [u8], usize)>,
    close_after: bool,
    recv_armed: bool,
}

impl Conn {
    fn new() -> Self {
        Conn {
            buf: vec![0u8; HANDOFF_BUFFER_BYTES].into_boxed_slice(),
            len: 0,
            pending: None,
            close_after: false,
            recv_armed: false,
        }
    }
}

fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

fn tune_client_fd(fd: RawFd) {
    unsafe {
        let one: libc::c_int = 1;
        let p = &one as *const libc::c_int as *const libc::c_void;
        let l = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let _ = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, p, l);
        let _ = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK, p, l);
    }
}

/// Re-arm one-shot TCP_QUICKACK after each request so the response ACK isn't
/// delayed (perf-handoff-learnings: QUICKACK is worth ~1.2ms here).
#[inline]
fn set_quickack(fd: RawFd) {
    unsafe {
        let one: libc::c_int = 1;
        let _ = libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            &one as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

/// Blocking-but-nonblocking-fd SCM_RIGHTS receive (control socket is O_NONBLOCK;
/// returns Ok(Some(fd)) / Ok(None) on close / Err(EAGAIN) when drained).
fn recv_fd(control_fd: RawFd) -> io::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut cbuf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = cbuf.len() as _;
    let n = unsafe { libc::recvmsg(control_fd, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
    if n == 0 {
        return Ok(None);
    }
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no fd in cmsg"));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            &mut fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
        if fd < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad fd"));
        }
        Ok(Some(fd))
    }
}

fn process_request(state: &AppState, body: &[u8], route: HandoffRoute) -> &'static [u8] {
    match route {
        HandoffRoute::Ready => HTTP_READY_OK,
        HandoffRoute::FraudScore => {
            let handler_start = if state.perf.is_some() || state.log_search_avg {
                Some(Instant::now())
            } else {
                None
            };
            let bucket = if state.perf.is_none()
                && !state.log_search_avg
                && state.parser == ApiParser::Fast
            {
                score_body_fast_bucket(state, body)
            } else {
                score_body(state, body, handler_start, false)
            };
            match bucket {
                Ok(b) => handoff_response_for_bucket(b),
                Err(_) => HTTP_BAD_REQUEST,
            }
        }
    }
}

fn compact(conn: &mut Conn, consumed: usize) {
    if consumed >= conn.len {
        conn.len = 0;
    } else {
        conn.buf.copy_within(consumed..conn.len, 0);
        conn.len -= consumed;
    }
}

pub fn run(listen: String, state: AppState) -> io::Result<()> {
    if std::env::var("API_URING_MULTISHOT").ok().as_deref() == Some("1") {
        return run_multishot(listen, state);
    }
    run_plain(listen, state)
}

fn run_plain(listen: String, state: AppState) -> io::Result<()> {
    let path = listen.strip_prefix("unix:").unwrap_or(&listen).to_string();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    eprintln!("FD handoff server (io_uring, plain recv/send) listening on {}", path);

    let mut ring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(4096)?;
    configure_napi(&ring);

    let mut control: Option<RawFd> = None;
    let mut conns: HashMap<RawFd, Box<Conn>> = HashMap::new();

    // Accept exactly one (or first) control connection synchronously, then drive
    // everything through the ring. The LB opens one control connection per API.
    accept_control_blocking(&listener, &mut control)?;

    if let Some(cfd) = control {
        arm_ctrl_poll(&mut ring, cfd)?;
        ring.submit()?;
    }

    let mut cqes: Vec<(u64, i32, u32)> = Vec::with_capacity(256);
    loop {
        // Busy-poll-aware wait for at least one completion.
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(e),
        }

        cqes.clear();
        {
            let cq = ring.completion();
            for cqe in cq {
                cqes.push((cqe.user_data(), cqe.result(), cqe.flags()));
            }
        }

        for (user_data, res, _flags) in cqes.drain(..) {
            match ud_op(user_data) {
                OP_CTRL_POLL => {
                    let cfd = ud_fd(user_data);
                    drain_control(&mut ring, cfd, &mut conns)?;
                    // Re-arm a one-shot poll on the control fd.
                    arm_ctrl_poll(&mut ring, cfd)?;
                }
                OP_RECV => {
                    let fd = ud_fd(user_data);
                    on_recv(&mut ring, fd, res, &mut conns, &state)?;
                }
                OP_SEND => {
                    let fd = ud_fd(user_data);
                    on_send(&mut ring, fd, res, &mut conns, &state)?;
                }
                _ => {}
            }
        }
    }
}

fn accept_control_blocking(listener: &UnixListener, control: &mut Option<RawFd>) -> io::Result<()> {
    // Block until the LB connects (it does so at startup).
    listener.set_nonblocking(false)?;
    let (stream, _) = listener.accept()?;
    let fd = stream.as_raw_fd();
    std::mem::forget(stream); // keep the fd alive; we own it via `control`
    set_nonblocking(fd);
    *control = Some(fd);
    listener.set_nonblocking(true)?;
    Ok(())
}

fn arm_ctrl_poll(ring: &mut IoUring, control_fd: RawFd) -> io::Result<()> {
    let poll = opcode::PollAdd::new(types::Fd(control_fd), libc::POLLIN as u32)
        .build()
        .user_data(ud(OP_CTRL_POLL, control_fd));
    unsafe {
        push(ring, &poll)?;
    }
    Ok(())
}

fn drain_control(
    ring: &mut IoUring,
    control_fd: RawFd,
    conns: &mut HashMap<RawFd, Box<Conn>>,
) -> io::Result<()> {
    loop {
        match recv_fd(control_fd) {
            Ok(Some(client_fd)) => {
                set_nonblocking(client_fd);
                tune_client_fd(client_fd);
                let mut conn = Box::new(Conn::new());
                arm_recv(ring, client_fd, &mut conn)?;
                conns.insert(client_fd, conn);
            }
            Ok(None) => return Ok(()),
            Err(ref e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(()),
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => return Ok(()),
        }
    }
}

fn arm_recv(ring: &mut IoUring, fd: RawFd, conn: &mut Conn) -> io::Result<()> {
    let cap = conn.buf.len();
    if conn.len >= cap {
        return Ok(()); // buffer full; handled in on_recv
    }
    let ptr = unsafe { conn.buf.as_mut_ptr().add(conn.len) };
    let recv = opcode::Recv::new(types::Fd(fd), ptr, (cap - conn.len) as u32)
        .build()
        .user_data(ud(OP_RECV, fd));
    unsafe {
        push(ring, &recv)?;
    }
    conn.recv_armed = true;
    Ok(())
}

fn on_recv(
    ring: &mut IoUring,
    fd: RawFd,
    res: i32,
    conns: &mut HashMap<RawFd, Box<Conn>>,
    state: &AppState,
) -> io::Result<()> {
    {
        let Some(conn) = conns.get_mut(&fd) else {
            return Ok(());
        };
        conn.recv_armed = false;
        if res > 0 {
            conn.len += res as usize;
            set_quickack(fd);
            advance(ring, fd, conn, state)?;
            return Ok(());
        }
    }
    // res <= 0: peer closed / error.
    close_conn(fd, conns);
    Ok(())
}

fn on_send(
    ring: &mut IoUring,
    fd: RawFd,
    res: i32,
    conns: &mut HashMap<RawFd, Box<Conn>>,
    state: &AppState,
) -> io::Result<()> {
    {
        let Some(conn) = conns.get_mut(&fd) else {
            return Ok(());
        };
        if res > 0 {
            if let Some((data, off)) = conn.pending.take() {
                let new_off = off + res as usize;
                if new_off < data.len() {
                    // partial send: continue from new_off
                    conn.pending = Some((data, new_off));
                    let send = opcode::Send::new(
                        types::Fd(fd),
                        unsafe { data.as_ptr().add(new_off) },
                        (data.len() - new_off) as u32,
                    )
                    .build()
                    .user_data(ud(OP_SEND, fd));
                    unsafe {
                        push(ring, &send)?;
                    }
                    return Ok(());
                }
            }
            if !conn.close_after {
                // keep-alive: drain any pipelined request, else re-arm recv.
                advance(ring, fd, conn, state)?;
                return Ok(());
            }
        }
    }
    close_conn(fd, conns);
    Ok(())
}

/// Process complete requests already buffered; submit a Send for the first one
/// found (one in-flight op per fd), or arm a Recv if more data is needed.
fn advance(ring: &mut IoUring, fd: RawFd, conn: &mut Conn, state: &AppState) -> io::Result<()> {
    match parse_handoff_request(&conn.buf[..conn.len]) {
        Ok(None) => {
            if conn.len == conn.buf.len() {
                submit_send(ring, fd, HTTP_PAYLOAD_TOO_LARGE, true, conn)
            } else {
                arm_recv(ring, fd, conn)
            }
        }
        Ok(Some(req)) => {
            let body = &conn.buf[req.header_len..req.total_len];
            let resp = process_request(state, body, req.route);
            let total = req.total_len;
            let close = req.close_after_response;
            compact(conn, total);
            submit_send(ring, fd, resp, close, conn)
        }
        Err(err) => submit_send(ring, fd, err.response, true, conn),
    }
}

fn submit_send(
    ring: &mut IoUring,
    fd: RawFd,
    resp: &'static [u8],
    close: bool,
    conn: &mut Conn,
) -> io::Result<()> {
    conn.pending = Some((resp, 0));
    conn.close_after = close;
    let send = opcode::Send::new(types::Fd(fd), resp.as_ptr(), resp.len() as u32)
        .build()
        .user_data(ud(OP_SEND, fd));
    unsafe { push(ring, &send) }
}

fn close_conn(fd: RawFd, conns: &mut HashMap<RawFd, Box<Conn>>) {
    conns.remove(&fd);
    unsafe {
        libc::close(fd);
    }
}

/// Push an SQE, submitting to make room if the queue is full.
unsafe fn push(ring: &mut IoUring, entry: &io_uring::squeue::Entry) -> io::Result<()> {
    loop {
        if ring.submission().push(entry).is_ok() {
            return Ok(());
        }
        ring.submit()?;
    }
}

// ===== Multishot recv + provided buffer ring (API_URING_MULTISHOT=1) =========
// Eliminates the per-request recv: one RecvMulti per fd, the kernel keeps
// delivering chunks into a registered buffer pool. We copy each chunk into the
// per-conn accumulator (parsing stays identical), recycle the provided buffer
// immediately, and submit a Send for completed requests (one in flight per fd).

const BGID: u16 = 1;
const BUF_COUNT: u16 = 512; // power of 2
const BUF_SIZE: usize = 16 * 1024; // >= HANDOFF_BUFFER_BYTES

/// Owns the registered buf_ring memory + backing buffer pool.
struct BufPool {
    ring: *mut BufRingEntry,
    ring_bytes: usize,
    bufs: Vec<u8>,
    mask: u16,
    tail: u16,
}

impl BufPool {
    unsafe fn new() -> io::Result<Self> {
        let entries = BUF_COUNT as usize;
        let ring_bytes = entries * std::mem::size_of::<BufRingEntry>();
        // Page-aligned (mmap) as required by the buf_ring interface.
        let ring = libc::mmap(
            std::ptr::null_mut(),
            ring_bytes,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANONYMOUS | libc::MAP_PRIVATE,
            -1,
            0,
        );
        if ring == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let mut pool = BufPool {
            ring: ring as *mut BufRingEntry,
            ring_bytes,
            bufs: vec![0u8; entries * BUF_SIZE],
            mask: BUF_COUNT - 1,
            tail: 0,
        };
        for bid in 0..BUF_COUNT {
            pool.publish(bid);
        }
        Ok(pool)
    }

    #[inline]
    fn buf_addr(&self, bid: u16) -> u64 {
        unsafe { self.bufs.as_ptr().add(bid as usize * BUF_SIZE) as u64 }
    }

    #[inline]
    fn buffer(&self, bid: u16, len: usize) -> &[u8] {
        let start = bid as usize * BUF_SIZE;
        &self.bufs[start..start + len.min(BUF_SIZE)]
    }

    /// Publish/recycle buffer `bid` into the ring and advance the tail (release).
    unsafe fn publish(&mut self, bid: u16) {
        let idx = (self.tail & self.mask) as usize;
        let entry = &mut *self.ring.add(idx);
        entry.set_addr(self.buf_addr(bid));
        entry.set_len(BUF_SIZE as u32);
        entry.set_bid(bid);
        self.tail = self.tail.wrapping_add(1);
        let tail_ptr = BufRingEntry::tail(self.ring) as *mut u16;
        AtomicU16::from_ptr(tail_ptr).store(self.tail, Ordering::Release);
    }
}

impl Drop for BufPool {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ring as *mut libc::c_void, self.ring_bytes);
        }
    }
}

fn run_multishot(listen: String, state: AppState) -> io::Result<()> {
    let path = listen.strip_prefix("unix:").unwrap_or(&listen).to_string();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    eprintln!("FD handoff server (io_uring, multishot+bufring) listening on {}", path);

    let mut ring = IoUring::builder()
        .setup_single_issuer()
        .setup_defer_taskrun()
        .build(4096)?;
    configure_napi(&ring);

    let mut pool = unsafe { BufPool::new()? };
    unsafe {
        ring.submitter()
            .register_buf_ring_with_flags(pool.ring as u64, BUF_COUNT, BGID, 0)?;
    }
    eprintln!("registered buf_ring bgid={} count={} size={}", BGID, BUF_COUNT, BUF_SIZE);

    let mut control: Option<RawFd> = None;
    let mut conns: HashMap<RawFd, Box<Conn>> = HashMap::new();
    accept_control_blocking(&listener, &mut control)?;
    if let Some(cfd) = control {
        arm_ctrl_poll(&mut ring, cfd)?;
        ring.submit()?;
    }

    let mut cqes: Vec<(u64, i32, u32)> = Vec::with_capacity(512);
    loop {
        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(e),
        }
        cqes.clear();
        {
            for cqe in ring.completion() {
                cqes.push((cqe.user_data(), cqe.result(), cqe.flags()));
            }
        }
        for (user_data, res, flags) in cqes.drain(..) {
            match ud_op(user_data) {
                OP_CTRL_POLL => {
                    let cfd = ud_fd(user_data);
                    drain_control_multi(&mut ring, cfd, &mut conns)?;
                    arm_ctrl_poll(&mut ring, cfd)?;
                }
                OP_RECV => {
                    on_recv_multi(&mut ring, ud_fd(user_data), res, flags, &mut conns, &mut pool, &state)?;
                }
                OP_SEND => {
                    on_send_multi(&mut ring, ud_fd(user_data), res, &mut conns, &state)?;
                }
                _ => {}
            }
        }
    }
}

fn drain_control_multi(
    ring: &mut IoUring,
    control_fd: RawFd,
    conns: &mut HashMap<RawFd, Box<Conn>>,
) -> io::Result<()> {
    loop {
        match recv_fd(control_fd) {
            Ok(Some(client_fd)) => {
                set_nonblocking(client_fd);
                tune_client_fd(client_fd);
                conns.insert(client_fd, Box::new(Conn::new()));
                arm_recv_multi(ring, client_fd)?;
            }
            Ok(None) => return Ok(()),
            Err(ref e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(()),
            Err(ref e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(_) => return Ok(()),
        }
    }
}

fn arm_recv_multi(ring: &mut IoUring, fd: RawFd) -> io::Result<()> {
    let recv = opcode::RecvMulti::new(types::Fd(fd), BGID)
        .build()
        .user_data(ud(OP_RECV, fd));
    unsafe { push(ring, &recv) }
}

#[allow(clippy::too_many_arguments)]
fn on_recv_multi(
    ring: &mut IoUring,
    fd: RawFd,
    res: i32,
    flags: u32,
    conns: &mut HashMap<RawFd, Box<Conn>>,
    pool: &mut BufPool,
    state: &AppState,
) -> io::Result<()> {
    let bid = cqueue::buffer_select(flags);
    let more = cqueue::more(flags);
    let mut should_close = false;
    {
        let Some(conn) = conns.get_mut(&fd) else {
            // fd already gone: just return the buffer to the kernel.
            if let Some(b) = bid {
                unsafe { pool.publish(b) };
            }
            return Ok(());
        };
        if res > 0 {
            if let Some(b) = bid {
                let n = res as usize;
                let space = conn.buf.len() - conn.len;
                let take = n.min(space);
                conn.buf[conn.len..conn.len + take].copy_from_slice(pool.buffer(b, take));
                conn.len += take;
                unsafe { pool.publish(b) }; // recycle AFTER copying out
            }
            set_quickack(fd);
            advance_multi(ring, fd, conn, state)?;
        } else if res == -libc::ENOBUFS {
            // pool momentarily exhausted; re-arm below.
        } else {
            should_close = true; // 0 = peer closed, or error
        }
        // Multishot terminated (more=false) and still open -> re-arm.
        if !more && !should_close {
            arm_recv_multi(ring, fd)?;
        }
    }
    if should_close {
        close_conn(fd, conns);
    }
    Ok(())
}

/// Like `advance`, but never arms a Recv (multishot delivers automatically) and
/// only acts when no Send is in flight (one outstanding response per fd).
fn advance_multi(ring: &mut IoUring, fd: RawFd, conn: &mut Conn, state: &AppState) -> io::Result<()> {
    if conn.pending.is_some() {
        return Ok(());
    }
    match parse_handoff_request(&conn.buf[..conn.len]) {
        Ok(None) => {
            if conn.len == conn.buf.len() {
                submit_send(ring, fd, HTTP_PAYLOAD_TOO_LARGE, true, conn)
            } else {
                Ok(()) // wait for more multishot data
            }
        }
        Ok(Some(req)) => {
            let body = &conn.buf[req.header_len..req.total_len];
            let resp = process_request(state, body, req.route);
            let total = req.total_len;
            let close = req.close_after_response;
            compact(conn, total);
            submit_send(ring, fd, resp, close, conn)
        }
        Err(err) => submit_send(ring, fd, err.response, true, conn),
    }
}

fn on_send_multi(
    ring: &mut IoUring,
    fd: RawFd,
    res: i32,
    conns: &mut HashMap<RawFd, Box<Conn>>,
    state: &AppState,
) -> io::Result<()> {
    {
        let Some(conn) = conns.get_mut(&fd) else {
            return Ok(());
        };
        if res > 0 {
            if let Some((data, off)) = conn.pending.take() {
                let new_off = off + res as usize;
                if new_off < data.len() {
                    conn.pending = Some((data, new_off));
                    let send = opcode::Send::new(
                        types::Fd(fd),
                        unsafe { data.as_ptr().add(new_off) },
                        (data.len() - new_off) as u32,
                    )
                    .build()
                    .user_data(ud(OP_SEND, fd));
                    unsafe {
                        push(ring, &send)?;
                    }
                    return Ok(());
                }
            }
            if !conn.close_after {
                advance_multi(ring, fd, conn, state)?;
                return Ok(());
            }
        }
    }
    close_conn(fd, conns);
    Ok(())
}

//! Single-threaded epoll reactor for the FD-handoff API hot path (Linux only).
//!
//! This replaces the blocking-thread-per-FD model in `main.rs` so the receive
//! side can use NAPI busy-poll via `EPIOCSPARAMS` (the per-epoll-context busy
//! poll, kernel >= 6.9). That is the *unprivileged* busy-poll path: socket-level
//! `SO_BUSY_POLL` needs `CAP_NET_ADMIN`, but `EPIOCSPARAMS` with
//! `busy_poll_budget <= NAPI_POLL_WEIGHT (64)` does not, so it works under the
//! Rinha `privileged: false` constraint.
//!
//! Scoring semantics are unchanged: this module only owns I/O and reuses the
//! existing parser (`crate::parse_handoff_request`), scorer
//! (`crate::score_body*`), and pre-rendered responses
//! (`crate::handoff_response_for_bucket`).

#![cfg(target_os = "linux")]

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::{
    handoff_response_for_bucket, parse_handoff_request, score_body, score_body_fast_bucket,
    ApiParser, AppState, HandoffRoute, HANDOFF_BUFFER_BYTES, HTTP_BAD_REQUEST,
    HTTP_PAYLOAD_TOO_LARGE, HTTP_READY_OK,
};

/// `struct epoll_params` from `<sys/epoll.h>` (kernel >= 6.9). Layout must match
/// the kernel exactly: 4 + 2 + 1 + 1 = 8 bytes, naturally aligned.
#[repr(C)]
#[derive(Clone, Copy)]
struct EpollParams {
    busy_poll_usecs: u32,
    busy_poll_budget: u16,
    prefer_busy_poll: u8,
    __pad: u8,
}

// EPIOCSPARAMS = _IOW('p', 0x01, struct epoll_params) = 0x40087001.
const EPIOCSPARAMS: libc::c_ulong = 0x4008_7001;

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

/// Configure NAPI busy-poll on the epoll context. Graceful: on any failure
/// (kernel < 6.9 -> ENOTTY, unsupported, etc.) we log and continue — the loop
/// falls back to plain `epoll_wait(timeout)`. This honours the project rule
/// "never fail to boot / 0 5xx".
fn configure_busy_poll(epfd: RawFd) {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_else(|_| "unknown".to_string());
    let usecs = env_u32("API_BUSY_POLL_US", 50);
    let budget = env_u32("API_BUSY_POLL_BUDGET", 8).min(u16::MAX as u32) as u16;
    let prefer = env_u32("API_PREFER_BUSY_POLL", 1).min(1) as u8;

    if usecs == 0 && prefer == 0 {
        eprintln!("epoll busy-poll disabled (API_BUSY_POLL_US=0); kernel {}", kernel.trim());
        return;
    }

    let params = EpollParams {
        busy_poll_usecs: usecs,
        busy_poll_budget: budget,
        prefer_busy_poll: prefer,
        __pad: 0,
    };
    let rc = unsafe { libc::ioctl(epfd, EPIOCSPARAMS, &params as *const EpollParams) };
    if rc == 0 {
        eprintln!(
            "epoll NAPI busy-poll enabled via EPIOCSPARAMS: usecs={} budget={} prefer={} (kernel {})",
            usecs,
            budget,
            prefer,
            kernel.trim()
        );
    } else {
        let err = io::Error::last_os_error();
        eprintln!(
            "EPIOCSPARAMS unavailable ({}); falling back to plain epoll_wait. kernel {} (NAPI busy-poll needs >= 6.9)",
            err,
            kernel.trim()
        );
    }
}

/// Optional in-process CPU pinning fallback (works inside the cgroup's allowed
/// CPU set even when docker-compose `cpuset` is ignored). Enabled via
/// `API_PIN_CPU=<index>`.
fn maybe_pin_cpu() {
    let Some(cpu) = std::env::var("API_PIN_CPU").ok().and_then(|v| v.parse::<usize>().ok()) else {
        return;
    };
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc == 0 {
            eprintln!("pinned process to CPU {}", cpu);
        } else {
            eprintln!("failed to pin to CPU {}: {}", cpu, io::Error::last_os_error());
        }
    }
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Best-effort client-socket tuning. `TCP_NODELAY`/`TCP_QUICKACK` are the
/// effective ones under `privileged: false`; `SO_BUSY_POLL` family is attempted
/// but typically no-ops without `CAP_NET_ADMIN` (the EPIOCSPARAMS path is what
/// actually delivers busy-poll).
fn tune_client_fd(fd: RawFd) {
    unsafe {
        let one: libc::c_int = 1;
        let one_ptr = &one as *const libc::c_int as *const libc::c_void;
        let len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let _ = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, one_ptr, len);
        let _ = libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_QUICKACK, one_ptr, len);
        let busy: libc::c_int = env_u32("API_BUSY_POLL_US", 50) as libc::c_int;
        let busy_ptr = &busy as *const libc::c_int as *const libc::c_void;
        let _ = libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_BUSY_POLL, busy_ptr, len);
        // SO_PREFER_BUSY_POLL = 69, SO_BUSY_POLL_BUDGET = 70 (since 5.7).
        let prefer: libc::c_int = env_u32("API_PREFER_BUSY_POLL", 1) as libc::c_int;
        let prefer_ptr = &prefer as *const libc::c_int as *const libc::c_void;
        let _ = libc::setsockopt(fd, libc::SOL_SOCKET, 69, prefer_ptr, len);
        let budget: libc::c_int = env_u32("API_BUSY_POLL_BUDGET", 8) as libc::c_int;
        let budget_ptr = &budget as *const libc::c_int as *const libc::c_void;
        let _ = libc::setsockopt(fd, libc::SOL_SOCKET, 70, budget_ptr, len);
    }
}

fn epoll_add(epfd: RawFd, fd: RawFd, events: u32) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: fd as u64 };
    let rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn epoll_mod(epfd: RawFd, fd: RawFd, events: u32) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: fd as u64 };
    let rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut ev) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn epoll_del(epfd: RawFd, fd: RawFd) {
    let mut ev = libc::epoll_event { events: 0, u64: fd as u64 };
    unsafe {
        let _ = libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, &mut ev);
    }
}

/// Non-blocking single-fd SCM_RIGHTS receive. Returns:
/// - `Ok(Some(fd))` on a received fd,
/// - `Ok(None)` when the peer closed the control connection,
/// - `Err(EAGAIN)` when there is nothing to read right now (caller treats as drained),
/// - other `Err` on a real failure.
fn recv_fd_nonblocking(control_fd: RawFd) -> io::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control_buf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control_buf.as_mut_ptr().cast();
    msg.msg_controllen = control_buf.len() as _;

    let received =
        unsafe { libc::recvmsg(control_fd, &mut msg, libc::MSG_DONTWAIT | libc::MSG_CMSG_CLOEXEC) };
    if received == 0 {
        return Ok(None);
    }
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "control message did not contain fd",
            ));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(
            libc::CMSG_DATA(cmsg).cast::<u8>(),
            &mut fd as *mut RawFd as *mut u8,
            std::mem::size_of::<RawFd>(),
        );
        if fd < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "received invalid fd"));
        }
        Ok(Some(fd))
    }
}

enum WriteState {
    Done,
    Pending(usize),
    Closed,
}

fn try_write(fd: RawFd, data: &[u8], mut off: usize) -> WriteState {
    loop {
        if off >= data.len() {
            return WriteState::Done;
        }
        let remaining = &data[off..];
        let n = unsafe {
            libc::write(fd, remaining.as_ptr().cast(), remaining.len())
        };
        if n > 0 {
            off += n as usize;
            continue;
        }
        if n == 0 {
            return WriteState::Closed;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // EWOULDBLOCK == EAGAIN on Linux.
            Some(libc::EAGAIN) => return WriteState::Pending(off),
            Some(libc::EINTR) => continue,
            _ => return WriteState::Closed,
        }
    }
}

struct Conn {
    buf: Box<[u8]>,
    len: usize,
    /// Remaining response bytes when a write went partial; `(slice, offset)`.
    pending: Option<(&'static [u8], usize)>,
    close_after: bool,
    want_out: bool,
}

impl Conn {
    fn new() -> Self {
        Conn {
            buf: vec![0u8; HANDOFF_BUFFER_BYTES].into_boxed_slice(),
            len: 0,
            pending: None,
            close_after: false,
            want_out: false,
        }
    }
}

enum Outcome {
    Keep,
    Close,
}

/// Per-stage I/O instrumentation for the winning tree_only+epoll path (which the
/// PerfCollector does not cover). Gated by API_IO_TRACE=1; zero overhead when
/// off (no Instant::now calls). Measures the CPU cost of each request stage so
/// we can see how little of warm-p99 is CPU vs I/O/wakeup latency.
struct IoTrace {
    enabled: bool,
    every: usize,
    read_ns: Vec<u64>,
    score_ns: Vec<u64>,
    write_ns: Vec<u64>,
}

impl IoTrace {
    fn from_env() -> Self {
        let enabled = std::env::var("API_IO_TRACE").ok().as_deref() == Some("1");
        let every = std::env::var("API_IO_TRACE_EVERY")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(50_000)
            .max(1);
        if enabled {
            eprintln!("API_IO_TRACE on: per-stage read/score/write ns, summary every {}", every);
        }
        IoTrace {
            enabled,
            every,
            read_ns: Vec::new(),
            score_ns: Vec::new(),
            write_ns: Vec::new(),
        }
    }

    #[inline]
    fn on(&self) -> bool {
        self.enabled
    }

    #[inline]
    fn push_read(&mut self, ns: u64) {
        if self.enabled {
            self.read_ns.push(ns);
        }
    }

    #[inline]
    fn push_score(&mut self, ns: u64) {
        if self.enabled {
            self.score_ns.push(ns);
        }
    }

    #[inline]
    fn push_write(&mut self, ns: u64) {
        if self.enabled {
            self.write_ns.push(ns);
            if self.score_ns.len() >= self.every {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        eprintln!(
            "{{\"kind\":\"io_trace\",\"samples\":{},\"read_ns\":{},\"score_ns\":{},\"write_ns\":{}}}",
            self.score_ns.len(),
            stat(&self.read_ns),
            stat(&self.score_ns),
            stat(&self.write_ns),
        );
        self.read_ns.clear();
        self.score_ns.clear();
        self.write_ns.clear();
    }
}

fn stat(values: &[u64]) -> String {
    if values.is_empty() {
        return "{\"p50\":0,\"p99\":0,\"max\":0,\"avg\":0.0}".to_string();
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let pct = |p: usize| -> u64 {
        let idx = ((v.len() - 1) * p).div_ceil(100);
        v[idx]
    };
    let sum: u128 = v.iter().map(|&x| x as u128).sum();
    format!(
        "{{\"p50\":{},\"p99\":{},\"max\":{},\"avg\":{:.1}}}",
        pct(50),
        pct(99),
        v[v.len() - 1],
        sum as f64 / v.len() as f64
    )
}

/// Compute the response for a fully-buffered request, mirroring the blocking
/// handler in `main.rs` exactly (same perf/parser/classifier branching).
fn process_request(
    state: &AppState,
    body: &[u8],
    route: HandoffRoute,
) -> &'static [u8] {
    match route {
        HandoffRoute::Ready => HTTP_READY_OK,
        HandoffRoute::FraudScore => {
            let handler_start = if state.perf.is_some() || state.log_search_avg {
                Some(Instant::now())
            } else {
                None
            };
            let bucket_result = if state.perf.is_none()
                && !state.log_search_avg
                && state.parser == ApiParser::Fast
            {
                score_body_fast_bucket(state, body)
            } else {
                score_body(state, body, handler_start, false)
            };
            match bucket_result {
                Ok(bucket) => handoff_response_for_bucket(bucket),
                Err(_) => HTTP_BAD_REQUEST,
            }
        }
    }
}

/// Drain and respond to every complete request currently buffered. Returns
/// `Some(Outcome)` when the caller should stop (connection closing, or a write
/// went pending and we now wait on EPOLLOUT); `None` when the buffer is drained
/// and more socket data is needed.
fn process_buffered(
    epfd: RawFd,
    fd: RawFd,
    conn: &mut Conn,
    state: &AppState,
    trace: &mut IoTrace,
) -> Option<Outcome> {
    loop {
        if conn.len == 0 {
            return None;
        }
        match parse_handoff_request(&conn.buf[..conn.len]) {
            Ok(None) => {
                if conn.len == conn.buf.len() {
                    let _ = try_write(fd, HTTP_PAYLOAD_TOO_LARGE, 0);
                    return Some(Outcome::Close);
                }
                return None;
            }
            Ok(Some(req)) => {
                let body = &conn.buf[req.header_len..req.total_len];
                let t_score = if trace.on() { Some(Instant::now()) } else { None };
                let resp = process_request(state, body, req.route);
                if let Some(t) = t_score {
                    trace.push_score(t.elapsed().as_nanos() as u64);
                }
                let total_len = req.total_len;
                let close = req.close_after_response;
                let t_write = if trace.on() { Some(Instant::now()) } else { None };
                let write_state = try_write(fd, resp, 0);
                if let Some(t) = t_write {
                    trace.push_write(t.elapsed().as_nanos() as u64);
                }
                match write_state {
                    WriteState::Done => {
                        compact(conn, total_len);
                        if close {
                            return Some(Outcome::Close);
                        }
                        // continue draining further pipelined requests
                    }
                    WriteState::Pending(off) => {
                        compact(conn, total_len);
                        conn.pending = Some((resp, off));
                        conn.close_after = close;
                        if !conn.want_out {
                            if epoll_mod(epfd, fd, libc::EPOLLOUT as u32).is_err() {
                                return Some(Outcome::Close);
                            }
                            conn.want_out = true;
                        }
                        return Some(Outcome::Keep);
                    }
                    WriteState::Closed => return Some(Outcome::Close),
                }
            }
            Err(err) => {
                let _ = try_write(fd, err.response, 0);
                return Some(Outcome::Close);
            }
        }
    }
}

/// Whether to re-arm `TCP_QUICKACK` after every read (the flag is one-shot;
/// the kernel clears it after each ACK). Default OFF: this adds a `setsockopt`
/// to the per-request hot path, which regressed p99 on the target (preview
/// #7385: 0.387 -> 0.403ms). Toggle with `API_QUICKACK_REARM=1` for A/B. The
/// one-time QUICKACK set in `tune_client_fd` stays regardless.
static REARM_QUICKACK: AtomicBool = AtomicBool::new(false);

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

fn compact(conn: &mut Conn, consumed: usize) {
    if consumed >= conn.len {
        conn.len = 0;
    } else {
        conn.buf.copy_within(consumed..conn.len, 0);
        conn.len -= consumed;
    }
}

/// Drive one connection in response to an epoll event (EPOLLIN/EPOLLOUT).
fn drive(epfd: RawFd, fd: RawFd, conn: &mut Conn, state: &AppState, trace: &mut IoTrace) -> Outcome {
    // 1. Flush any pending write first.
    if let Some((data, off)) = conn.pending.take() {
        match try_write(fd, data, off) {
            WriteState::Pending(o) => {
                conn.pending = Some((data, o));
                return Outcome::Keep;
            }
            WriteState::Closed => return Outcome::Close,
            WriteState::Done => {
                if conn.close_after {
                    return Outcome::Close;
                }
                if conn.want_out {
                    if epoll_mod(epfd, fd, libc::EPOLLIN as u32).is_err() {
                        return Outcome::Close;
                    }
                    conn.want_out = false;
                }
            }
        }
    }

    // 2. Process buffered requests, then read more, repeat until EAGAIN.
    loop {
        if let Some(outcome) = process_buffered(epfd, fd, conn, state, trace) {
            return outcome;
        }
        // Buffer drained; read more from the socket.
        let cap = conn.buf.len();
        if conn.len >= cap {
            // Full buffer with no complete request was handled in process_buffered.
            return Outcome::Close;
        }
        let t_read = if trace.on() { Some(Instant::now()) } else { None };
        let n = unsafe {
            let dst = conn.buf.as_mut_ptr().add(conn.len);
            libc::read(fd, dst.cast(), cap - conn.len)
        };
        if n == 0 {
            return Outcome::Close;
        }
        if n > 0 {
            if let Some(t) = t_read {
                trace.push_read(t.elapsed().as_nanos() as u64);
            }
            conn.len += n as usize;
            // Re-arm QUICKACK so the response's ACK isn't delayed. Gated behind
            // API_QUICKACK_REARM (default OFF): on the target this extra per-read
            // setsockopt regressed p99 (preview #7385: 0.387 -> 0.403ms). The
            // one-time set in tune_client_fd remains.
            if REARM_QUICKACK.load(Ordering::Relaxed) {
                set_quickack(fd);
            }
            continue;
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            // EWOULDBLOCK == EAGAIN on Linux.
            Some(libc::EAGAIN) => return Outcome::Keep,
            Some(libc::EINTR) => continue,
            _ => return Outcome::Close,
        }
    }
}

/// Entry point: run the FD-handoff API on a single-threaded epoll reactor.
pub fn run(listen: String, state: AppState) -> io::Result<()> {
    maybe_pin_cpu();

    let path = listen.strip_prefix("unix:").unwrap_or(&listen).to_string();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let listener_fd = listener.as_raw_fd();
    eprintln!("FD handoff server (epoll) listening on {}", path);

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        return Err(io::Error::last_os_error());
    }
    configure_busy_poll(epfd);

    let rearm = env_u32("API_QUICKACK_REARM", 0) != 0;
    REARM_QUICKACK.store(rearm, Ordering::Relaxed);
    eprintln!(
        "per-request QUICKACK re-arm: {}",
        if rearm { "on" } else { "off (default)" }
    );

    epoll_add(epfd, listener_fd, libc::EPOLLIN as u32)?;

    let timeout_ms = env_u32("API_EPOLL_TIMEOUT_MS", 1) as libc::c_int;
    let max_events = 1024usize;
    let mut events: Vec<libc::epoll_event> =
        vec![libc::epoll_event { events: 0, u64: 0 }; max_events];

    let mut control: HashSet<RawFd> = HashSet::new();
    let mut conns: HashMap<RawFd, Conn> = HashMap::new();
    let mut io_trace = IoTrace::from_env();

    loop {
        let n = unsafe {
            libc::epoll_wait(epfd, events.as_mut_ptr(), max_events as libc::c_int, timeout_ms)
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }

        for ev in events.iter().take(n as usize) {
            let fd = ev.u64 as RawFd;
            let flags = ev.events;

            if fd == listener_fd {
                accept_control(epfd, listener_fd, &mut control);
                continue;
            }

            if control.contains(&fd) {
                drain_control(epfd, fd, &mut control, &mut conns);
                continue;
            }

            // Client fd.
            if let Some(conn) = conns.get_mut(&fd) {
                // EPOLLERR/EPOLLHUP without readable data -> read()/write() will
                // surface the close; let drive() handle it.
                let _ = flags;
                match drive(epfd, fd, conn, &state, &mut io_trace) {
                    Outcome::Keep => {}
                    Outcome::Close => {
                        epoll_del(epfd, fd);
                        conns.remove(&fd);
                        unsafe {
                            libc::close(fd);
                        }
                    }
                }
            } else {
                // Unknown fd (already closed): defensively deregister.
                epoll_del(epfd, fd);
            }
        }
    }
}

fn accept_control(epfd: RawFd, listener_fd: RawFd, control: &mut HashSet<RawFd>) {
    loop {
        let cfd = unsafe {
            libc::accept4(
                listener_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if cfd < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                // EWOULDBLOCK == EAGAIN on Linux.
            Some(libc::EAGAIN) => return,
                Some(libc::EINTR) => continue,
                _ => return,
            }
        }
        if epoll_add(epfd, cfd, libc::EPOLLIN as u32).is_err() {
            unsafe {
                libc::close(cfd);
            }
            continue;
        }
        control.insert(cfd);
    }
}

fn drain_control(
    epfd: RawFd,
    control_fd: RawFd,
    control: &mut HashSet<RawFd>,
    conns: &mut HashMap<RawFd, Conn>,
) {
    loop {
        match recv_fd_nonblocking(control_fd) {
            Ok(Some(client_fd)) => {
                if register_client(epfd, client_fd, conns).is_err() {
                    unsafe {
                        libc::close(client_fd);
                    }
                }
            }
            Ok(None) => {
                // Control peer closed.
                epoll_del(epfd, control_fd);
                control.remove(&control_fd);
                unsafe {
                    libc::close(control_fd);
                }
                return;
            }
            Err(err) => match err.raw_os_error() {
                // EWOULDBLOCK == EAGAIN on Linux.
            Some(libc::EAGAIN) => return,
                Some(libc::EINTR) => continue,
                _ => {
                    epoll_del(epfd, control_fd);
                    control.remove(&control_fd);
                    unsafe {
                        libc::close(control_fd);
                    }
                    return;
                }
            },
        }
    }
}

fn register_client(epfd: RawFd, client_fd: RawFd, conns: &mut HashMap<RawFd, Conn>) -> io::Result<()> {
    // Take ownership so it is closed if anything below fails before we insert.
    let owned = unsafe { OwnedFd::from_raw_fd(client_fd) };
    set_nonblocking(client_fd)?;
    tune_client_fd(client_fd);
    epoll_add(epfd, client_fd, libc::EPOLLIN as u32)?;
    conns.insert(client_fd, Conn::new());
    // Ownership now tracked by `conns`; don't let the guard close it.
    std::mem::forget(owned);
    Ok(())
}

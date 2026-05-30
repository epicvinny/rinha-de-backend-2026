# Stage 7 design: io_uring reactor (next bet to beat 0.366ms)

Rationale: `docs/perf-bottlenecks.md` shows warm-p99 is dominated by epoll **wakeup
latency + per-request syscalls** (`read`/`write`), not CPU (~4µs). io_uring can attack
all three at once. Nobody in the top-3 used it — the unexplored path.

## Crate: `io-uring` 0.7.12 (tokio-rs), API confirmed from source

- Builder: `IoUring::builder().setup_single_issuer().setup_defer_taskrun().build(entries)`.
  (Optional `setup_sqpoll(idle_ms)` — but SQPOLL burns a core; gate off by default, the
  0.40-CPU/API budget likely makes it net-negative.)
- `ring.split() -> (submitter, sq, cq)`; `sq.push(&sqe)`, `submitter.submit()` / `submit_and_wait(n)`, `cq` iterator of cqes.
- Provided buffer ring (zero syscall/buffer): `submitter.register_buf_ring_with_flags(ring_addr, ring_entries, bgid, flags)`; entries are `types::BufRingEntry` (set addr/len/bid); we own the ring memory + the backing buffer pool; recycle a buffer by re-publishing its entry after processing.
- Multishot: `opcode::AcceptMulti::new(fd, ...)`, and `opcode::Recv::new(...).buf_group(bgid)` (or `RecvMsgMulti`) — one SQE yields many CQEs; `cqueue::more(flags)` true while the multishot is still armed (re-arm when it clears).
- CQE: `cqueue::buffer_select(flags) -> Option<u16>` gives the buffer id that was filled.
- **NAPI busy-poll:** the safe API does NOT expose `register_napi` in 0.7.12. Call it raw:
  `io_uring_register(ring_fd, IORING_REGISTER_NAPI=27, &io_uring_napi{busy_poll_to:50, prefer_busy_poll:1}, 1)` via `libc::syscall(SYS_io_uring_register, ...)`. Graceful fallback on error (kernel <6.9) — log + continue, exactly like the EPIOCSPARAMS path in `epoll_server.rs`.

## Reactor shape (mirror `epoll_server.rs`, swap epoll for the ring)

- One ring per API process, `SINGLE_ISSUER | DEFER_TASKRUN`, NAPI registered (graceful).
- Control Unix socket: a (re-armed) `RecvMsg` SQE to receive SCM_RIGHTS client fds; on each, register the fd and arm a multishot `Recv` with the provided buffer ring.
- Per client fd: multishot `Recv` (buf_group) → CQE carries a buffer id + len. Reuse the
  existing `parse_handoff_request` + `score_body_fast_bucket` + `handoff_response_for_bucket`
  (scoring byte-identical). Submit a `Send`/`Write` SQE for the response; recycle the recv buffer.
- Keep-alive: multishot recv keeps delivering; close on 0-len/`Connection: close`.
- Gate: `API_FD_URING=1` (default off; epoll stays default). A/B vs epoll on the target.

## Acceptance (same gates as every stage)
0 HTTP 5xx, 0 oracle mismatches, vectorizer byte-equal; measurable per-request syscall
reduction vs epoll (via API_IO_TRACE-style counters); confirm the wakeup win on the ≥6.9 target.

## Open decision
The preview test (upstream #7368) measures the epoll+busy-poll solution on the real Mac
Mini. **That score should size this effort:** if epoll+busy-poll already beats 0.366ms,
io_uring is a marginal/optional refinement; if not, it's the primary lever.

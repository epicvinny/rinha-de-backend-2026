# Handoff & roadmap — Rinha 2026 (as of 2026-05-30)

## ►► START HERE (exact starting point for the next session) ◄◄
- **Branch (local, source — never pushed to a public repo):** `codex/epoll-frontier`
  at commit **`855f588`** (created off `codex/exp-io-uring`; contains the epoll
  reactor + QUICKACK-rearm + LB-pin + all docs; the io_uring code is present but
  SHELVED — do not use it).
- **Live submission image (Docker Hub, single-arch manifest.v2):**
  **`visuzano/rinha-2026:uring-dfd89b3`** — built from commit `dfd89b3` (ancestor of
  `855f588`; the only delta from `855f588` is this handoff doc). On the next image
  rebuild, retag to `epoll-<shortsha>` for clarity (io_uring is dead — the tag name
  is legacy).
- **Live submission branch (registered repo `epicvinny/rinha-de-backend-2026-epicvinny`):**
  `submission` @ **`9225b7a`** → epoll + QUICKACK-rearm + LB-pin, image
  `uring-dfd89b3`, NO seccomp. This is the current preview entry (#7385).
- **Local submission compose under test:** `C:\Users\visuz\rinha-submission\docker-compose.yml`.
- **Read first:** this file, then `CLAUDE.md` (submission procedure + 10/day test
  tracker + dead ends), `docs/perf-bottlenecks.md`, `docs/exp-unikernel-scratch.md`.


## TL;DR
- We went from broken to **4th place, p99 0.387ms, perfect score** (0 FP/FN, 0 5xx).
- Topology: **C `fd_handoff_lb` (SCM_RIGHTS round-robin) + 2× API epoll reactor + EPIOCSPARAMS NAPI busy-poll + `tree_only` classifier.**
- **io_uring is a confirmed DEAD END** (needs `seccomp=unconfined`, which the Rinha forbids).
- An improved build (epoll + per-request `TCP_QUICKACK` re-arm + LB core-pin) is submitted as preview test #7385 (pending).
- Leaderboard to beat: **#1 asm 0.353ms**, #2 cpp 0.357, #3 rust 0.385.

## What was done (this work)
1. **Corrected the strategy** (`docs/exp-unikernel-scratch.md`): the top-1 uses **epoll + EPIOCSPARAMS busy-poll, NOT io_uring** (its README misleads). Busy-poll is the latency floor; verified all three reference repos.
2. **Built the epoll reactor** (`crates/api/src/epoll_server.rs`, `API_FD_EPOLL`, default on): single-thread epoll + EPIOCSPARAMS NAPI busy-poll (graceful fallback <6.9) + `API_PIN_CPU` affinity. Replaced the 120-blocking-thread model. Local A/B: **0.499 vs 0.827ms** (epoll vs threaded).
3. **Measured the real bottleneck** (`docs/perf-bottlenecks.md`, `API_IO_TRACE`): CPU/scoring ≈ **4µs (NOT the bottleneck)**; ~500µs of the p99 is **epoll wakeup + read/write syscalls**.
4. **LB + memory + warmup**: `TCP_DEFER_ACCEPT`/`QUICKACK`/`FASTOPEN` on the LB; `MAP_POPULATE`+`madvise(HUGEPAGE|WILLNEED)`+`mlock` default; classifier warm-up + LB forked self-warm.
5. **Submission pipeline + fix**: single-arch image (`--provenance=false --sbom=false` — attestation indexes break fresh pulls), pushed to Docker Hub, `submission` branch on the **registered repo** (`epicvinny/rinha-de-backend-2026-epicvinny`), `rinha/test` preview issue on upstream. See the procedure in `CLAUDE.md`.
6. **Tunings (preview #7385)**: per-request `TCP_QUICKACK` re-arm (one-shot; worth ~1.2ms per perf-learnings) + LB core-pin via `sched_setaffinity` (`LB_PIN_CPU`, because compose `cpuset` is ignored by the host).
7. **io_uring reactor built + validated locally then shelved** (`crates/api/src/uring_server.rs`, plain + multishot+buf_ring) — dead end, see below.

## Hard environment facts (measured, not assumed)
- Host: Mac Mini Late 2014, 2.6GHz **Haswell (2C/4T = 4 logical)**, 8GB, Ubuntu 24.04. Kernel is **≥6.9** (EPIOCSPARAMS busy-poll engages — that's how we/leaders get sub-0.4ms).
- `privileged: false`, **`CapAdd: null`** (no CAP_NET_ADMIN → `busy_poll_budget` ≤ 64; we use 8).
- **compose `cpuset` is stripped** (no `CpusetCpus` in HostConfig) → pin via in-process `sched_setaffinity` (`API_PIN_CPU`, `LB_PIN_CPU`).
- **`seccomp=unconfined` is NOT allowed** → io_uring (`io_uring_setup/enter/register`) is blocked by default seccomp → **io_uring is unusable**. Same kills `SO_BUSY_POLL` (needs CAP_NET_ADMIN), AF_XDP/DPDK (caps), kernel swap.
- Docker **bridge** networking (no host mode). 1.0 CPU / 350MB total: LB 0.20/30MB, api 0.40/160MB ×2.
- **Preview tests: 10/day cap** — track in `CLAUDE.md`, validate locally (fresh-pull release image) before every submit.

## Dead ends — do NOT re-attempt
- **io_uring** (any flavor): needs `seccomp=unconfined`, forbidden. Code is shelved on `codex/exp-io-uring`.
- **CPU micro-opts** (classifier/parser/SIMD): scoring is ~4µs, 100× below p99. Wasted effort.
- **musl / FROM scratch**: removes glibc *cold-start* overhead only — irrelevant to *warm* p99 (the scored metric). In `tree_only` mode tokio/axum aren't even run.
- **compose `cpuset`**, **`SO_BUSY_POLL`** socket option: ignored / need caps.

## The honest perf model & the 0.10ms question
Per-request warm path under keep-alive (LB is out of the loop after the one-time fd handoff):
`epoll_wait(NAPI busy-poll wakeup) → read() → classify(~4µs) → write() → bridge RTT back`.
The ~387µs decomposes into: **NAPI/scheduler wakeup + 2–3 syscalls + docker-bridge RTT (both ways)**, with CPU negligible.

**Reaching 0.10ms (100µs) is almost certainly infeasible under these constraints.** The world-best on this exact hardware+rules is **0.353ms (hand-written ASM, epoll+busy-poll)**. 100µs would require removing ~250µs that is dominated by **docker-bridge RTT + syscall/wakeup floor** — which need host-mode networking, kernel bypass (AF_XDP/DPDK), or io_uring, all **blocked by the Rinha sandbox** (`privileged:false`, no caps, no seccomp relaxation, bridge-only). So treat 0.10ms as aspirational; **the realistic frontier is ~0.30–0.35ms (beat 0.353).**

## Roadmap for the next session (in priority order, all within the allowed sandbox)
1. **Land #7385** (epoll + QUICKACK-rearm + LB-pin). Expect a small win over 0.387. New baseline.
2. **Tune busy-poll ON TARGET** (1 test each, batch wisely): sweep `API_BUSY_POLL_US` ∈ {25, 50, 100, 200} and `API_BUSY_POLL_BUDGET` ∈ {8, 16, 32, 64}. This is the single biggest target-only knob and can't be tuned on WSL. Pick the best.
3. **Out-ASM the per-request overhead** (the #1 is ASM for a reason): the remaining gap to 0.353 is per-request *runtime/loop* overhead, not CPU math. Rewrite the API hot path as a **C-static or ASM `FROM scratch`** epoll+EPIOCSPARAMS reactor (the existing Rust epoll_server.rs is the spec; port it). Goal: match/exceed the top-1's syscall sequence + flat-array fd state (no HashMap), pre-rendered responses (already have), zero per-request allocation. This is the credible path from ~0.35 → ~0.32.
4. **Micro-loop opts** (measure each on target, they're below WSL noise): flat-array fd state vs HashMap; `recv`/`send` with `MSG_DONTWAIT` tight loop; re-arm QUICKACK timing; epoll vs edge-triggered; minimize the LB→API handoff cost (it's per-connection — ensure keep-alive dominates).
5. **Warm-up depth**: the forked LB self-warm primes NAT/BPU/TLB; tune `LB_SELF_WARM` count; ensure `/ready` only flips after warm.
6. **Final test is 2026-06-05** (heavier script). Lock the best build before then; keep 2–3 test runs in reserve for the final-script behavior (more load → watch for any 5xx / queueing).

## Key files & entry points
- API hot path: `crates/api/src/epoll_server.rs` (reactor), `classifier.rs`/`tree_model.rs` (scoring), `main.rs` (dispatch + warmup + mmap).
- LB: `crates/lb/fd_handoff_lb.c` (SCM_RIGHTS handoff, socket tuning, `LB_PIN_CPU`, self-warm).
- Shared (reuse, don't touch scoring): `crates/shared/src/{vectorize,quantize,distance}.rs`.
- Tracing: `API_IO_TRACE=1` (per-stage read/score/write ns). Bench: `temporary-results/tools/run-wsl-native-k6.sh --transport handoff` (`LB_IMPL=c API_CLASSIFIER=tree_only`).
- Submission: image `visuzano/rinha-2026:<tag>` (single-arch!), `submission` branch on `epicvinny/rinha-de-backend-2026-epicvinny`, procedure + 10/day tracker in `CLAUDE.md`.
- Local submission compose under test: `C:\Users\visuz\rinha-submission\docker-compose.yml`.

## Non-negotiables (carry forward)
0 5xx, 0 oracle mismatches, vectorizer byte-equal builder/api, single-query warm p99, no test-payload lookup. Validate the **release** image via fresh-pull `docker compose up` before every preview test (a debug build hid an io_uring release crash — caught without spending a test).

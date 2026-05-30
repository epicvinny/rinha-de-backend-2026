# Next-session prompt (paste this to start the next agent)

---

You are continuing work on **Rinha de Backend 2026** (fraud-detection vector-search;
scored on single-request **warm p99**, lower is better). The repo is at
`D:\Github\rinha-de-backend-2026` (public fork). Read `CLAUDE.md` and
`docs/handoff-roadmap.md` FIRST — they hold the procedure, constraints, dead ends,
and the test-budget tracker.

## Start from
- **Local branch `codex/epoll-frontier` @ commit `855f588`** (epoll reactor + QUICKACK
  re-arm + LB-pin + docs; io_uring code present but SHELVED — do not use it).
- **Live submission image:** `visuzano/rinha-2026:uring-dfd89b3` (single-arch). On
  rebuild, retag `epoll-<shortsha>`.
- **Live submission:** `submission` branch on `epicvinny/rinha-de-backend-2026-epicvinny`
  @ `9225b7a` (epoll + QUICKACK + LB-pin, no seccomp) = preview test #7385.

## Where we are
4th place, **p99 0.387ms**, perfect score (0 FP/FN, 0 5xx). Leaders: #1 asm 0.353,
#2 cpp 0.357, #3 rust 0.385. Topology: C `fd_handoff_lb` (SCM_RIGHTS) + 2× API epoll
reactor + EPIOCSPARAMS NAPI busy-poll + `tree_only` classifier. #7385 (QUICKACK+LB-pin)
result may already be in — check it: `gh issue view 7385 --repo zanfranceschi/rinha-de-backend-2026`.

## Hard constraints (do not fight these)
- `privileged:false`, `CapAdd:null` (no CAP_NET_ADMIN), **`seccomp=unconfined` FORBIDDEN**,
  compose `cpuset` IGNORED, docker bridge only, 1.0 CPU / 350MB (lb 0.20/30, api 0.40/160×2),
  kernel ≥6.9, Haswell 2.6GHz 4 logical CPUs.
- **DEAD ENDS — do not attempt:** io_uring (needs seccomp), SO_BUSY_POLL/AF_XDP/DPDK
  (need caps), musl/FROM-scratch for warm p99 (cold-start only), CPU/classifier micro-opts
  (scoring is ~4µs — measured non-bottleneck), compose cpuset (pin via in-process
  `sched_setaffinity`: `API_PIN_CPU`, `LB_PIN_CPU`).

## Goal (reframed honestly)
**0.10ms is almost certainly infeasible** under this sandbox (world-best 0.353ms ASM on
the same hardware/rules; the floor is docker-bridge RTT + syscall/wakeup, all
kernel-bypass options blocked). **Target: beat 0.353ms** (frontier ~0.30–0.35).

## Roadmap (priority order — see docs/handoff-roadmap.md for detail)
1. Confirm #7385's score (QUICKACK+LB-pin should beat 0.387). New baseline.
2. **Tune busy-poll ON TARGET** (biggest knob, untunable on WSL): sweep
   `API_BUSY_POLL_US` {25,50,100,200} and `API_BUSY_POLL_BUDGET` {8,16,32,64}.
3. **Out-ASM the per-request overhead**: rewrite the epoll+EPIOCSPARAMS hot path as
   C-static or ASM `FROM scratch` (Rust `crates/api/src/epoll_server.rs` is the spec):
   flat-array fd state (no HashMap), zero per-request alloc, tight syscall sequence,
   pre-rendered responses (already have). This is the credible ~0.35→~0.32 path.
4. Micro-loop opts (measure each on target — below WSL noise).

## Operating rules
- **Preview tests: 10/DAY cap.** Track in `CLAUDE.md`. ALWAYS validate the RELEASE image
  via fresh-pull `docker compose up` + curl BEFORE filing (a debug build once hid a
  release crash). Build images SINGLE-ARCH (`docker buildx ... --provenance=false
  --sbom=false`) or `/opt/*` vanish on pull.
- **Never push source to a public repo** — only the Docker image + the test-files
  `submission` branch (compose + info.json, no code) on the registered repo.
- Submission procedure (image → fresh-pull validate → submission branch via git plumbing
  → `gh issue create --repo zanfranceschi/... --body "rinha/test epicvinny"` → log it):
  full steps in `CLAUDE.md`.
- Gotchas: `gh issue create` ALWAYS needs `--repo` (defaults to upstream on forks);
  PowerShell CWD persists + `C:\Users\visuz` is a git repo → use `git -C "D:\Github\..."`;
  bare Bash tool = Git Bash (Windows paths), use `wsl -d Ubuntu -e bash -lc '...'` for
  `/mnt/...` + cargo/gcc. WSL is kernel 6.6 → no NAPI locally (busy-poll is target-only).
- Non-negotiables: 0 5xx, 0 oracle mismatches, vectorizer byte-equal builder/api,
  single-query warm p99, no test-payload lookup.

Final test deadline: **2026-06-05**.

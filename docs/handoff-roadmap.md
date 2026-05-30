# Handoff & roadmap — Rinha 2026 (as of 2026-05-30)

## ►► START HERE (exact starting point for the next session) ◄◄
- **Branch (local, source — never pushed to a public repo):** `codex/epoll-frontier`
  (HEAD), fast-forward-merged into **`main`** locally (NOT pushed). Contains the
  `scorer` crate + C reactor (`crates/scorer/reactor.c`) + QUICKACK gate + all docs +
  `docker-compose.submission.yml` (the winning config, versioned).
- **Live submission image (Docker Hub, single-arch, no attestation):**
  **`visuzano/rinha-2026:epoll-clean-ec97581`** (digest `sha256:c3351fac…`) — contains
  BOTH `/opt/api` (Rust baseline, QUICKACK off) and `/opt/api_c_reactor` (the C reactor).
- **Live submission branch (registered repo `epicvinny/rinha-de-backend-2026-epicvinny`):**
  `submission` @ **`285ee52`** → C reactor (`/opt/api_c_reactor`), LB 0.02 / API 0.49×2,
  busy-poll US=50, NO seccomp = **p99 0.3647ms, score 6000** (preview #7396 — the campaign best).
- **Local + versioned submission compose:** `C:\Users\visuz\rinha-submission\docker-compose.yml`
  (live, plumbed to the submission branch) and `docker-compose.submission.yml` (versioned copy in this repo).
- **Read first:** this file, then `CLAUDE.md` (submission procedure + 10/day test
  tracker + dead ends), `docs/perf-bottlenecks.md`, `docs/exp-unikernel-scratch.md`.

## ★ Sweep campaign 2026-05-30 — p99 0.387 → 0.3647 (rank only; score was already maxed)

**Scoring reality:** p99 ≤ 1ms saturates the p99 component at 3000 → we already hold the
**maximum 6000**. Lowering p99 earns **zero points**; it only improves **leaderboard rank**
(tiebreak among perfect-score entries). **0.20ms is infeasible** — it's below the world-best
(0.353 hand-ASM) and every floor-breaking lever is rule-locked (verified from the engine's
applied config: bridge enforced, no host net, `CapAdd:null`, no privileged, ≥2 instances + LB
mandatory; io_uring needs forbidden seccomp). The realistic floor is ~0.33–0.35.

**The reactor is NOT the lever** — three runs at the same floor: Rust epoll 0.387 (#7376),
Rust+quickack+pin 0.403 (#7385), **C reactor 0.387 (#7393)**. The cost is external (bridge RTT
+ NAPI wakeup + scheduling on a shared 2C/4T box; k6 is co-located on `localhost:9999`).

**Results (each = one preview test, one variable, on image `epoll-clean-ec97581`):**

| # | Change (vs prior) | p99 (ms) | Verdict |
|---|---|---|---|
| 7393 | C reactor, LB 0.20 / API 0.40×2, US=50 | 0.3871 | baseline (= Rust) |
| 7394 | LB 0.10 / API 0.45×2 | **0.3769** | −10µs ✅ |
| 7395 | LB 0.05 / API 0.475×2 | **0.3682** | −9µs ✅ |
| 7396 | LB 0.02 / API 0.49×2 | **0.3647** | −3µs ✅ (CPU lever flattens) → **BEST** |
| 7397 | busy-poll US=25 | 0.3971 | +32µs ❌ |
| 7398 | busy-poll US=100 | 0.3900 | +25µs ❌ |

**Findings:**
- **CPU allocation is the lever.** The APIs are quota-bound (CFS throttling under burst); the LB
  is idle after the fd handoff. Shrinking the LB to its minimum (0.02) and maximizing per-API
  quota (0.49 each) bought ~22µs. The curve flattens by 0.49 (near the quota ceiling).
- **busy-poll US=50 is a hard optimum** (25 and 100 both regress 25–32µs). Leave it at 50.
- **Keep 2 instances (the rule minimum)** — more would *dilute* per-API quota = worse. Do NOT add instances.
- **LB unpinned** (dropping `LB_PIN_CPU`); pinning it onto an API HT-sibling regressed (#7385).
- **Best config = `docker-compose.submission.yml` = submission `285ee52` = p99 0.3647ms, 6000.** Live now.

**Remaining levers (next session, low probability, ~µs each — rank only):** busy-poll BUDGET
{16,32,64} at US=50; pinning topology (APIs 0/2; LB pinned to a non-sibling); LB round-robin
smoothing (repeat upstreams). The ~12µs gap to #1 (0.353) may be the ASM's per-instruction edge
or run variance; the big config wins are already captured.

## ⚠️ UPDATE 2026-05-30 (later) — #7385 REGRESSED; reverted
- **Preview #7385 came back at p99 0.4029ms — WORSE than #7376's 0.387ms** (perfect
  detection, 0 5xx either way). The two changes bundled into #7385 both hurt:
  - **per-request `TCP_QUICKACK` re-arm** (`7c92d83`) — adds a 4th `setsockopt`
    syscall to the hot path; contradicts our own measured "syscalls dominate" thesis.
  - **`LB_PIN_CPU=2`** (`dfd89b3`) — likely pins the LB onto a hyperthread sibling of a
    busy-polling API core (starvation). On 2C/4T, scheme {0,2}=core0/{1,3}=core1 is
    empirically likely (APIs on 0/1 score 0.387, not the disaster two-spinners-on-one-core
    would cause), making CPU2 = API1's sibling.
- **Reverted in code:** per-request QUICKACK re-arm is now gated behind
  `API_QUICKACK_REARM` (**default OFF**) in `epoll_server.rs` + `uring_server.rs`; the
  one-time QUICKACK in `tune_client_fd` stays. **LB pin to be dropped via compose**
  (`LB_PIN_CPU` removed → LB floats). This returns to the #7376 baseline config.
- **No preview test filed for this revert** (user decision — re-banking the known-good
  0.387 config doesn't need a test).
- **Clean baseline image BUILT + PUSHED + fresh-pull verified:**
  `visuzano/rinha-2026:epoll-clean-ec97581` (digest `sha256:c3351fac…`, single manifest,
  no attestation). Contains BOTH `/opt/api` (Rust baseline, QUICKACK off) and
  `/opt/api_c_reactor`. Fresh-pull confirmed `/opt` binaries intact; both paths smoke-tested
  in-container (LB + 2× api, bridge, tmpfs): keep-alive + churn = 0 5xx, correct buckets,
  /ready 200. *(Superseded: this image was later submitted and tuned — see the Sweep campaign
  section above; live submission is now `285ee52` at p99 0.3647ms.)*
- **Next:** front-load the cheap env-only sweeps (busy-poll + pinning topology), then the
  C-static reactor rewrite. See `docs/perf-bottlenecks.md` and the session plan.

### Track B started (2026-05-30) — C-static reactor BUILT + validated, not yet container-built
- **`crates/scorer`**: classifier+tree_model extracted into a `staticlib`+`rlib` crate
  exporting `rinha_score_body(ptr,len)->u8` (0/5/255). The Rust `api` now depends on it
  (single source of truth); behaviour unchanged.
- **`crates/scorer/reactor.c`**: from-scratch C epoll reactor (flat fd→Conn* array + Conn
  freelist, QUICKACK once, EPIOCSPARAMS busy-poll, SCM_RIGHTS, HTTP keep-alive, /ready TCP
  thread) calling `rinha_score_body` via FFI. Dockerfile builds it to `/opt/api_c_reactor`
  (dynamic link); `/opt/api` stays the fallback. A/B = one-line compose `command:` swap.
- **Validated natively (no Docker, no preview test):** FFI==classify (3/3), check_classifier
  0 mismatches over 54100, handoff smoke (approved/denied/400/ready), keep-alive 20000-req
  stress = 20000×200 / 0 5xx, and **byte-equivalence to the Rust reactor**.
- **⚠️ `validate` CANNOT test the tree_only+fast path:** its `build_request_json` serializes
  JSON fields in an order the positional `parse_fast_fields` rejects → Err on every request,
  IDENTICALLY on the Rust api and the C reactor. Use `check_classifier` (correct field order)
  for scoring correctness and a keep-alive HTTP stress for 0-5xx; not `validate`.
- **Container gates CLOSED (2026-05-30):** the image builds in bookworm (reactor.c compiles),
  `/opt/api_c_reactor` is present + dynamically linked, and an in-container compose run
  (LB + 2× `command:[/opt/api_c_reactor]`) passed keep-alive 20000 + churn 3000 = 0 5xx.
  Image = `visuzano/rinha-2026:epoll-clean-ec97581`. The C-reactor preview test remains gated
  on the env-sweep results (per the session plan); nothing is submitted yet.


## TL;DR
- We went from broken to **4th place, p99 0.387ms, perfect score** (0 FP/FN, 0 5xx).
- Topology: **C `fd_handoff_lb` (SCM_RIGHTS round-robin) + 2× API epoll reactor + EPIOCSPARAMS NAPI busy-poll + `tree_only` classifier.**
- **io_uring is a confirmed DEAD END** (needs `seccomp=unconfined`, which the Rinha forbids).
- The #7385 build (epoll + per-request `TCP_QUICKACK` re-arm + LB core-pin) **regressed to 0.403ms and was reverted** — see the UPDATE block at the top.
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
1. **Re-bank the 0.387 baseline** with a clean image: QUICKACK re-arm gated OFF
   (`API_QUICKACK_REARM` default 0) + LB unpinned (`LB_PIN_CPU` removed). #7385 proved the
   QUICKACK-rearm + LB-pin combo regressed; this reverts it. (Done in code 2026-05-30.)
2. **Tune busy-poll + pinning ON TARGET** (1 test each, batch wisely): sweep `API_BUSY_POLL_US` ∈ {25, 50, 100, 200} and `API_BUSY_POLL_BUDGET` ∈ {8, 16, 32, 64}; also probe pinning topology (LB float vs `LB_PIN_CPU=3`; APIs 0/1 vs 0/2 — a pin that tanks p99 reveals HT siblings). These are env-only on the clean image (no rebuild) and can't be tuned on WSL. Pick the best.
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

# Next-session prompt (paste this to start the next agent)

---

You are continuing work on **Rinha de Backend 2026** (fraud-detection vector-search;
scored on single-request **warm p99**, lower is better). Repo: `D:\Github\rinha-de-backend-2026`
(public fork). Read `CLAUDE.md`, `docs/handoff-roadmap.md`, and `docs/gcp-bench-lab.md`
FIRST — procedure, constraints, dead ends, test-budget tracker, and the GCP lab.

## Start from (state after the 2026-05-31 GCP campaign)
- **Local branch `codex/epoll-frontier` @ `06df365`** (adds the GCP bench-lab + an
  env-gated `SO_INCOMING_CPU` lever, **default OFF**; io_uring code present but SHELVED).
  Not pushed. NOTE: the repo working tree has a pre-existing CRLF↔LF churn — `git status`
  shows the whole tree "modified"; stage only the specific files you change.
- **Live submission (registered repo `epicvinny-sub` = epicvinny/rinha-de-backend-2026-epicvinny):**
  `submission` branch @ **`285ee52`**, image **`visuzano/rinha-2026:epoll-clean-ec97581`**.
  This is the banked best (C `fd_handoff_lb` + 2× C-reactor APIs, LB 0.02 / API 0.49×2,
  busy 50/8/prefer1, pins 0/1, `tree_only`). **Do not destabilize it.**

## Where we are — read this carefully
- **Best p99 = 0.3647ms** (preview #7396), perfect score 6000, 0 errors. Sits between
  #2 (0.357) and #1 (0.353). BUT the same config re-tested at **0.3927ms** (#7555) →
  **the preview environment has ~28µs run-to-run variance.**
- **KEY CONCLUSION: the gap to #1 (~12µs) is SMALLER than the measurement noise (~28µs).**
  It is **not reliably winnable by parameter tuning** — which run you draw matters more
  than µs-level config changes. 0.3647 is the practical floor.
- **Env knobs are exhausted.** The banked config is env-optimal and already at the
  **1 CPU / 350 MB rule limit**. GCP sweep (2026-05-31) confirmed: budget/US/CPU-split all
  within noise; only US>50 and PREFER=0 were clearly worse (so US≤50 + PREFER=1 are right).
- **SO_INCOMING_CPU lever: tested and rejected.** Built (`incoming-06df365`), 0 5xx,
  but #7553 = 0.3854ms (within noise, no gain) → **reverted** to 285ee52. Aligning RX
  softirq with the reactor core does NOT help. Code archived (`API_INCOMING_CPU`, default OFF).

## The GCP bench-lab (built this session — reusable, but know its limits)
- `docs/gcp-bench-lab.md` = full how-to. VM `rinha-haswell` (us-central1-a, n1-standard-8,
  SPOT, **currently STOPPED**), driven from WSL via `CLOUDSDK_CONFIG=/mnt/c/Users/visuz/AppData/Roaming/gcloud`.
  Tooling in `bench/` (gen_compose.sh, sweep-runner.sh, run.sh, memdiag.sh); `bench/results/`.
- **🔴 Lab limits (why it can't pick µs winners):** ~240µs/session baseline DRIFT,
  ~30µs run noise, AND **NAPI busy-poll does NOT engage on GCP virtio** (`EPIOCSPARAMS`
  EINVAL). Use GCP only for **correctness + gross regressions**, never busy-poll/RX tuning.
- **🔴 k6 MUST use `SharedArray`** (a per-VU `open()` of the 27MB dataset OOM'd & wedged
  the VM — cost hours). Already fixed in `bench/k6-bench.js`.

## Hard constraints (do not fight)
`privileged:false`, `CapAdd:null` (no CAP_NET_ADMIN), `seccomp=unconfined` FORBIDDEN,
compose `cpuset` IGNORED (pin via in-process `API_PIN_CPU`/`LB_PIN_CPU`), docker bridge only,
**1 CPU / 350 MB total** (lb 0.02/30, api 0.49/160×2), kernel ≥6.9, Haswell ~2.6GHz 4 logical CPUs.
**DEAD ENDS:** io_uring (seccomp), SO_BUSY_POLL/AF_XDP/DPDK (caps), musl/scratch (cold-start only),
classifier/parser/SIMD micro-opt (~4µs, non-bottleneck), per-req QUICKACK rearm (regressed #7385),
SO_INCOMING_CPU (rejected #7553), more total CPU (already at limit).

## What's actually left (all low-EV, be honest with the user before spending previews)
The remaining gains are below the preview noise floor, so each is a **coin-flip preview gamble**:
1. **C/ASM hot-path rewrite** of the epoll+EPIOCSPARAMS reactor — the only *credible* code
   path (flat fd state, zero per-req alloc, tighter syscalls). `crates/api/src/epoll_server.rs`
   is the spec; `crates/scorer/reactor.c` is the current C reactor. But #7393 showed C==Rust,
   so the upside is doubtful.
2. Other socket-opt levers (TCP_NOTSENT_LOWAT, SO_RCVLOWAT) — but SO_INCOMING_CPU already
   showed RX/wakeup socket-opts don't move it.
**Recommendation:** 0.3647 is at/near the floor; the honest move is likely to STOP tuning and
bank it, unless the user explicitly wants to spend previews on a sub-noise gamble. Don't
auto-spend previews — surface the gamble and let the user decide.

## Operating rules (unchanged)
- **Previews: 10/DAY cap** (tracker in `CLAUDE.md`; used 2/10 on 2026-05-31). ALWAYS
  fresh-pull validate the RELEASE image (`docker compose up` + curl) BEFORE filing.
  Build SINGLE-ARCH (`docker buildx ... --provenance=false --sbom=false`) or `/opt/*` vanish.
- Submission procedure (image → fresh-pull validate → submission branch via git plumbing on
  `epicvinny-sub` → `gh issue create --repo zanfranceschi/... --body "rinha/test epicvinny"`
  → log it → **revert to 285ee52 on any regression**): full steps in `CLAUDE.md`.
- Gotchas: `gh issue create` ALWAYS needs `--repo`; use `git -C "D:\Github\..."`; bare Bash
  = Git Bash, use `wsl -d Ubuntu -e bash -lc '...'` for `/mnt/...`; WSL git can't write
  `.git/config` (use `GIT_AUTHOR_*` env for commits). Author = `epicvinny <vinicius.suzano@rdstation.com>`.
- Non-negotiables: 0 5xx, 0 oracle mismatches, vectorizer byte-equal, single-query warm p99.

Final test deadline: **2026-06-05**.

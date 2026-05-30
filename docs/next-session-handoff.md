# Next-session handoff — Rinha 2026 (continue p99 optimization + GCP bench lab)

> Read this first, then `docs/handoff-roadmap.md` (full campaign + START-HERE),
> `CLAUDE.md` (submission procedure + 10/day tracker + dead ends), and the
> deep-research brief at `C:\Users\visuz\rinha-deep-research-brief.md`.

## 1. Where we are (TL;DR)

- **Best config = live submission `285ee52` = p99 0.3647ms, score 6000 (perfect).** Image
  `visuzano/rinha-2026:epoll-clean-ec97581`. Versioned as `docker-compose.submission.yml`.
- **Branch:** `codex/epoll-frontier` (HEAD) = `main` (fast-forwarded locally, **NOT pushed**).
- **Score is already MAXED** (p99 ≤ 1ms saturates the metric at 3000 → total 6000). Lower p99
  only improves **leaderboard rank** (tiebreak). Leaders: #1 ASM 0.353, #2 C++ 0.357, #3 Rust 0.385.
- **0.20ms is infeasible** under the sandbox (bridge-only net, no host net, no caps, no privileged,
  default seccomp → io_uring dead, ≥2 instances + LB mandatory; verified from the engine's applied
  config). Realistic floor ~0.33–0.35.

## 2. What we already proved (do NOT repeat)

| Lever | Result | Verdict |
|---|---|---|
| Reactor language (Rust vs from-scratch C calling Rust scorer via FFI) | both 0.387 | **not the lever** |
| **CPU split** LB 0.20/API 0.40 → … → **LB 0.02 / API 0.49×2** | 0.387 → **0.3647** | **the big lever** (APIs are CFS-quota-bound; LB idle after handoff) |
| busy-poll `usecs` (25 / 50 / 100) | 0.397 / **0.365** / 0.390 | **US=50 is a hard optimum** |
| Instance count | 2 is the minimum AND best | more dilutes per-API quota → worse |
| LB pin (`LB_PIN_CPU`) | regressed | leave LB unpinned |
| per-request `TCP_QUICKACK` re-arm | +syscall, regressed | gated off (`API_QUICKACK_REARM=0` default) |

Measured model: ~365µs ≈ NAPI/epoll wakeup + CFS scheduling jitter + docker-bridge RTT (both
ways); CPU/scoring ~4µs (irrelevant); 3 syscalls/req (floor). k6 is **co-located** on the host.

## 3. Optimization possibilities NOT yet tested (the actual next work)

All are **env/compose-only** on the public image unless noted — cheap to A/B. Priority order:

1. **busy-poll `budget` sweep** `{16, 32, 64}` at `usecs=50` (we use 8; max 64 without caps). Env-only.
2. **Pinning topology** via in-process `API_PIN_CPU`: pairs `{0,1}` vs `{0,2}` vs `{2,3}`; LB float vs
   pinned to a non-API-HT-sibling. A pin that tanks p99 reveals the HT layout. Also probe whether the
   host honors compose `cpuset:` (top-1 ships it). Env/compose-only.
3. **Per-container `sysctls:`** allowed in bridge without privilege — test which `net.ipv4.tcp_*` /
   `net.core.*` are settable and help (e.g. low-latency TCP knobs). Compose-only; verify each applies.
4. **Reactor socket-option experiments** (code, then rebuild): `SO_INCOMING_CPU` (steer RX wakeups to
   the reactor's pinned CPU), `TCP_NOTSENT_LOWAT`, `SO_RCVLOWAT`, send-buffer sizing. These target the
   wakeup/scheduling tail directly.
5. **LB round-robin smoothing** (repeat the `BACKENDS` upstream list ×4 — competitor-proven, marginal).
6. **CFS interaction**: confirm the CPU-quota lever isn't leaving anything (e.g. LB 0.01 / API 0.495);
   investigate whether CFS period/throttle is the tail source on the GCP Haswell box (where we CAN trace).
7. **Whatever the ChatGPT deep-research returns** (brief at `C:\Users\visuz\rinha-deep-research-brief.md`):
   triage its suggestions into "env-only / legal" (test cheaply) vs "rule-locked" (discard).
8. **The ASM-vs-C micro-arch question** (#1 ASM 0.353 vs our 0.387): now testable on the GCP Haswell VM
   natively (see §4) — figure out whether that 34µs is real per-instruction edge or harness noise.

**The GCP Haswell bench lab (§4) exists precisely to A/B items 1–8 cheaply (RELATIVE p99) without
spending official preview tests.** Always confirm a winner on the official preview before locking it.

## 4. GCP Haswell bench lab — automated local→VM test loop

**Why:** WSL/Docker-Desktop can't measure p99 (VM, kernel 6.6, no busy-poll). A GCP **N1 VM pinned to
Intel Haswell** runs our `linux/amd64` + AVX2 image **natively**, has **kernel ≥6.9 → real
`EPIOCSPARAMS` busy-poll**, and 4 vCPU = 2C/4T = the Rinha topology. It yields **RELATIVE** p99 signal
(absolute won't match the Mac Mini — Haswell-EP server vs i5 mobile, VM vCPU jitter). Confirm winners on
the official preview.

**ONE-TIME MANUAL PREREQ (the only thing the user must do):**
```
gcloud auth login
gcloud config set project <PROJECT_ID>
gcloud services enable compute.googleapis.com   # + billing enabled
```
After that, the agent drives everything via `gcloud` from WSL — no further manual steps.

**Scripts (in `bench/`, run from WSL bash):**
- `bench/gcp.sh find-zone` — list which zones still offer Intel Haswell (it's only in OLD zones).
- `bench/gcp.sh provision` — create the SPOT VM (`n1-standard-4`, `--min-cpu-platform "Intel Haswell"`,
  Ubuntu 24.04 rolling kernel), install docker+k6+jq, push `test/test-data.json` + the remote scripts.
- `bench/gcp.sh bench <compose.yml>` — scp the compose to the VM, run the stack + k6 (co-located),
  print `{"p99_ms":…,"http_req_failed":…}` to stdout. **This is the per-experiment command.**
- `bench/gcp.sh build` — rsync the repo (minus `target/`,`.git`) to the VM and `docker build` there
  (for CODE experiments; needs `resources/references.json.gz` pushed once — provision does it if present).
- `bench/gcp.sh start` / `stop` / `delete` — manage the VM (STOP it when idle; SPOT is cheap but not free).

**Agent workflow each iteration (fully automatic):**
1. `bench/gcp.sh start` (if stopped).
2. Edit `docker-compose.submission.yml` (config experiment) OR edit code + `bench/gcp.sh build` (code experiment).
3. `bench/gcp.sh bench docker-compose.submission.yml` → read p99 JSON from stdout → keep/discard.
4. When a config beats the current best on the VM, run the **official preview** (CLAUDE.md procedure:
   plumb submission branch → `gh issue create --repo zanfranceschi/... --body "rinha/test epicvinny"`
   → monitor the bot comment) to confirm on the real Mac Mini.
5. `bench/gcp.sh stop` when done for the session.

**Caveats:** VM ≠ bare-metal (vCPU scheduling jitter); busy-poll on veth/bridge inside a VM may differ
from the Mac Mini's real bridge; treat results as directional. The scripts are first-draft — validate on
first run (k6 install method, jq, healthcheck path, summary-export keys) and fix in place.

## 5. Submission mechanics (carry-over, from CLAUDE.md)

- Two repos: dev `epicvinny/rinha-de-backend-2026` (`origin`) + registered `…-epicvinny` (remote
  `epicvinny-sub`, branch `submission` = compose+info.json only). Upstream push is DISABLED (safety).
- Plumbing (no working-tree touch): `GIT_INDEX_FILE` temp + `read-tree <parent>` +
  `hash-object --no-filters -w <compose>` + `update-index --cacheinfo` + `write-tree` + `commit-tree -p` +
  `git push epicvinny-sub <commit>:submission --force`. Use **plain git** (not rtk) when capturing SHAs.
- Preview: `gh issue create --repo zanfranceschi/rinha-de-backend-2026 --title "Preview test - epicvinny"
  --body "rinha/test epicvinny"`. **10/day cap** — log every run in the CLAUDE.md tracker. Wait for the
  bot's result comment before filing the next. Final scored run: **2026-06-05**.
- Image builds (only if code changes): `docker buildx build --platform linux/amd64 --provenance=false
  --sbom=false -t visuzano/rinha-2026:<newtag> --push …`; fresh-pull verify before any preview.

## 6. Key files & pointers
- Best config: `docker-compose.submission.yml`. Live submission compose: `C:\Users\visuz\rinha-submission\docker-compose.yml`.
- Reactor (C): `crates/scorer/reactor.c`; scorer FFI: `crates/scorer/src/lib.rs`; Rust reactor: `crates/api/src/epoll_server.rs`.
- LB: `crates/lb/fd_handoff_lb.c`. Build: `Dockerfile` (builds `/opt/api`, `/opt/api_c_reactor`, `/opt/fd_handoff_lb`).
- Campaign results + findings: `docs/handoff-roadmap.md` ("★ Sweep campaign"). Deep-research brief: `C:\Users\visuz\rinha-deep-research-brief.md`.
- Bench lab: `bench/` (this handoff §4).
- Env: `git -C "D:\Github\rinha-de-backend-2026"`; `wsl -d Ubuntu -e bash -lc` for cargo/gcc/rsync/gcloud; `gh … --repo`.

## 7. Non-negotiables
0 5xx, 0 oracle mismatches, vectorizer byte-equal, single-query warm p99, no test-payload lookup.
Never let a rank chase risk the perfect 6000. Local (WSL) = correctness only; GCP VM = relative p99;
official preview = the only absolute p99 oracle.

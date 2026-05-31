# GCP Haswell bench-lab — how to run

Cheap **relative** p99 A/B lab for the Rinha hot path, on an Intel-Haswell GCP VM
that runs our amd64+AVX2 image natively with a kernel new enough for real
`EPIOCSPARAMS` NAPI busy-poll. Co-located k6 hits `localhost:9999` on the VM, so
the only network exposure needed is SSH.

> **Absolute p99 here will NOT match the official Mac-Mini target.** Use this only
> for relative deltas between configs, then confirm a winner on an official preview
> test. The 2-core/4-thread layout matches the Mac's core count, so busy-poll and
> CPU-split signal transfers; **pinning numbering does not** (weight it low).

---

## 1. Machine / config (this campaign)

| Item | Value |
|------|-------|
| Project | internal prod project — set via `GCP_PROJECT` (not committed; public repo) |
| VM name | `rinha-haswell` |
| Zone | `us-central1-a` (Haswell available in all `us-central1-*`) |
| Machine type | **`n1-standard-8`** → 8 vCPU (resized up from n1-standard-4; see note) |
| min-cpu-platform | `Intel Haswell` (reports `Xeon @ 2.30GHz`, **AVX2 present**) |
| Provisioning | **SPOT** (`--provisioning-model=SPOT --no-restart-on-failure`) — cheap, **can be preempted** (recurred here; if it hurts, recreate as `--provisioning-model=STANDARD`) |
| Image | `ubuntu-2404-lts-amd64`, boot disk 30 GB `pd-balanced` |
| Kernel | `6.17.0-1016-gcp` (need **≥ 6.9** for EPIOCSPARAMS busy-poll) ✓ |
| Network | `default`; ingress `default-allow-ssh` (tcp:22 from 0.0.0.0/0) already exists |
| External IP | ephemeral (you never need it — `gcloud compute ssh` resolves by VM name) |
| Installed | docker 29.5.2, docker compose v5.1.4, **k6 v2.0.0** (from GitHub binary), jq, sysstat |

> **Why 8 vCPU (not 4):** co-located k6 needs its own cores so it never steals from
> the stack. The stack is pinned to cpu0/cpu1 (+ CFS-limited to ~1 CPU), k6 is pinned
> to cpu2,3,6,7 (`K6_CPUS`), leaving the stack's 2 cores pristine. The stack still
> *behaves* like the ~1-CPU-quota, 2-core target; only k6 got headroom.

> **Measured GCP baseline (banked config, 900 rps): p99 ≈ 0.65 ms, 0 fail.** (Mac
> official is 0.36 ms — GCP virtio/bridge adds ≈+0.3 ms. Use **relative** deltas only.)

**CPU topology** (`/sys/.../topology/thread_siblings_list`):

```
core0 = {cpu0, cpu2}     core1 = {cpu1, cpu3}
```

So the banked pins `API1→cpu0 / API2→cpu1` put the two APIs on **separate
physical cores** (good); HT siblings cpu2/cpu3 stay free for the LB + RX softirqs.

**Org-policy facts that made this trivial** (checked before provisioning): external
IPs allowed, OS-Login & Shielded-VM not enforced, SPOT CPU quota 0/10000.

---

## 2. One-time host setup (already done on this machine)

The bench harness (`bench/gcp.sh`) runs from **WSL Ubuntu**, but WSL's gcloud was
not authenticated. Two host quirks were fixed:

**a) Bridge WSL gcloud to the Windows credentials** (no separate `gcloud auth login`):

```bash
export CLOUDSDK_CONFIG=/mnt/c/Users/visuz/AppData/Roaming/gcloud
# now WSL gcloud uses the Windows account/project. Prefix every bench call with this.
```

**b) Non-interactive SSH** (so an autonomous loop never blocks on a prompt):

```bash
mkdir -p ~/.ssh && chmod 700 ~/.ssh
ssh-keygen -t rsa -b 2048 -N "" -C rinha-bench-lab -f ~/.ssh/google_compute_engine   # empty passphrase
printf 'Host *\n    StrictHostKeyChecking accept-new\n    UserKnownHostsFile ~/.ssh/known_hosts\n' > ~/.ssh/config
chmod 600 ~/.ssh/config ~/.ssh/google_compute_engine
```

**c) ⚠️ CRLF gotcha.** `bench/*.sh` are checked out **CRLF** on the Windows FS, so
running them under WSL bash fails (`set: pipefail: invalid option name`). The same
breaks `remote-bench.sh` **on the VM**. Fix = run LF copies and push the LF
`remote-bench.sh`:

```bash
mkdir -p ~/rinha-bench
for f in gcp.sh remote-bench.sh k6-bench.js; do
  tr -d '\r' < /mnt/d/Github/rinha-de-backend-2026/bench/$f > ~/rinha-bench/$f
done
chmod +x ~/rinha-bench/*.sh
```

**d) k6 install.** The apt repo (`dl.k6.io`) failed its GPG signature on this VM.
k6 was installed straight from the GitHub release binary instead (see
`~/rinha-bench/setup-vm.sh`).

**e) 🔴 THE big one — k6 `SharedArray` is mandatory.** The repo's `bench/k6-bench.js`
loads the dataset with module-scope `const RAW = open('test-data.json'); JSON.parse(RAW)`.
That runs **once per VU**, and `test-data.json` is **27 MB** → every VU holds its own
parsed copy (hundreds of MB each). At 100–250 VUs that's **10–37 GB** → the VM OOMs and
**SSH wedges** ("Connection timed out during banner exchange"; the box never recovers
without a reset). This looks exactly like a CPU/busy-poll/NAPI pathology but is **not** —
`mpstat` showed `%soft≈0.5%`, `%idle≈49%`; `vmstat` showed `free` falling ~200 MB/s.
The fix (already applied to `~/rinha-bench/k6-bench.js`, `k6-light.js`):

```js
import { SharedArray } from 'k6/data';
const ENTRIES = new SharedArray('entries', function () {
  const parsed = JSON.parse(open(__ENV.K6_DATA || 'test-data.json'));
  return Array.isArray(parsed) ? parsed : (parsed.entries || []);
});
// also: options.discardResponseBodies = true
```

With this, k6 RSS stays flat (~0.3 GB) and `MemAvailable` barely moves. **Any new k6
script for this lab MUST use SharedArray.** (Diagnosed via a memory-sampling `memdiag.sh`
that logged `MemAvailable` + top-RSS per second — keep it for future "wedge" mysteries.)

**f) Don't SSH a box under heavy load mid-bench**, and **read results, never hold the
SSH session.** Launch sweeps **detached** (`nohup … &`, via `run.sh`) so an SSH drop
can't kill or starve the run; the incremental TSV on disk survives drops, preemption,
and resets. k6 is pinned off the stack cores (`K6_CPUS=2,3,6,7`) so it can't steal CPU.

---

## 3. Provision / lifecycle

```bash
export CLOUDSDK_CONFIG=/mnt/c/Users/visuz/AppData/Roaming/gcloud
cd /mnt/d/Github/rinha-de-backend-2026

bash ~/rinha-bench/gcp.sh find-zone     # list zones still offering Intel Haswell
bash ~/rinha-bench/gcp.sh provision     # create SPOT VM + install docker/jq + push assets
#   then once: gcloud compute scp ~/rinha-bench/{remote-bench.sh,gen_compose.sh,sweep-runner.sh} \
#              rinha-haswell:~/rinha/ --zone=us-central1-a   # LF helpers
#   and:       bash ~/rinha/setup-vm.sh   (installs k6, prints topology)

bash ~/rinha-bench/gcp.sh ssh           # interactive shell
bash ~/rinha-bench/gcp.sh stop          # STOP when idle — SPOT is cheap, not free
bash ~/rinha-bench/gcp.sh start
bash ~/rinha-bench/gcp.sh delete        # tear down completely
```

VM-side assets live in `~/rinha/`: `test-data.json`, `references.json.gz`,
`remote-bench.sh`, `k6-bench.js`, `gen_compose.sh`, `sweep-runner.sh`,
`experiments.tsv`.

---

## 4. Run ONE experiment

`bench/gcp.sh bench <compose.yml>` scp's the compose, runs co-located k6 (mirrors the
official `ramping-arrival-rate 1→900/120s`), prints JSON, tears down:

```bash
bash ~/rinha-bench/gcp.sh bench /mnt/d/Github/rinha-de-backend-2026/docker-compose.submission.yml
# -> {"p99_ms": 0.xx, "http_req_failed": 0, "checks_failed": 0}
```

Pass/fail bar: `http_req_failed` and `checks_failed` must be **0** (correctness +
no 5xx). Only `p99_ms` deltas ≥ ~5µs that repeat across runs are real (±3–5µs noise).

---

## 5. Run a SWEEP (the efficient path)

`gen_compose.sh` builds a compose from knobs; `sweep-runner.sh` iterates an
experiment matrix on the VM, repeats each N times, logs **median p99** to a TSV.
Running it all in one SSH job avoids per-config scp/ssh overhead and survives
across turns.

**`gen_compose.sh` knobs** (env vars; defaults = banked `285ee52` config but with
`cpuset` removed — faithful to the official engine, which strips cpuset; affinity
via `API_PIN_CPU` only):

| Env | Default | Meaning |
|-----|---------|---------|
| `IMG` | `visuzano/rinha-2026:epoll-clean-ec97581` | image |
| `CMD` | `/opt/api_c_reactor` | API binary (`/opt/api` = Rust fallback) |
| `LB_CPU` / `API_CPU` | `0.02` / `0.49` | CFS cpu limits (APIs are quota-bound) |
| `BUSY_US` | `50` | `API_BUSY_POLL_US` (proven optimum 50) |
| `BUSY_BUDGET` | `8` | `API_BUSY_POLL_BUDGET` (cap 64 w/o caps) |
| `PREFER` | `1` | `API_PREFER_BUSY_POLL` |
| `PIN1` / `PIN2` | `0` / `1` | `API_PIN_CPU` per instance |
| `LB_PIN` | unset | `LB_PIN_CPU` |
| `USE_CPUSET` | `0` | `1` = add `cpuset:` (NOT faithful to official) |
| `EXTRA_API` | empty | extra API env lines (e.g. a new socket-opt flag) |

**Experiment matrix** `~/rinha/experiments.tsv` — `LABEL<TAB>KEY=val KEY=val…`
(empty 2nd field = default baseline). Example:

```
baseline	
budget32	BUSY_BUDGET=32
us60	BUSY_US=60
cpu_01_495	LB_CPU=0.01 API_CPU=0.495
```

**Launch** (REPEATS repeats per config; results append to `~/rinha/sweep-results.tsv`):

```bash
gcloud compute ssh rinha-haswell --zone=us-central1-a \
  --command="REPEATS=3 bash ~/rinha/sweep-runner.sh ~/rinha/experiments.tsv"
# results: ts | label | median_p99_ms | runs | max_fail | max_chkfail | all_p99 | overrides
```

Each run ≈ compose up + 120s k6 + teardown ≈ 3–4 min, so `12 configs × 3 ≈ 2 h`.
The TSV is written incrementally → partial results survive a preemption; resume by
trimming `experiments.tsv` to what's left.

---

## 6. Code experiments (rebuild on the VM)

For socket-opt / compiler-flag changes that need a rebuilt image:

```bash
bash ~/rinha-bench/gcp.sh build rinha-local:bench   # rsync repo + docker build on VM
# then point a compose `IMG=rinha-local:bench` and bench it
```

---

## 7. Promotion rule

GCP win (≥5µs, repeatable, 0 fail) → fresh-pull the release image locally →
`docker compose up` + curl smoke → **one** official preview (10/day cap; log
issue#/config/result in `CLAUDE.md`) → keep or revert. Never destabilize the banked
`0.3647ms`. **Stop the VM when idle.**

---

## 8. Phase A findings (2026-05-31) — env-knob frontier is exhausted

Full matrix: baseline (×3 interleaved) + busy-poll budget{16,32,64} + US{40,60,75} +
prefer0 + CPU-split{0.01/0.495, 0.005/0.4975}. Raw TSV: `bench/results/phaseA-sweep-results.tsv`.
All configs **0 fail / 0 chkfail**.

**The dominant effect is session drift, not config.** The three interleaved baselines
fell monotonically the whole run:

```
baseline      0.6805 ms  (23:43)
baseline_mid  0.5841 ms  (00:12)
baseline_end  0.4403 ms  (01:34)     →  ~240 µs downward drift over ~2 h
```

That drift (and ~30 µs run-to-run noise) **dwarfs every config delta**. Drift-corrected
against the interpolated baseline:

| lever | result | drift-corrected verdict |
|-------|--------|-------------------------|
| busy-poll budget 16 / 32 / 64 | 0.675 / 0.603 / 0.607 | within noise → **no effect** (the early "budget32 win" was pure drift) |
| busy-poll US 40 / 60 | 0.587 / 0.597 | within noise → no effect |
| **busy-poll US 75** | **1.472** | **catastrophic (+~0.9 ms)** → keep **US ≤ 50** |
| **PREFER_BUSY_POLL=0** | 0.697 | **+~115 µs worse** → keep **PREFER=1** |
| CPU-split 0.01/0.495, 0.005/0.4975 | 0.562 / 0.455 | within noise/drift → **no effect** (rules cap total at 1 CPU anyway) |

**Verdict:** the banked config (budget=8, US=50, PREFER=1, LB 0.02 / API 0.49×2, **already
at the 1 CPU / 350 MB rule limit**) is **env-optimal** — no knob produces a GCP-resolvable
improvement. Two settings are positively confirmed (US≤50, PREFER=1); they're the only
effects large enough to clear the drift.

**Methodological limit (important):** GCP's per-session baseline drifts ~240 µs and run
noise is ~30 µs, so this lab **cannot resolve the ≤30 µs tie-break gains** the leaderboard
needs, and (virtio NIC ≠ the Mac's real NIC) it's a **poor proxy for RX-steering / wakeup-path
levers**. Use it to catch gross regressions and confirm directional choices — **not** to pick
µs winners. The remaining path to beat 0.3647 is code-level wakeup-path changes
(`SO_INCOMING_CPU`, etc.) measured **directly on official previews**, since GCP can't pre-screen them.

> **⚠️ NAPI busy-poll does NOT engage on this GCP VM.** The reactor logs
> `EPIOCSPARAMS unavailable (Invalid argument)` — the ioctl returns `EINVAL` because
> `prefer_busy_poll=1` needs `CAP_NET_ADMIN`, which the container doesn't have (and
> unprivileged `SO_BUSY_POLL` is capped at the `net.core.busy_poll` sysctl = 0). So on
> GCP the stack runs with **busy-poll effectively off**. That means the Phase A
> busy-poll sweep (budget / US / prefer) was measuring a no-busy-poll stack — treat the
> "US≤50 / PREFER=1" GCP signals as **unreliable** (they may be variance, not the knob).
> The banked config's busy-poll tuning was validated on the **official Mac previews**, not
> here. Net: GCP can validate **correctness** and catch gross regressions, but cannot
> measure busy-poll-dependent latency at all.

> **Always STOP the VM when not actively benching** (`bash ~/rinha-bench/gcp.sh stop`) — it's
> SPOT (preempted twice mid-campaign; incremental TSV made resume painless) and bills while up.

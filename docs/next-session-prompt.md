# Next-session prompt (paste this para iniciar o próximo agente)

---

Você está continuando o trabalho na **Rinha de Backend 2026** (fraud-detection; pontuado em **warm p99 single-request**, menor é melhor). Repo: `D:\Github\rinha-de-backend-2026` (fork público). Leia `CLAUDE.md` PRIMEIRO — tracker de previews, procedimentos, dead ends.

## Estado atual (2026-05-31, fim de sessão)

### Leaderboard (vivo em rinhadebackend.com.br)
| # | Participante | Stack | p99 |
|---|---|---|---|
| 1 | rafaelcoelhox (rafaelcoelhox-detecta-fraude) | Rust+C LB | **0.362192ms** |
| 2 | dalvorsn (dalvorsn-cpp) | C++ | 0.365789ms |
| 3 | vinicius-piassa (rinha-backend-2026-asm) | ASM | 0.377381ms |
| 4 | fksegundo (fksegundo-rust) | Rust | 0.391305ms |
| **5** | **epicvinny (nos)** | C reactor+Rust | **0.404048ms** |

O 0.404ms vem do #7566 (single-recv regrediu). O **#7577** (LB 0.20/API 0.40x2) esta PENDENTE na fila (~1h) e e o teste vivo. Resultado do sweep GCP de CPU split tambem pendente (agente background).

### Submission branch atual
- **Repo registrado:** `epicvinny-sub` = `epicvinny/rinha-de-backend-2026-epicvinny`
- **Commit:** `4819600` — `epoll-clean-ec97581`, LB **0.20** cpu/30MB, API **0.40x2**/160MB, `API_BUSY_POLL_US=50`, `API_PIN_CPU=0/1`, sem `API_SINGLE_RECV`
- **Imagem:** `visuzano/rinha-2026:epoll-clean-ec97581` (C reactor, tree_only, busy-poll 50/8/prefer1)
- **Revert de segurança:** se o #7577 regredir, reverter para `285ee52` (LB 0.02/API 0.49, mesma imagem)

### Branch dev
- `codex/epoll-frontier` @ `43f9383` (pushed) — tem single-recv archived (default OFF, nao usar)

---

## DESCOBERTAS CRITICAS desta sessão (2026-05-31)

### 1. O novo #1 subiu com mudanca APENAS no compose (zero codigo)
`rafaelcoelhox` (0.397ms->0.362ms) mudou so `docker-compose.yml` (commit `16e54a5c`, +11/-5):
```diff
- lb cpus: "0.10"   ->  lb cpus: "0.20"    # LB ganhou mais CPU
- api cpus: "0.45"  ->  api cpus: "0.40"   # APIs cederam CPU ao LB
+ EPOLL_BUSY_POLL_US: "100"                 # busy-poll kernel 50->100us
+ EPOLL_IDLE_US: "60"                       # idle quantum 250->60us
```
Imagens dos containers nao foram reconstruidas. Ganho de ~35us so com rebalance de CPU.

### 2. Nossas conclusoes da campanha de CPU eram provavelmente artefatos de variancia
Todos os "resultados" (0.387->0.377->0.368->0.365) eram cada um **um unico draw** dentro de distribuicao com ~28us de variancia. A progressao pareceu convincente mas era sequencial sem repeticoes. O #1 ganhou usando o sentido oposto (mais LB, menos API). Conclusao: **as conclusoes de env-tuning da campanha de maio nao sao confiaveis como verdades absolutas**.

### 3. single-recv REGREDIU +40us — motivo entendido
Removemos o drain-until-EAGAIN do `drive()` e regredimos (#7566=0.404ms). Causa: k6 roda **co-localizado** com o server na Rinha. Apos enviar a resposta, o cliente ja enviou o proximo request — o drain `read()` le o **proximo request antecipadamente** (lookahead implicito). Removendo o drain adicionamos uma round-trip extra de epoll_wait. O #1 faz single-recv mas tem busy-poll agressivo (30us spin + 60us `epoll_pwait2`) que torna a round-trip desprezivel. Codigo single-recv fica arquivado em `reactor.c`/`epoll_server.rs` com flag `API_SINGLE_RECV` (default OFF).

### 4. Analise completa do rafaelcoelhox (repo: `rafaelcoelhox/detecta-fraude`)
- **C fd-lb** (`native/fd-lb.c`): `TCP_DEFER_ACCEPT` no listen; `TCP_NODELAY+TCP_QUICKACK` em cada fd **antes do SCM_RIGHTS** (API nao precisa fazer setsockopt por conexao)
- **3-tier epoll wait** (`src/server.rs`): (1) `epoll_wait(0)` non-blocking -> (2) 30us `spin_loop` -> (3) `epoll_pwait2(timespec{tv_nsec:60000})` — ns-granularity, muito mais preciso que nosso `epoll_wait(1ms)`
- **`network_mode: none`** no api2 — zero overhead veth/conntrack/bridge
- **Scoring**: vector 5-NN sobre 3M vetores i16 AVX2, KD-tree com early-exit. CPU ~0.92us (comparavel com nossa tree de 4us — ambos abaixo do gargalo de wakeup)
- `logging: driver: none` em todos os servicos
- JSON parser order-independent (nosso e posicional/dependente de ordem)
- Imagem separada para LB (`Dockerfile.lb`), static binary em scratch

---

## O que fazer apos acordar (ordem de prioridade)

### 1. Verificar resultado do #7577 (LB 0.20/API 0.40x2)
```bash
gh api repos/zanfranceschi/rinha-de-backend-2026/issues/7577/comments --jq '.[0].body' | python3 -c "import sys,json; d=json.load(sys.stdin); print(d['test-results']['scoring']['raw']['p99_ms'])"
# ou simplesmente:
gh issue view --repo zanfranceschi/rinha-de-backend-2026 7577 --comments
```
- **< 0.362ms** -> somos #1. **PARAR** (nunca re-rolar um resultado vencedor).
- **< 0.404ms (melhorou)** -> LB 0.20 funciona. Considerar Test 2.
- **>= 0.404ms (regrediu)** -> reverter submission para `285ee52`:
  ```bash
  # git plumbing para reverter (ver procedimento completo abaixo)
  ```

### 2. Verificar resultado do sweep GCP
```bash
wsl -d Ubuntu -e bash -lc 'export CLOUDSDK_CONFIG=/mnt/c/Users/visuz/AppData/Roaming/gcloud && gcloud compute ssh rinha-haswell --zone=us-central1-a --command="cat ~/rinha/sweep-results.tsv" 2>&1'
```
Isso revela qual CPU split otimiza relativamente no GCP. PARAR a VM depois:
```bash
wsl -d Ubuntu -e bash -lc 'export CLOUDSDK_CONFIG=/mnt/c/Users/visuz/AppData/Roaming/gcloud && gcloud compute instances stop rinha-haswell --zone=us-central1-a 2>&1'
```

### 3. Se #7577 melhorou — Test 2 (sem rebuild)
Testar em conjunto (compose-only):
- `API_BUSY_POLL_US=100` (o #1 usa; nossa campanha rejeitou com um unico draw ruidoso)
- `network_mode: none` no api2 — mas validar localmente primeiro: o healthcheck curl precisa do loopback (existe em none mode); trocar `condition: service_healthy` por `condition: service_started` no api2 do LB depends_on para evitar deadlock
- `logging: driver: none` em todos os servicos (pequena reducao de overhead Docker)

### 4. Se ainda nao #1 — codigo (rebuild necessario)
Gap de codigo real vs o #1:
1. **3-tier epoll idle em `reactor.c`**: apos EPIOCSPARAMS expirar, fazer 30us `spin_loop` de `epoll_wait(0)` -> depois `epoll_pwait2({.tv_nsec=60000})` em vez de `epoll_wait(1ms)`. Isso reduz latencia de acordar de 1ms para 60us.
2. **Com idle agressivo, re-testar single-recv** — round-trip de epoll vira 60us (vs 1ms atual); o trade-off muda.
3. **TCP_NODELAY/QUICKACK no LB pre-handoff** (`fd_handoff_lb.c`), remover do API — ~2 setsockopt por conexao removidos do core de API.

---

## Hard constraints (nao mudar)
- `privileged:false`, `CapAdd:null`, sem `seccomp=unconfined`
- Compose `cpuset` IGNORADO no engine oficial — usar `API_PIN_CPU` in-process
- **1 CPU / 350 MB total** (todas as instancias somadas)
- Scoring: 0 FP, 0 FN — validar com `cargo run --release --bin check_classifier -- --queries test/test-data.json`
- Previews: 10/dia (tracker em `CLAUDE.md`; usado 5/10 em 2026-05-31); **NUNCA 2 issues em paralelo**

## Dead ends confirmados
- io_uring (seccomp bloqueado)
- SO_INCOMING_CPU (rejeitado #7553, piassa tambem nao usa)
- SO_BUSY_POLL socket-level (sem CAP_NET_ADMIN)
- single-recv SEM busy-poll agressivo (regrediu #7566)
- Otimizar scorer/parser (4us, nao e o gargalo)
- 3+ instancias de API (dilui quota CFS)

## Procedimento de submissão (quick-ref)
```bash
# === BUILD (sem attestation) ===
docker buildx build --platform linux/amd64 --provenance=false --sbom=false \
  -t visuzano/rinha-2026:<tag> -t visuzano/rinha-2026:latest --push "D:\Github\rinha-de-backend-2026"

# === VALIDAR LOCAL ===
docker rmi visuzano/rinha-2026:<tag>
docker compose -f C:/Users/visuz/rinha-submission/docker-compose.yml pull
docker compose -f C:/Users/visuz/rinha-submission/docker-compose.yml up -d
curl -X POST localhost:9999/fraud-score -H "Content-Type: application/json" -d '{...}'
docker compose -f C:/Users/visuz/rinha-submission/docker-compose.yml down

# === ATUALIZAR SUBMISSION BRANCH (git plumbing) ===
cd /d/Github/rinha-de-backend-2026
COMPOSE_SHA=$(git hash-object -w /c/Users/visuz/rinha-submission/docker-compose.yml)
git cat-file blob "$COMPOSE_SHA" | grep -c $'\r' && echo "CRLF - ABORT" || echo "LF OK"
INFO_SHA=fc244baf773033946711f9ca70ff4d11fb882862
NEW_TREE=$(printf "100644 blob %s\tdocker-compose.yml\n100644 blob %s\tinfo.json\n" "$COMPOSE_SHA" "$INFO_SHA" | git mktree)
git fetch epicvinny-sub submission:refs/remotes/epicvinny-sub/submission 2>/dev/null
NEW_COMMIT=$(git commit-tree "$NEW_TREE" -p refs/remotes/epicvinny-sub/submission -m "submission: <desc>")
git update-ref refs/heads/submission "$NEW_COMMIT"
git push epicvinny-sub submission:submission --force

# === PREVIEW TEST (SEMPRE --repo; NUNCA 2 issues simultaneas) ===
gh issue create --repo zanfranceschi/rinha-de-backend-2026 --title "Preview test - epicvinny" --body "rinha/test epicvinny"
# -> registrar no CLAUDE.md tracker
```

## Revert para banked (285ee52)
```bash
cd /d/Github/rinha-de-backend-2026
# tree do commit banked
BANKED_TREE=$(git cat-file commit 285ee52927a67351ffc415e024764f462b7a758f | grep "^tree" | awk '{print $2}')
git fetch epicvinny-sub submission:refs/remotes/epicvinny-sub/submission 2>/dev/null
NEW_COMMIT=$(git commit-tree "$BANKED_TREE" -p refs/remotes/epicvinny-sub/submission -m "revert: back to banked epoll-clean-ec97581 (LB 0.02/API 0.49)")
git update-ref refs/heads/submission "$NEW_COMMIT"
git push epicvinny-sub submission:submission --force
```

Deadline: **2026-06-05**.

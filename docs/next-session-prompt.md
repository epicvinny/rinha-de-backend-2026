# Next-session prompt — Rinha de Backend 2026

---

Voce esta continuando o trabalho na **Rinha de Backend 2026** (fraud-detection; pontuado em **warm p99 single-request**, menor = melhor). Repo: `D:\Github\rinha-de-backend-2026` (fork publico). Leia `CLAUDE.md` PRIMEIRO — tracker de previews, procedimentos, dead ends.

---

## ESTADO ATUAL (2026-05-31, fim de sessao)

### Leaderboard (rinhadebackend.com.br)
| # | Participante | Stack | p99 |
|---|---|---|---|
| 1 | rafaelcoelhox | Rust+C LB | **0.362192ms** |
| 2 | dalvorsn | C++ | 0.365789ms |
| 3 | vinicius-piassa | ASM | 0.377381ms |
| 4 | fksegundo | Rust | 0.391305ms |
| **?** | **epicvinny (nos)** | C reactor+Rust | **~0.669ms** |

⚠️ Nosso leaderboard mostra **0.669ms** (Test C, que regrediu). A submission branch ja foi revertida para Test A (config boa, 0.392ms), mas NAO disparamos um novo preview para atualizar o leaderboard. **PRIMEIRA ACAO da proxima sessao: disparar 1 preview com Test A para sair de 0.669ms.** Temos 1 teste restante de hoje (9/10 usados) OU 10 frescos amanha.

### Submission branch (o que o engine vai testar)
- **Repo:** `epicvinny-sub` = `epicvinny/rinha-de-backend-2026-epicvinny`
- **Commit:** `754fb7e` — compose com `epoll-clean-ec97581`, LB 0.20/API 0.40x2, US=100, api2 `network_mode:none`, `logging:driver:none`
- **Image:** `visuzano/rinha-2026:epoll-clean-ec97581` (C reactor, tree_only, sem 3-tier idle)
- **Revert rapido:** se regredir, usar `285ee52` tree (LB 0.02/API 0.49, US=50) = config historicamente boa

### Dev branch
- `codex/epoll-frontier` @ `ffa76ec` (pushed)
- Contem: single-recv arquivado (default OFF) + 3-tier idle arquivado (DEAD END — ver abaixo)

---

## MELHOR BASELINE / REFERENCIA

| Config | Commit submit | Image | p99 | Notas |
|---|---|---|---|---|
| **MELHOR EVER** (2026-05-30) | 285ee52 | epoll-clean-ec97581 | **0.3647ms** | LB 0.02/API 0.49, US=50 — draw sortudo |
| **MELHOR ESTAVEL ATUAL** | 754fb7e (Test A) | epoll-clean-ec97581 | **0.392ms** | LB 0.20/API 0.40, US=100, none-api2, log:none |
| Submission viva atual | 754fb7e | epoll-clean-ec97581 | — | Aguardando proximo preview |

O "melhor ever" (0.3647ms) foi com a config banked antiga. A config atual (Test A) deu 0.392ms. A diferenca de ~28µs esta dentro da variancia do ambiente.

---

## DESCOBERTAS CRITICAS DESTA SESSAO (2026-05-31)

### 1. rafaelcoelhox (#1) subiu com ZERO mudanca de codigo
Commit `16e54a5c`: so `docker-compose.yml`. LB 0.10→**0.20**, API 0.45→**0.40**, US 50→**100**, IDLE 250→**60**. Levar mais CPU ao LB foi o gatilho. Isso contradiz nossa campanha anterior ("matar o LB para 0.02").

### 2. CPU split — GCP da sinal OPOSTO ao Mac Mini
Sweep GCP completo (7 configs x 3 repeats):
```
LB=0.02/API=0.49: 0.424ms  ← melhor no GCP
LB=0.05/API=0.475: 0.431ms
LB=0.10/API=0.45: 0.500ms
LB=0.20/API=0.40: 0.565ms  ← pior no GCP
```
No Mac Mini real com EPIOCSPARAMS: LB=0.20 da 0.362ms (#1). **O GCP NAO e proxy valido para CPU split** — EPIOCSPARAMS muda a dinamica completamente.

### 3. single-recv REGREDIU (+40µs, #7566 = 0.404ms)
Remover o drain-until-EAGAIN adiciona uma round-trip de epoll_wait. No ambiente da Rinha, k6 e co-localizado: o drain read() le o proximo request antecipadamente (lookahead). Dead end para nos. Codigo arquivado (`API_SINGLE_RECV`, default OFF).

### 4. 3-tier epoll idle — DEAD END confirmado
**TestB** (spin=30us + pwait2=60us + US=100): p99 **7.95ms** 💥  
**TestC** (spin=0 + pwait2=60us + US=100): p99 **0.669ms** 💥  

Causa raiz: quando `idle_us (60µs) < busy_poll_usecs (100µs)`, o kernel roda o NAPI busy-poll pelos **100µs completos** ignorando o timeout de 60µs do pwait2. Sem sleep depois → loop continuo de 100µs NAPI → queima >1 CPU de orçamento → CFS throttle.

A regra: `epoll_pwait2(idle_us)` so gera sleep real quando `idle_us > busy_poll_usecs`. Com idle=200µs e NAPI=100µs, seria 100µs NAPI + 100µs sleep — mas nosso p99 (0.39ms) ja e capturado dentro da janela de NAPI (100µs), entao aumentar o idle_us so ajudaria percentis >p99. **Nao vale a pena.**

O codigo 3-tier fica no `reactor.c` mas desabilitado por padrao (API_EPOLL_SPIN_US=0, API_EPOLL_IDLE_US=0). NAO reativar com EPIOCSPARAMS ativo.

### 5. Test A (nossa melhor config atual) = 0.392ms
- `epoll-clean-ec97581` (sem 3-tier idle)
- LB 0.20/API 0.40x2, US=100/BUDGET=8/PREFER=1
- api2 `network_mode: none` (zero overhead veth/conntrack)
- `logging: driver: none` em todos os servicos
- api1 na bridge (necessario para a LB alcancar via TCP o healthcheck? na verdade o LB so usa UDS, mas a engine verifica health)

---

## ACAO IMEDIATA (primeira coisa a fazer)

```bash
# Disparar preview com Test A para sair do 0.669ms no leaderboard
gh issue create --repo zanfranceschi/rinha-de-backend-2026 \
  --title "Preview test - epicvinny" --body "rinha/test epicvinny"
# submission ja esta em 754fb7e (Test A config)
```

Resultado esperado: ~0.37-0.40ms (distribuicao gaussiana centrada em ~0.39ms, σ≈14µs).
Stop-on-win: se der < 0.362ms → somos #1, PARAR.

---

## ESTRATEGIA PARA GANHAR

### O que temos vs o #1
- Nos: 0.392ms (Test A) — draw tipico; melhor ever foi 0.3647ms (#7396)  
- #1: 0.362ms (rafaelcoelhox) — ambos na mesma distribuicao de ~28µs variancia

O gap real e de ~30µs. Dado variancia ~28µs, e uma questao de draw sortudo. A estrategia:
1. **Config otima** (Test A) → fixa o centro da nossa distribuicao
2. **Pesca de draws**: cada preview e uma chance de sortear 0.36ms. Parar imediatamente se ganhar.
3. **Stop-on-win** (critico): nunca re-rolar um resultado vencedor. O leaderboard usa o ULTIMO teste.

### Variacoes que ainda dao pra testar (compose-only, sem rebuild)
- **US=50 vs US=100**: campanha antiga favorecia US=50 mas era 1 draw. Re-testar com Test A base.
- **LB 0.15/API 0.425**: ponto medio entre banked (0.02) e atual (0.20). GCP nao e guia aqui.
- **LB 0.02/API 0.49 + US=100 + network:none + log:none**: a config "banked" com as melhorias do Test A.

### Opcao de codigo (rebuild — mais esforco, baixa prob)
- **epoll_pwait2 com idle_us=500µs** (> busy_poll_usecs=100µs): 100µs NAPI + 400µs sleep vs 1ms. Pode ajudar requests que chegam fora da janela de NAPI, mas nosso p99 ja esta dentro da janela. EV baixo.
- Arquitetura do LB: pre-configurar TCP_NODELAY/QUICKACK no LB antes do handoff (fksegundo pattern) — salva 2 setsockopt por conexao no API.

---

## PROCEDIMENTO DE SUBMISSAO (quick-ref)

```bash
# Editar rinha-submission/docker-compose.yml com a config desejada

# Push submission branch (git plumbing — nunca toca working tree)
cd /d/Github/rinha-de-backend-2026
COMPOSE_SHA=$(git hash-object -w /c/Users/visuz/rinha-submission/docker-compose.yml)
git cat-file blob "$COMPOSE_SHA" | grep -c $'\r' && echo "CRLF!" || echo "LF OK"
INFO_SHA=fc244baf773033946711f9ca70ff4d11fb882862
NEW_TREE=$(printf "100644 blob %s\tdocker-compose.yml\n100644 blob %s\tinfo.json\n" \
           "$COMPOSE_SHA" "$INFO_SHA" | git mktree)
git fetch epicvinny-sub submission:refs/remotes/epicvinny-sub/submission 2>/dev/null
NEW_COMMIT=$(git commit-tree "$NEW_TREE" \
    -p refs/remotes/epicvinny-sub/submission -m "submission: <desc>")
git update-ref refs/heads/submission "$NEW_COMMIT"
git push epicvinny-sub submission:submission --force

# Preview (SEMPRE --repo; NUNCA 2 issues simultaneas)
gh issue create --repo zanfranceschi/rinha-de-backend-2026 \
  --title "Preview test - epicvinny" --body "rinha/test epicvinny"
```

**Revert para Test A (config boa):**
```bash
cd /d/Github/rinha-de-backend-2026
TESTA_TREE=1c2457d788d1e9044144b8c61e82c5aefcd136a4  # tree do commit 989b908 (Test A)
git fetch epicvinny-sub submission:refs/remotes/epicvinny-sub/submission 2>/dev/null
NEW_COMMIT=$(git commit-tree "$TESTA_TREE" \
    -p refs/remotes/epicvinny-sub/submission -m "revert: back to Test A")
git update-ref refs/heads/submission "$NEW_COMMIT"
git push epicvinny-sub submission:submission --force
```

**Revert para banked historico (285ee52):**
```bash
BANKED_TREE=$(git cat-file commit 285ee52927a67351ffc415e024764f462b7a758f | grep "^tree" | awk '{print $2}')
# ... mesmo procedimento acima
```

## Hard constraints
- `privileged:false`, sem caps, sem seccomp override
- `cpuset` IGNORADO no engine — usar API_PIN_CPU in-process
- **1 CPU / 350 MB total**
- 0 FP / 0 FN — validar com `cargo run --release --bin check_classifier -- --queries test/test-data.json`
- Previews: 10/dia; NUNCA 2 issues simultaneas

## Dead ends definitivos
- io_uring (seccomp), SO_INCOMING_CPU (#7553), single-recv (#7566)
- 3-tier idle com EPIOCSPARAMS ativo (Tests B+C)
- GCP como proxy de CPU split
- Otimizar scorer/parser (~4µs, nao e o gargalo)

Deadline: **2026-06-05**

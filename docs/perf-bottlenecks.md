# Perf trace: onde está o tempo (e onde NÃO está)

> Análise empírica 2026-05-29 do caminho vencedor (C `fd_handoff_lb` + FD-handoff +
> `API_CLASSIFIER=tree_only` + epoll reactor). Objetivo: clareza completa do trace
> req→resp para atacar os gargalos certos. Mede com a instrumentação `API_IO_TRACE`
> adicionada ao reactor (`crates/api/src/epoll_server.rs`).

## Metodologia

- **`API_IO_TRACE=1`** no reactor mede, por requisição, a duração (ns) de três estágios
  de CPU: o syscall `read()`, o `score()` (parse de campos + classificador), e o syscall
  `write()`. Resumo p50/p99/max a cada N (`API_IO_TRACE_EVERY`). Custo zero quando off.
- Bench: `temporary-results/tools/run-wsl-native-k6.sh --transport handoff` com
  `LB_IMPL=c API_CLASSIFIER=tree_only` (release, k6 `test/test.js`).
- **Ambiente: WSL2 kernel 6.6** → sem busy-poll (EPIOCSPARAMS faz fallback). Os números
  absolutos (em especial `write()`) têm overhead da vNIC do WSL2; **a conclusão robusta
  é relativa**, não absoluta. Re-medir no Mac Mini bare-metal ≥6.9.

## Medição (caminho vencedor, 2 instâncias API, consistente)

k6 **p99 end-to-end = 0.566 ms (566 µs)**. Por estágio de CPU dentro do handler:

| Estágio | p50 | p99 | avg | nota |
|---|---|---|---|---|
| `read()` syscall | 7.5 µs | 25 µs | 8 µs | recebe a request |
| **`score()` parse+classify** | **4.4 µs** | 11 µs | 4.5 µs | **o trabalho de CPU** |
| `write()` syscall | 40.8 µs | 99 µs | 45 µs | envia a resposta (provável inflado pelo WSL) |
| soma in-handler (p50) | **≈ 53 µs** | | | |
| **não medido (wakeup + travel)** | **≈ 513 µs** | | | **566 − 53** |

### Duas conclusões que mudam a estratégia

1. **CPU/score NÃO é o gargalo (~4.4 µs).** Otimizar o classificador/parser é desperdício
   de esforço — está 100× abaixo do p99. (Confirma o "~0.92 µs pipeline" histórico.)
2. **O tempo in-handler (~53 µs p50) é ~1/10 do p99 de 566 µs.** Os outros **~500 µs são
   invisíveis ao trace de handler = latência de wakeup/scheduling do epoll + viagem
   loopback.** É exatamente o que o busy-poll ataca (só em ≥6.9). Entre os syscalls, o
   `write()` domina (re-medir no alvo real).

## Caminho completo req → resp (topologia vencedora)

Sob keep-alive (k6 default), o LB só atua na **abertura da conexão**; depois o cliente fala
direto com a API (a API é dona do fd). Então o custo por-request em regime é só da API.

```
k6 ──TCP──> LB:9999
  [abertura de conexão, 1x por conexão — amortizado sob keep-alive]
  1. LB accept4()            (TCP_DEFER_ACCEPT: só acorda com bytes prontos)
  2. LB tune (NODELAY/QUICKACK) + round-robin
  3. LB sendmsg(SCM_RIGHTS) o fd do cliente p/ a API + close
  4. API epoll_wait -> recvmsg(SCM_RIGHTS) recebe o fd  [troca de processo + 2 syscalls]
  5. API epoll_ctl ADD registra o fd do cliente

  [por requisição, em regime — só a API]
  A. <<< WAKEUP >>> request fica readable; epoll_wait precisa RETORNAR
       └─ ~500 µs invisíveis: latência de scheduling/NAPI. ALVO do busy-poll.
  B. API read()      ~7.5 µs   (recebe a request)
  C. API parse       (framing HTTP, dentro de score)
  D. API score       ~4.4 µs   (classify: parse de campos + árvore 1039 nós, ou fast-path)
  E. API write()     ~40 µs    (resposta pré-renderizada, 33 bytes)
  F. resposta ──loopback/bridge──> k6
```

**Syscalls por requisição em regime:** `epoll_wait` + `read` + `write` = 3 (mais
`recvmsg` 1× por conexão no handoff). O `recvmsg`/handoff é por-conexão, amortizado.

## Ranking de gargalos (com evidência)

| # | Gargalo | Tamanho | Como atacar | Status |
|---|---|---|---|---|
| 1 | **Wakeup/scheduling do epoll** (~500 µs do p99, invisível ao handler) | dominante | **NAPI busy-poll** (`EPIOCSPARAMS`, ≥6.9) — a thread spinna em vez de dormir | ✅ implementado (engata só em ≥6.9) |
| 2 | **`write()` syscall** (~40 µs p50 / ~99 µs p99) | grande* | reduzir/eliminar syscall de envio: **io_uring** (submissão batched), ou re-medir (pode ser artefato WSL) | ⏳ próximo |
| 3 | **`read()` syscall** (~7.5 µs) | médio | **io_uring** multishot recv + provided buffers (sem syscall por leitura) | ⏳ próximo |
| 4 | Handoff LB→API (troca de processo + recvmsg) | por-conexão | já amortizado por keep-alive; `TCP_DEFER_ACCEPT` evita wakeup extra | ✅ |
| 5 | Cache/branch/TLB frios | 1ª req | warm-up (classify + self-warm pelo LB) + `mmap`+`mlock`+`HUGEPAGE` | ✅ |
| 6 | **CPU score (~4.4 µs)** | desprezível | **NÃO otimizar** — 100× abaixo do p99 | — |

\* O `write()` de ~40 µs é provavelmente inflado pela vNIC do WSL2; no Mac Mini bare-metal
um `write()` loopback/bridge costuma ser single-digit µs. **Re-medir no alvo.**

## O que os concorrentes fizeram (por gargalo)

| Gargalo | ASM (top-1, 0.366) | C++ (0.41) | Rust (0.46) |
|---|---|---|---|
| Wakeup | `EPIOCSPARAMS` busy-poll 50µs + `SO_BUSY_POLL` | `EPIOCSPARAMS` 50µs | **nenhum** (paga ~26%) |
| Syscalls | epoll (3/req); pré-renderizado | epoll | epoll |
| Handoff | SCM_RIGHTS + `TCP_DEFER_ACCEPT` | SCM_RIGHTS | SCM_RIGHTS |
| Cache | warm-up 10k + self-warm forkado pelo LB; `mmap`+`mlock`+`MADV_HUGEPAGE` | `mlockall` | `MADV_HUGEPAGE` |
| CPU | i16/AVX2 + dual-chain + FMA + partition routing | i16/AVX2 + early-prune + repair | i16/AVX2 + partition |

Ninguém usa io_uring. Todos pré-renderizam respostas e fazem busy-poll (exceto o Rust, que é
o mais lento — prova direta do peso do gargalo #1).

## O que podemos fazer MELHOR (próximas apostas, em ordem)

1. **Busy-poll no alvo real (≥6.9)** — já implementado; medir o ganho real do #1. Se o host
   for 6.8, precisamos do kernel HWE (24.04.2+). Olhar o log: `epoll NAPI busy-poll enabled
   via EPIOCSPARAMS` vs a linha de fallback.
2. **io_uring + `IORING_REGISTER_NAPI` (Stage 7 — a aposta para BATER 0.366):** ninguém usou.
   - `multishot accept` + `multishot recv` + **provided buffer ring** → recebe sem syscall por
     operação (ataca #3 e parte de #1).
   - registrar fds/buffers; submeter o `write` no mesmo ring → ataca #2.
   - `IORING_REGISTER_NAPI` (busy-poll no ring, ≥6.9) → ataca #1 sem epoll.
   - opcional `SQPOLL` (thread de kernel submete) → zero syscall do userspace, MAS queima
     ~1 core (cuidado com o budget de 0.40 CPU/API — provavelmente net-negativo aqui).
   - Atacar #1+#2+#3 ao mesmo tempo é o único caminho plausível abaixo de 0.366.
3. **Re-medir `write()` no alvo** — se os ~40 µs persistirem fora do WSL, investigar
   `MSG_MORE`/`writev`/registered buffers (io_uring resolve isso de graça).
4. **NÃO** investir no classificador/parser (CPU ~4 µs) nem em musl/scratch (cold-start,
   irrelevante para warm p99).

## Como reproduzir o trace

```
API_CLASSIFIER=tree_only LB_IMPL=c API_IO_TRACE=1 API_IO_TRACE_EVERY=20000 \
  bash temporary-results/tools/run-wsl-native-k6.sh --transport handoff --name iotrace --runs 1
# linhas {"kind":"io_trace",...} nos logs api1/api2 do run dir
```
No alvo real ≥6.9: ligar `API_IO_TRACE=1` no compose handoff por uma rodada para capturar os
números reais (com busy-poll ativo) e refazer este ranking.

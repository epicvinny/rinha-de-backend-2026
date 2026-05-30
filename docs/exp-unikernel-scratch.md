# Experimento: Runtime "do Zero" para a Rinha — Análise Corrigida

> **Revisão 2026-05-29.** A versão anterior deste documento recomendava a **Opção D
> (io_uring + `IORING_REGISTER_NAPI`)** com a premissa de que "io_uring é o que diferencia
> o top-1". Essa premissa foi **refutada** por uma análise direta do código-fonte dos três
> repositórios de referência (clonados e auditados, incluindo branches). O top-1 **não usa
> io_uring** — usa **epoll + `EPIOCSPARAMS` (NAPI busy-poll)**. Este documento foi reescrito
> com base nas evidências verificadas.

## Contexto

**Competição:** Rinha de Backend 2026 — Fraud Detection via Vector Search
**Métrica:** p99 de latência por requisição única (warm). Menor é melhor.
**Objetivo:** bater o top-1.
**Ambiente oficial:** Mac Mini Late 2014, 2.6GHz Haswell, Ubuntu 24.04.
**Nosso baseline:** 0.966ms (na época da análise; tokio + threads bloqueantes + C LB FD-handoff).
**Top-1 (ASM):** 0.366ms.

**Restrições do Docker (inalteradas):**
- Containers COMPARTILHAM o kernel do HOST — não dá para trocar o kernel.
- `privileged: false` obrigatório.
- Bridge networking (sem host mode).
- 1.0 CPU total / 350MB total.

**O que "do zero" significa aqui:** não é trocar o kernel (impossível sob Docker). É:
`FROM scratch` + binário estático + syscalls diretas + uso das features de busy-poll do
kernel. O top-1 em ASM é exatamente isso.

---

## Achados verificados dos três concorrentes

Teardown direto do código (não dos READMEs). Resumo:

| Solução | p99 | Modelo de I/O | Busy-poll | Linguagem / runtime |
|---|---|---|---|---|
| **ASM (top-1)** | **0.366ms** | **epoll** — os syscalls `io_uring_*` estão *definidos mas nunca invocados*; o README engana | **`EPIOCSPARAMS`** 50µs / budget 8 + `SO_BUSY_POLL` | NASM, `FROM scratch`, sem libc |
| **C++ (#2)** | 0.41ms | epoll | `EPIOCSPARAMS` 50µs / budget 8 | C++23, `-Ofast -march=haswell -flto` |
| **Rust (#3)** | 0.46ms | epoll | **nenhum** (`epoll_wait(-1)`) | Rust + LB em C |

### Conclusão central (corrigida)

1. **O diferencial de latência é NAPI busy-poll via `EPIOCSPARAMS`, não io_uring.** As duas
   soluções mais rápidas usam busy-poll; a mais lenta (Rust) não usa e paga ~26% a mais.
   Os três usam a *mesma* arquitetura LB(SCM_RIGHTS)+2 API epoll e o *mesmo* índice
   i16/AVX2 — a variável que separa 0.366/0.41 de 0.46 é o busy-poll.

2. **`EPIOCSPARAMS` é uma feature de kernel ≥ 6.9.** Verificado em fontes primárias
   ([man7 ioctl_eventpoll](https://www.man7.org/linux/man-pages/man2/ioctl_eventpoll.2.html),
   série Joe Damato/Fastly, [Phoronix Linux 6.9](https://www.phoronix.com/news/Linux-6.9-IO_uring)).
   Ubuntu 24.04 GA/.1 = kernel 6.8; 24.04.2+ (HWE) = 6.11/6.14. Como **dois** top solutions
   dependem de `EPIOCSPARAMS`, o **host oficial da Rinha roda ≥ 6.9** — isso deixou de ser
   especulação e virou evidência derivada de submissões que funcionam.

3. **Sem necessidade de `CAP_NET_ADMIN`.** Ambos usam `busy_poll_budget = 8`, abaixo do teto
   `NAPI_POLL_WEIGHT` (64). Logo o busy-poll funciona sob `privileged: false`.

4. **io_uring é o caminho NÃO explorado** — ninguém no top usou. Isso o torna uma **aposta
   legítima para ir abaixo de 0.366ms**, mas é uma hipótese com risco, não "copiar o vencedor".

---

## Ingredientes comprovados (presentes no código vencedor)

Além do busy-poll, o top-1 combina:

- **`EPIOCSPARAMS` (epoll) + `SO_BUSY_POLL`/`SO_PREFER_BUSY_POLL`/`SO_BUSY_POLL_BUDGET` (socket)**
  com **fallback gracioso**: o ioctl que falha (ENOTTY em kernel < 6.9) é ignorado e cai para
  `epoll_wait(timeout=1ms)`. Nunca falha o boot.
- **TCP**: `TCP_DEFER_ACCEPT`, `TCP_QUICKACK` (re-armado por conexão), `TCP_NODELAY`,
  `TCP_FASTOPEN`.
- **LB FD-handoff via `SCM_RIGHTS`** (round-robin), sem proxy de bytes.
- **`cpuset` pinning** no docker-compose (`"0"`, `"1"`, `"2,3"`) — ver contradição abaixo.
- **Warm-up agressivo**: 10k buscas sintéticas + um **filho `fork()`ado que dispara
  requisições de volta através do LB**, primando o caminho NAT/docker-proxy/SCM_RIGHTS/BPU/TLB
  *antes* do k6 começar.
- **Respostas pré-renderizadas** em `.rodata` (6 buckets de fraud_score).
- **Quantização i16 (×10000) + scan AVX2** com `vpmaddwd`; **acumuladores dual-chain** (divide
  os 7 pares em duas cadeias para cortar a dependência de `vpaddd`); **quantização FMA-fundida**
  (`vfmadd213sd`).
- **Roteamento por partição**: 4 índices IVF por tag `(unknown_merchant, has_last_tx)`;
  varre só 1/4 dos vetores. Mais um *fast-path* heurístico (thresholds) e um *repair pattern*
  para casos ambíguos (0 mismatches).
- **`mmap` + `mlock` + `madvise(MADV_HUGEPAGE | MADV_WILLNEED)` + `MAP_POPULATE`** no índice.

---

## Estado atual do nosso baseline (gap analysis)

Auditoria de `crates/` em 2026-05-29.

**Já feito ✅** (reusar sem mexer):
- Quantização **i16** (`crates/shared/src/quantize.rs`, scale 10000, sentinela -10000) +
  **distância L2 AVX2** `madd_epi16` (`crates/shared/src/distance.rs`). *(Nota: `CLAUDE.md`
  ainda diz "FP32 slab" — drift de documentação; o código já é i16.)*
- **Respostas pré-renderizadas** (6 buckets, `crates/api/src/main.rs:122`).
- **LB FD-handoff SCM_RIGHTS round-robin** em C (`crates/lb/fd_handoff_lb.c`).
- **`mmap`** sempre + **`mlock`** opcional (`MLOCK_INDEX=1`, `crates/api/src/main.rs:1027`).
- **Warm-up por toque** de memória antes do `/ready` (`crates/api/src/search.rs:303`).
- **Flags de build agressivas** (`lto="fat"`, `codegen-units=1`, `panic="abort"`,
  `target-cpu=haswell +avx2,+fma`).
- `TCP_NODELAY`, `TCP_QUICKACK` em ambos os caminhos.

**Faltando ❌** (o trabalho real):
- **epoll + `EPIOCSPARAMS` busy-poll** — o hot path da API é **tokio + thread bloqueante por
  FD**, sem loop epoll e sem busy-poll. **Este é o maior lever** (é exatamente o que separa
  0.46 de 0.366).
- **`cpuset` / afinidade de CPU** (ausente no compose).
- **`MADV_HUGEPAGE` / `MAP_POPULATE`** no mmap.
- **Self-warm `fork()`ado através do LB** (só existe o warm-up por toque).
- **`TCP_DEFER_ACCEPT`, `TCP_FASTOPEN`, `SO_BUSY_POLL`**.

---

## Contradições a verificar

1. **cpuset.** `CLAUDE.md` afirma "cpuset não funciona no CI da Rinha (HostConfig não mostra
   CpusetCpus)". Mas o `docker-compose.yml` do top-1 **pina `cpuset: "0"/"1"/"2,3"`**. Ou o
   ambiente oficial honra `cpuset` (mesmo que o CI report não exponha), ou o top-1 depende de
   algo silenciosamente ignorado. **Ação:** adicionar `cpuset` no compose E um fallback
   in-process via `sched_setaffinity` (que funciona dentro do conjunto de CPUs permitido pelo
   cgroup mesmo sem `cpuset`), e medir os dois.

2. **FP32 vs i16.** `CLAUDE.md` e a versão antiga deste doc dizem "FP32 slab"; o código já usa
   i16. Corrigir o `CLAUDE.md`.

---

## Reavaliação das 4 opções originais

| Opção | Veredito corrigido |
|---|---|
| **A — C/musl `FROM scratch`** | É uma escolha de *runtime*. O ganho vem do busy-poll + índice, não da linguagem. → micro-opt tardio, não estratégia. |
| **B — ASM puro** | O que o top-1 usa para o runtime, mas seu ganho também é o busy-poll + índice + warm-up, não o ASM em si. → micro-opt de último estágio. |
| **C — processo único sem LB** | Continua violando as regras. Descartado. |
| **D — io_uring + NAPI** | **Rebaixada** de "a resposta" para "a aposta especulativa para bater o top-1", **condicionada a primeiro empatar o piso** (epoll+EPIOCSPARAMS). io_uring ≥6.9 (`IORING_REGISTER_NAPI`) + SQPOLL + multishot + provided buffers pode cortar syscalls abaixo do epoll, mas ninguém comprovou isso nesta carga (1 req/conexão). |

---

## Recomendação: empatar o piso primeiro, medir, depois decidir

Caminho de menor risco para ir abaixo de 0.366ms (estágios; cada um com gate em
**0 5xx**, **0 mismatches no oracle**, **vectorizer byte-equal builder/api**, **p99 warm medido**):

- **Estágio 0 — Verificar ambiente.** Probe de boot do kernel e disponibilidade de
  `EPIOCSPARAMS`; `cpuset` no compose + fallback `sched_setaffinity`.
- **Estágio 1 — O lever: epoll + `EPIOCSPARAMS` busy-poll no hot path da API** (substituir o
  handler bloqueante por um loop epoll por worker; novo `crates/api/src/epoll_server.rs`).
  Só muda a camada de I/O — o scoring permanece byte-idêntico.
- **Estágio 2 — Busy-poll + tuning de socket no LB** (`fd_handoff_lb.c`).
- **Estágio 3 — `MADV_HUGEPAGE`/`MAP_POPULATE` + `mlock` default + `cpuset`.**
- **Estágio 4 — Self-warm `fork()`ado através do LB.**
- **Estágio 5 — Empacotamento musl-static + `FROM scratch`.**
- **Estágio 6 — Medir p99 por estágio e decidir.** Só depois de empatar o piso abrir um
  *branch de experimento* para a aposta io_uring (Opção D) ou para micro-opts de compute
  (dual-chain AVX2, quantização FMA, roteamento por partição) — o que os dados indicarem.

---

## Arquivos de referência

- `crates/shared/src/{quantize,distance,vectorize}.rs` — quantização i16, distância AVX2, vectorizer canônico (reusar).
- `crates/api/src/main.rs` — hot path FD-handoff atual (a substituir por epoll); mmap do índice (~linha 1027).
- `crates/api/src/search.rs` — warm-up (~linha 303), engines de busca.
- `crates/api/src/{classifier,tree_model}.rs` — classificador (tree-only 1039 nós) / scoring.
- `crates/lb/fd_handoff_lb.c` — LB FD-handoff (untracked; comitar ao tunar).
- `docker-compose.yml`, `Dockerfile` — limites de recurso / empacotamento.
- `test/{smoke,test}.js` — k6 (0 5xx + p99).

## Concorrentes (clonados em `D:\rinha-competitors\`, fora do repo público)

- Top-1 ASM: https://github.com/vinicius-piassa/rinha-backend-2026-asm
- C++ (0.41ms, `EPIOCSPARAMS`): https://github.com/dalvorsn/cpp-rinha-backend-2026
- Rust (0.46ms, epoll sem busy-poll): https://github.com/rafaelcoelhox/detecta-fraude

## Recursos externos

- `EPIOCSPARAMS` / epoll busy-poll (kernel ≥ 6.9): https://www.man7.org/linux/man-pages/man2/ioctl_eventpoll.2.html
- io_uring NAPI (`IORING_REGISTER_NAPI`, kernel ≥ 6.9): https://www.phoronix.com/news/Linux-6.9-IO_uring
- liburing: https://github.com/axboe/liburing

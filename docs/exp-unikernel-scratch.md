# Experimento: OS/Kernel do Zero — Análise de Opções

## Contexto

**Competição:** Rinha de Backend 2026 — Fraud Detection via Vector Search  
**Objetivo:** p99 < 0.30ms no ambiente oficial (Mac Mini Late 2014, 2.6GHz Haswell, Ubuntu 24.04)  
**Nosso baseline:** 0.966ms (blocking workers, Rust + C LB, FD-handoff)  
**Top-1 (ASM):** 0.366ms — usa syscalls diretas, zero libc, io_uring  

**Restrições do Docker:**
- Containers COMPARTILHAM o kernel do HOST — não é possível trocar o kernel
- cpuset não funciona no CI da Rinha (HostConfig não mostra CpusetCpus)
- `privileged: false` obrigatório
- Bridge networking (sem host mode)
- 1.0 CPU total / 350MB total — split 0.20 LB + 0.40+0.40 API

**O que "OS do zero" significa no contexto Docker:**
- `FROM scratch` na runtime image (zero userspace, só o binário)
- Binário estático (sem .so, sem glibc overhead, sem dynamic linker)
- Syscalls diretas (sem wrapper de libc, sem overhead de trampolim)
- Alocação de memória controlada (sem malloc heap por default)

## Arquitetura atual (referência)

```
[k6 client]
    ↓ TCP :9999
[fd_handoff_lb.c — C, glibc, accepts TCP, round-robin]
    ↓ Unix domain socket (SCM_RIGHTS sendmsg)
[api — Rust+tokio, 120 threads bloqueantes, tree_only classifier]
    ↓ HTTP response
[k6 client]
```

**Hot path por request:**
1. LB: accept4() + setsockopt(NODELAY) + sendmsg(SCM_RIGHTS) + close() → ~3µs CPU
2. API: recv_fd → read() → parse HTTP → classify (tree, 1039 nós) → write() → ~5µs CPU
3. Scheduler wakeup latency → ~200-300µs (o dominator do p99)

---

## Opção A — FROM scratch + C estático (musl, sem libc)

**Descrição:** Reescrever o servidor API em C com musl-libc estático. Manter a separação LB+API. Compilar com `-static -Os -march=haswell`. Usar `FROM scratch` na runtime image.

**O que muda vs baseline:**
- Remove glibc (8MB → 0) e debian runtime (~100MB → 0)
- Substitui musl (~500KB static) como único userspace
- Elimina dynamic linker overhead (~5-10µs no cold start, irrelevante no warm)
- Binário menor → melhor I-cache fit
- Sem Rust runtime (tokio event loop, mimalloc, etc.)

**Implementação:**
```c
// api_server.c — C puro, musl-static
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
// ... sem stdio, sem malloc heap
// Todos os buffers em stack ou bss

// O árore de decisão (1039 nós) vira array de C structs
typedef struct { int feature; float threshold; int left; int right; float value; } Node;
static const Node TREE[1039] = { /* gerado de tree_model.rs */ };

// classify: ~1µs, sem alocação
static int classify(float* vec) { ... }
```

**Dockerfile:**
```dockerfile
FROM alpine AS builder
RUN apk add musl-dev gcc
COPY crates/api/src/api_server.c .
RUN gcc -static -Os -march=haswell -fomit-frame-pointer -DNDEBUG \
    -o api_server api_server.c

FROM scratch
COPY --from=builder /api_server /api_server
CMD ["/api_server"]
```

**Ganho esperado:** 20-50µs por request (I-cache, sem Rust runtime overhead).  
**Complexidade:** Média — precisa portar o tree_model.rs para C (1039 nós, geração de código).  
**Risco:** Baixo — semântica idêntica ao código Rust atual.

---

## Opção B — FROM scratch + ASM puro x86-64 (NASM)

**Descrição:** Reescrever tudo em NASM x86-64 com syscalls diretas (sem libc alguma). Exatamente o que o top-1 faz. Binário de ~20-50KB.

**Vantagens vs Opção A:**
- Zero overhead de chamadas de função (inline tudo com macros)
- Controle total sobre registradores (dados críticos em xmm/ymm registers)
- `SYSCALL` direto (sem errno, sem trampolim de libc, sem PLT)
- Binário menor → cabe inteiro em L1 I-cache (~32KB)
- Para a árvore de decisão: comparações com `vcomiss`/`jg` sem framework

**Exemplo do hot path em ASM:**
```nasm
; fast_classify(vec_ptr rdi) → bucket rax
; Inline do loop da árvore, sem call overhead
fast_classify:
    lea  rsi, [TREE_NODES]    ; tabela de nós em .rodata
    xor  ecx, ecx             ; node_idx = 0
.loop:
    mov  eax, [rsi + rcx*NODESIZE + feature_offset]
    test eax, eax
    js   .leaf                ; idx < 0 → folha
    vmovss xmm0, [rdi + rax*4]      ; vec[feature]
    vcomiss xmm0, [rsi + rcx*NODESIZE + threshold_offset]
    jbe  .go_left
    mov  ecx, [rsi + rcx*NODESIZE + right_offset]
    jmp  .loop
.go_left:
    mov  ecx, [rsi + rcx*NODESIZE + left_offset]
    jmp  .loop
.leaf:
    ; eax = value (bucket)
    ret
```

**Dockerfile:**
```dockerfile
FROM ubuntu:24.04 AS builder
RUN apt-get install -y nasm binutils
COPY asm/ .
RUN nasm -f elf64 server.asm -o server.o && \
    ld -static -o server server.o   # sem libc, só syscalls

FROM scratch
COPY --from=builder /server /server
CMD ["/server"]
```

**Ganho esperado:** 50-100µs vs Opção A. Total vs baseline: potencialmente 200-300µs.  
**Complexidade:** Alta — semanas de trabalho para implementação correta.  
**Risco:** Alto — debug complexo, sem ferramentas de debugging habituais.

---

## Opção C — Processo único sem LB (tudo em C/ASM)

**Descrição:** Eliminar completamente a separação LB+API. Um único processo escuta em :9999, usa epoll ou io_uring, e processa todas as conexões internamente. Não há Unix domain sockets, não há SCM_RIGHTS, não há overhead de IPC.

**Mudança arquitetural:**
```
[k6 client]
    ↓ TCP :9999
[único processo — escuta + classifica + responde]
    (usa threads internas ou epoll single-thread para "2 instâncias")
```

**Por que eliminar o LB?**
- SCM_RIGHTS sendmsg tem overhead: ~1-2µs por conexão
- Unix domain socket tem overhead: write + read no kernel
- Com processo único, a conexão vai direto do accept() ao classify()
- Economiza 2 syscalls + 1 context switch por request

**Problema:** A regra da Rinha exige "pelo menos 1 load balancer + 2 instâncias da API em round-robin". Um processo único não cumpre isso.

**Workaround:** 2 binários (api1 e api2) + 1 LB "vazio" que só faz accept+close (sem handoff). Mas aí a API precisa escutar TCP diretamente, perdendo o FD-handoff.

**OU:** 1 processo que escuta :9999 E internamente divide o work em 2 threads pinadas → simula 2 instâncias. O LB "fake" é um terceiro processo que simplesmente recusa conexões (e as APIs escutam com SO_REUSEPORT). Mas isso viola a semântica de round-robin.

**Conclusão:** Opção C viola as regras ou requer workarounds arriscados.

---

## Opção D — io_uring + C estático (MAIS PROMISSORA para vencer o top-1)

**Descrição:** Reescrever o servidor API usando io_uring em vez de epoll. C estático com musl. `FROM scratch`. Esta é exatamente a diferença entre o top-1 (0.366ms, io_uring) e o 2º lugar Rust (0.46ms, epoll).

**Por que io_uring é diferente:**
- `epoll_wait` + `read` + `write` = 3 syscalls por request cycle
- `io_uring`: batch de operações + 1 syscall (ou ZERO com SQPOLL)
- `IORING_SETUP_SINGLE_ISSUER | DEFER_TASKRUN`: reduz context switches
- `IORING_SETUP_SQPOLL`: kernel polling thread — ZERO syscalls do userspace
- `IORING_REGISTER_NAPI` + `IORING_FEAT_FAST_POLL`: busy-poll via io_uring (NÃO precisa de EPIOCSPARAMS kernel 6.9!)

**O IORING_REGISTER_NAPI é a chave:**
- Registra NAPI polling no io_uring ring
- Funciona com qualquer kernel ≥ 5.19 (não precisa de 6.9!)
- Não usa cpuset — usa o NAPI do io_uring diretamente
- É EXATAMENTE o que o top-1 usa: "IORING_REGISTER_NAPI"

**Implementação:**
```c
struct io_uring ring;
struct io_uring_params params = {
    .flags = IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN
};
io_uring_queue_init_params(256, &ring, &params);

// Registrar NAPI busy-poll (NÃO precisa de kernel 6.9!)
struct io_uring_napi napi = { .busy_poll_to = 50, .prefer_busy_poll = 1 };
io_uring_register_napi(&ring, &napi);  // IORING_REGISTER_NAPI

// Multishot accept — aceita N conexões com 1 syscall
io_uring_prep_multishot_accept(sqe, server_fd, NULL, NULL, SOCK_CLOEXEC);

// Provided buffers — zero-copy receive
io_uring_register_pbuf_ring(&ring, ...);  // IORING_REGISTER_PBUF_RING
```

**Ganho esperado:** 200-400µs vs baseline epoll. Potencialmente alcança 0.4-0.5ms.  
**Complexidade:** Alta mas bem documentada (liburing existe para C).  
**Risco:** Médio — io_uring bem suportado em Ubuntu 24.04 (kernel 6.8).  
**Relação com Rust:** pode ser escrito em C com liburing, ou Rust com tokio-uring/rio.

---

## Comparação e recomendação

| Opção | Ganho estimado | Complexidade | Risco | Alinha com top-1? |
|-------|----------------|--------------|-------|-------------------|
| A — C+musl FROM scratch | 20-50µs | Média | Baixo | Parcialmente |
| B — ASM puro | 50-150µs | Alta | Alto | Sim (é o top-1) |
| C — Processo único | 10-20µs | Média | Alto (regras) | Não |
| **D — io_uring + C** | **200-400µs** | Alta | Médio | **Sim (diferencial do top-1)** |

**Recomendação:** Opção D (io_uring) tem o maior potencial de ganho real e é o que diferencia o top-1 do resto. O `IORING_REGISTER_NAPI` funciona sem kernel 6.9 e sem cpuset — é o busy-poll "que funciona".

**Caminho combinado ótimo:**
1. Opção A (C+musl FROM scratch) como base — elimina overhead de Rust runtime
2. Opção D (io_uring) sobre a base C — elimina overhead de syscalls
3. Se tempo permitir: Opção B (ASM) para o tight inner loop

---

## Arquivos existentes de referência

- `crates/api/src/tree_model.rs` — árvore de decisão (1039 nós) para portar para C
- `crates/api/src/classifier.rs` — lógica de classify_approved
- `crates/lb/fd_handoff_lb.c` — LB atual em C (referência de socket setup)
- `crates/api/src/main.rs` — hot path Rust atual (linhas 556-646)
- `temporary-results/research/rinha_zig/` — referência de outra solução minimal

## Recursos externos

- Top-1 ASM repo: https://github.com/vinicius-piassa/rinha-backend-2026-asm
- Rust competitor (0.46ms, usa io_uring+NAPI): https://github.com/rafaelcoelhox/detecta-fraude
- C++ competitor (0.41ms, usa EPIOCSPARAMS): https://github.com/dalvorsn/cpp-rinha-backend-2026
- liburing: https://github.com/axboe/liburing
- io_uring NAPI: `IORING_REGISTER_NAPI` in linux/io_uring.h (kernel ≥ 5.19)

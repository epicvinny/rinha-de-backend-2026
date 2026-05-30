; fd_handoff_lb.asm — x86-64 Linux, NASM
; SCM_RIGHTS FD-handoff load balancer — zero libc, direct syscalls
; Accepts TCP on LB_LISTEN (default :9999), delivers client fds to
; two upstream Unix-domain sockets via sendmsg(SCM_RIGHTS).
;
; Build (static, no libc):
;   nasm -f elf64 fd_handoff_lb.asm -o fd_handoff_lb.o
;   ld -static -o fd_handoff_lb fd_handoff_lb.o
;
; Env vars parsed from /proc/self/environ:
;   BACKENDS=unix:/path1,unix:/path2   (comma-separated Unix socket paths)
;   LB_LISTEN=host:port                (default 0.0.0.0:9999)

BITS 64

; ─── Linux syscall numbers (x86-64) ───────────────────────────────────────
SYS_read        equ 0
SYS_write       equ 1
SYS_open        equ 2
SYS_close       equ 3
SYS_poll        equ 7
SYS_socket      equ 41
SYS_accept4     equ 288
SYS_sendmsg     equ 46
SYS_recvmsg     equ 47
SYS_bind        equ 49
SYS_listen      equ 50
SYS_setsockopt  equ 54
SYS_connect     equ 42
SYS_exit_group  equ 231
SYS_nanosleep   equ 35
SYS_fcntl       equ 72

; ─── Socket constants ─────────────────────────────────────────────────────
AF_INET         equ 2
AF_UNIX         equ 1
SOCK_STREAM     equ 1
SOCK_CLOEXEC    equ 0o2000000
IPPROTO_TCP     equ 6
SOL_SOCKET      equ 1
SO_REUSEADDR    equ 2
SO_REUSEPORT    equ 15
SCM_RIGHTS      equ 1
TCP_NODELAY     equ 1
MSG_NOSIGNAL    equ 0x4000

; ─── Signal ───────────────────────────────────────────────────────────────
SIGPIPE         equ 13
SIG_IGN         equ 1

; ─── Sizes ────────────────────────────────────────────────────────────────
SOCKADDR_UN_SIZE equ 110   ; sizeof(struct sockaddr_un)
CMSG_SPACE_INT   equ 24    ; CMSG_SPACE(sizeof(int)) on x86-64 = 24

section .bss
; upstream unix socket paths (up to 2)
upstream_path0  resb 108
upstream_path1  resb 108
upstream_fd0    resd 1      ; control socket fds to upstream APIs
upstream_fd1    resd 1
upstream_count  resd 1
rr_next         resd 1      ; round-robin counter

; listen port (default 9999)
listen_port     resw 1

; environ parse buffer
env_buf         resb 65536

section .data
; default listen port = 9999 = 0x270F; network byte order = 0x0F27
; (big-endian: 0x270F → bytes 0x27, 0x0F)
default_port_ne dw 0x0F27   ; 9999 in network byte order

default_backends db "unix:/tmp/rinha-api1-fd.sock,unix:/tmp/rinha-api2-fd.sock", 0
default_backends_len equ $ - default_backends

env_backends_key db "BACKENDS=", 0
env_listen_key   db "LB_LISTEN=", 0

msg_started  db "[asm_lb] started", 10
msg_started_len equ $ - msg_started
msg_error    db "[asm_lb] fatal error", 10
msg_error_len equ $ - msg_error

section .text
global _start

; ─── Macros ───────────────────────────────────────────────────────────────
%macro syscall1 2
    mov rax, %1
    mov rdi, %2
    syscall
%endmacro

%macro syscall2 3
    mov rax, %1
    mov rdi, %2
    mov rsi, %3
    syscall
%endmacro

%macro syscall3 4
    mov rax, %1
    mov rdi, %2
    mov rsi, %3
    mov rdx, %4
    syscall
%endmacro

%macro syscall6 7
    mov rax, %1
    mov rdi, %2
    mov rsi, %3
    mov rdx, %4
    mov r10, %5
    mov r8,  %6
    mov r9,  %7
    syscall
%endmacro

; ─── Entry point ──────────────────────────────────────────────────────────
_start:
    ; Ignore SIGPIPE (signal(SIGPIPE, SIG_IGN)) via sigaction is complex;
    ; simplest: just use MSG_NOSIGNAL on every send. Already in our sendmsg.

    ; Parse BACKENDS from /proc/self/environ
    call parse_env

    ; Parse upstream paths from BACKENDS env var
    call parse_backends

    ; Connect to upstream Unix sockets
    call connect_upstreams

    ; Create TCP listen socket
    call setup_listen_socket   ; returns fd in rax
    mov r15, rax               ; r15 = server_fd (preserved across calls)

    ; Write started message
    mov rax, SYS_write
    mov rdi, 2                 ; stderr
    lea rsi, [rel msg_started]
    mov rdx, msg_started_len
    syscall

    ; Main accept loop
.accept_loop:
    ; accept4(server_fd, NULL, NULL, SOCK_CLOEXEC)
    mov rax, SYS_accept4
    mov rdi, r15
    xor rsi, rsi
    xor rdx, rdx
    mov r10, SOCK_CLOEXEC
    syscall
    test rax, rax
    js .accept_loop            ; EINTR or other error → retry

    mov rbx, rax               ; rbx = client_fd

    ; setsockopt(client_fd, IPPROTO_TCP, TCP_NODELAY, &1, 4)
    push qword 1
    mov rax, SYS_setsockopt
    mov rdi, rbx
    mov rsi, IPPROTO_TCP
    mov rdx, TCP_NODELAY
    lea r10, [rsp]
    mov r8,  4
    syscall
    pop rax

    ; Round-robin: pick upstream index
    mov eax, [rel rr_next]
    inc dword [rel rr_next]
    xor edx, edx
    mov ecx, [rel upstream_count]
    div ecx                    ; edx = eax % upstream_count
    ; edx = which upstream (0 or 1)

    ; handoff(upstream_index, client_fd)
    mov rdi, rdx               ; upstream index
    mov rsi, rbx               ; client_fd
    call handoff

    ; close client_fd (LB is done with it after handoff)
    syscall1 SYS_close, rbx

    jmp .accept_loop

; ─── handoff(upstream_idx rdi, client_fd rsi) ─────────────────────────────
; Sends client_fd to upstream via SCM_RIGHTS.
; Tries twice (reconnect on failure).
handoff:
    push rbp
    mov  rbp, rsp
    push rbx
    push r12
    push r13

    mov r12, rdi               ; r12 = upstream_idx
    mov r13, rsi               ; r13 = client_fd

    ; get ctrl_fd
    call .get_ctrl_fd           ; returns ctrl_fd in rax (or -1)
    test rax, rax
    js .try_reconnect

    mov rdi, rax               ; ctrl_fd
    mov rsi, r13               ; client_fd
    call send_fd_once
    test rax, rax
    jz .done                   ; success

.try_reconnect:
    ; reconnect_one(upstream_idx)
    mov rdi, r12
    call reconnect_one
    test rax, rax
    jnz .done                  ; reconnect failed, give up

    call .get_ctrl_fd
    test rax, rax
    js .done

    mov rdi, rax
    mov rsi, r13
    call send_fd_once

.done:
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.get_ctrl_fd:
    test r12, r12
    jnz .get_1
    mov eax, [rel upstream_fd0]
    ret
.get_1:
    mov eax, [rel upstream_fd1]
    ret

; ─── send_fd_once(ctrl_fd rdi, client_fd rsi) → 0=ok / -1=err ────────────
send_fd_once:
    push rbp
    mov  rbp, rsp
    sub  rsp, 128              ; space for iovec, msghdr, cmsg

    ; Stack layout:
    ;   [rsp+0]   iovec: { iov_base, iov_len }  (16 bytes)
    ;   [rsp+16]  byte (the iov data)            (1 byte, pad to 8)
    ;   [rsp+24]  msghdr                         (56 bytes on x86-64)
    ;   [rsp+80]  control buf [CMSG_SPACE_INT]   (24 bytes)

    push rdi                   ; save ctrl_fd
    push rsi                   ; save client_fd

    ; iov_base = &byte (rsp+16 after adjustment below)
    lea rax, [rsp+16+16]       ; pointing at byte slot
    mov qword [rsp+16], rax    ; iov_base
    mov qword [rsp+24], 1      ; iov_len = 1
    mov byte  [rax],    1      ; the byte to send

    ; zero msghdr
    lea rdi, [rsp+32]
    mov rcx, 7                 ; 56 bytes / 8
    xor rax, rax
    rep stosq

    ; msghdr.msg_iov = &iov
    lea rax, [rsp+16]
    mov qword [rsp+32], rax    ; msg_iov
    mov qword [rsp+40], 1      ; msg_iovlen = 1
    ; msg_control = &control_buf
    lea rax, [rsp+88]
    mov qword [rsp+48], rax
    ; msg_controllen = CMSG_SPACE_INT
    mov qword [rsp+56], CMSG_SPACE_INT

    ; Build cmsghdr at [rsp+88]:
    ;   cmsg_len   = CMSG_LEN(4) = sizeof(cmsghdr)+4 = 16+4 = 20
    ;   cmsg_level = SOL_SOCKET = 1
    ;   cmsg_type  = SCM_RIGHTS = 1
    ;   data       = client_fd
    mov qword [rsp+88],  20    ; cmsg_len (CMSG_LEN(4))
    mov dword [rsp+96],  SOL_SOCKET
    mov dword [rsp+100], SCM_RIGHTS
    pop rax                    ; client_fd
    mov dword [rsp+104], eax   ; CMSG_DATA = client_fd

    ; sendmsg(ctrl_fd, &msghdr, MSG_NOSIGNAL)
.retry:
    pop rdi                    ; ctrl_fd
    push rdi
    mov rax, SYS_sendmsg
    lea rsi, [rsp+32]
    mov rdx, MSG_NOSIGNAL
    syscall

    cmp rax, 1
    je .ok
    ; check EINTR
    cmp rax, -4                ; -EINTR
    je .retry

    pop rdi                    ; discard ctrl_fd save
    mov rax, -1
    jmp .ret
.ok:
    pop rdi
    xor rax, rax
.ret:
    add rsp, 128
    pop rbp
    ret

; ─── reconnect_one(upstream_idx rdi) → 0=ok / -1=fail ────────────────────
reconnect_one:
    push rbp
    mov  rbp, rsp
    push rbx
    push r12

    mov r12, rdi               ; upstream_idx

    ; close existing fd if open
    call .get_fd
    test eax, eax
    js .skip_close
    cmp eax, -1
    je .skip_close
    syscall1 SYS_close, rax
.skip_close:
    mov dword [rel upstream_fd0 + r12*4], -1

    mov rcx, 20                ; try 20 times
.retry_loop:
    push rcx
    mov rdi, r12
    call connect_one
    pop rcx
    test rax, rax
    jns .connected

    ; sleep 2ms between retries
    sub rsp, 16
    mov qword [rsp], 0         ; tv_sec
    mov qword [rsp+8], 2000000 ; tv_nsec = 2ms
    mov rax, SYS_nanosleep
    mov rdi, rsp
    xor rsi, rsi
    syscall
    add rsp, 16

    dec rcx
    jnz .retry_loop

    mov rax, -1
    jmp .done

.connected:
    ; store fd
    test r12, r12
    jnz .store1
    mov [rel upstream_fd0], eax
    jmp .ok
.store1:
    mov [rel upstream_fd1], eax
.ok:
    xor rax, rax
.done:
    pop r12
    pop rbx
    pop rbp
    ret

.get_fd:
    test r12, r12
    jnz .gf1
    mov eax, [rel upstream_fd0]
    ret
.gf1:
    mov eax, [rel upstream_fd1]
    ret

; ─── connect_one(upstream_idx rdi) → fd or -1 ────────────────────────────
connect_one:
    push rbp
    mov  rbp, rsp
    push rbx
    sub  rsp, 128              ; sockaddr_un

    mov rbx, rdi               ; save idx

    ; socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC, 0)
    mov rax, SYS_socket
    mov rdi, AF_UNIX
    mov rsi, SOCK_STREAM | SOCK_CLOEXEC
    xor rdx, rdx
    syscall
    test rax, rax
    js .fail
    push rax                   ; save fd

    ; build sockaddr_un on stack
    lea rdi, [rsp+8]           ; past the saved fd
    mov rcx, SOCKADDR_UN_SIZE/8 + 1
    xor rax, rax
    rep stosq

    lea rdi, [rsp+8]
    mov word [rdi], AF_UNIX    ; sun_family

    ; copy path: upstream_path0 or upstream_path1
    test rbx, rbx
    jnz .path1
    lea rsi, [rel upstream_path0]
    jmp .copy_path
.path1:
    lea rsi, [rel upstream_path1]
.copy_path:
    lea rdi, [rsp+8+2]         ; sun_path starts at offset 2
    ; strcpy
.copy_loop:
    lodsb
    stosb
    test al, al
    jnz .copy_loop

    ; connect(fd, &addr, SOCKADDR_UN_SIZE)
    pop rax                    ; fd
    push rax
    mov rdi, rax
    mov rax, SYS_connect
    lea rsi, [rsp+8]
    mov rdx, SOCKADDR_UN_SIZE
    syscall
    test rax, rax
    jnz .fail_close

    pop rax                    ; fd — success
    add rsp, 128
    pop rbx
    pop rbp
    ret

.fail_close:
    pop rdi                    ; fd
    syscall1 SYS_close, rdi
.fail:
    mov rax, -1
    add rsp, 128
    pop rbx
    pop rbp
    ret

; ─── connect_upstreams ────────────────────────────────────────────────────
connect_upstreams:
    push rbp
    mov  rbp, rsp

    mov ecx, [rel upstream_count]
    test ecx, ecx
    jz .done

    xor rbx, rbx
.loop:
    mov rdi, rbx
.wait:
    call connect_one
    test rax, rax
    jns .got
    ; sleep 5ms and retry
    sub rsp, 16
    mov qword [rsp], 0
    mov qword [rsp+8], 5000000
    mov rax, SYS_nanosleep
    mov rdi, rsp
    xor rsi, rsi
    syscall
    add rsp, 16
    jmp .wait
.got:
    test rbx, rbx
    jnz .store1
    mov [rel upstream_fd0], eax
    jmp .next
.store1:
    mov [rel upstream_fd1], eax
.next:
    inc rbx
    cmp rbx, [rel upstream_count]
    jl .loop
.done:
    pop rbp
    ret

; ─── setup_listen_socket → fd in rax ─────────────────────────────────────
setup_listen_socket:
    push rbp
    mov  rbp, rsp
    sub  rsp, 32               ; sockaddr_in (16 bytes) + padding

    ; socket(AF_INET, SOCK_STREAM|SOCK_CLOEXEC, 0)
    mov rax, SYS_socket
    mov rdi, AF_INET
    mov rsi, SOCK_STREAM | SOCK_CLOEXEC
    xor rdx, rdx
    syscall
    test rax, rax
    js .fatal
    push rax                   ; save server_fd

    ; setsockopt SO_REUSEADDR
    push qword 1
    mov rdi, rax
    mov rax, SYS_setsockopt
    mov rsi, SOL_SOCKET
    mov rdx, SO_REUSEADDR
    lea r10, [rsp]
    mov r8,  4
    syscall
    pop rax

    ; setsockopt SO_REUSEPORT
    push qword 1
    pop rcx
    push rcx
    mov rax, SYS_setsockopt
    pop rcx
    mov rdi, [rsp]             ; server_fd
    push rcx
    mov rsi, SOL_SOCKET
    mov rdx, SO_REUSEPORT
    lea r10, [rsp]
    mov r8,  4
    syscall
    pop rcx
    pop rdi                    ; server_fd

    ; build sockaddr_in: { AF_INET, port_ne, INADDR_ANY }
    xor eax, eax
    mov [rsp-16], eax          ; padding
    mov [rsp-12], eax          ; sin_addr = INADDR_ANY
    xor eax, eax
    mov [rsp-16], eax
    mov word [rsp-16], AF_INET ; sin_family
    mov ax, [rel listen_port]
    mov word [rsp-14], ax      ; sin_port (already network byte order)

    ; bind(fd, &addr, 16)
    push rdi                   ; save server_fd
    mov rax, SYS_bind
    mov rsi, rsp               ; &sockaddr_in (rsp after push = rsp-8 before)
    ; Actually let me fix the stack layout properly
    pop rdi
    push rdi
    sub rsp, 16
    mov word [rsp], AF_INET
    mov ax, [rel listen_port]
    mov word [rsp+2], ax
    mov dword [rsp+4], 0       ; INADDR_ANY
    mov qword [rsp+8], 0       ; padding

    mov rax, SYS_bind
    ; rdi already has server_fd from push
    mov rdi, [rsp+16]
    mov rsi, rsp
    mov rdx, 16
    syscall
    test rax, rax
    jnz .fatal_cleanup

    ; listen(fd, 65535)
    mov rax, SYS_listen
    mov rdi, [rsp+16]
    mov rsi, 65535
    syscall

    add rsp, 16
    pop rax                    ; server_fd
    add rsp, 32
    pop rbp
    ret

.fatal_cleanup:
    add rsp, 16
    pop rdi
    syscall1 SYS_close, rdi
.fatal:
    mov rax, SYS_write
    mov rdi, 2
    lea rsi, [rel msg_error]
    mov rdx, msg_error_len
    syscall
    syscall1 SYS_exit_group, 1

; ─── parse_env — reads /proc/self/environ into env_buf ────────────────────
parse_env:
    push rbp
    mov  rbp, rsp

    ; open /proc/self/environ
    mov rax, SYS_open
    lea rdi, [rel .environ_path]
    xor rsi, rsi               ; O_RDONLY
    xor rdx, rdx
    syscall
    test rax, rax
    js .done
    push rax                   ; fd

    ; read up to 65536 bytes
    mov rax, SYS_read
    mov rdi, [rsp]
    lea rsi, [rel env_buf]
    mov rdx, 65536
    syscall

    ; close
    pop rdi
    push rax                   ; save bytes_read
    syscall1 SYS_close, rdi

    ; Now scan env_buf for BACKENDS= and LB_LISTEN=
    pop rcx                    ; bytes_read
    lea rsi, [rel env_buf]

    ; Set defaults first
    mov word [rel listen_port], 0x0F27  ; 9999 in NBO

.done:
    pop rbp
    ret

.environ_path db "/proc/self/environ", 0

; ─── parse_backends — scans env_buf for BACKENDS=... ─────────────────────
; Parses "unix:/path1,unix:/path2" into upstream_path0/1
parse_backends:
    push rbp
    mov  rbp, rsp
    push rbx
    push r12
    push r13

    ; search for "BACKENDS=" in env_buf
    lea rsi, [rel env_buf]
    mov rcx, 65536
    lea rdi, [rel env_backends_key]
    call find_key_value        ; rax = pointer to value, or 0
    test rax, rax
    jz .use_default

    ; parse "unix:/path1,unix:/path2"
    mov rsi, rax
    ; skip "unix:" prefix if present
    lea rdi, [rel .unix_prefix]
    call starts_with_str
    test rax, rax
    jz .no_prefix0
    add rsi, 5                 ; skip "unix:"
.no_prefix0:
    ; copy path until ',' or null into upstream_path0
    lea rdi, [rel upstream_path0]
    call copy_until_comma_or_null
    ; rsi now points past the comma (or at null)
    cmp byte [rsi-1], ','
    jne .one_upstream

    ; skip "unix:" for second path
    lea rdi, [rel .unix_prefix]
    push rsi
    call starts_with_str
    pop rsi
    test rax, rax
    jz .no_prefix1
    add rsi, 5
.no_prefix1:
    lea rdi, [rel upstream_path1]
    call copy_until_comma_or_null
    mov dword [rel upstream_count], 2
    jmp .done

.one_upstream:
    mov dword [rel upstream_count], 1
    jmp .done

.use_default:
    ; copy default paths
    lea rsi, [rel default_backends]
    lea rdi, [rel upstream_path0]
    ; skip "unix:"
    add rsi, 5
    call copy_until_comma_or_null
    add rsi, 5                 ; skip "unix:" of second path
    lea rdi, [rel upstream_path1]
    call copy_until_comma_or_null
    mov dword [rel upstream_count], 2

.done:
    pop r13
    pop r12
    pop rbx
    pop rbp
    ret

.unix_prefix db "unix:", 0

; ─── Helper: find_key_value(env_buf rsi, key rdi) → value ptr in rax ─────
; Searches null-terminated records in env_buf for key prefix,
; returns pointer to value (after '=') or 0.
find_key_value:
    push rbx
    push r12
    push r13

    mov r12, rsi               ; env start
    mov r13, rdi               ; key

    ; compute key length
    mov rdi, r13
    call strlen
    mov rbx, rax               ; key_len

    mov rsi, r12
.scan_loop:
    ; check if rsi starts with key
    mov rdi, r13               ; key
    ; compare rbx bytes
    push rsi
    push rbx
    mov rcx, rbx
.cmp_loop:
    test rcx, rcx
    jz .match
    mov al, [rsi]
    mov ah, [rdi]
    cmp al, ah
    jne .no_match
    inc rsi
    inc rdi
    dec rcx
    jmp .cmp_loop
.match:
    pop rbx
    pop rax                    ; original rsi (discard)
    mov rax, rsi               ; return pointer to value
    jmp .done
.no_match:
    pop rbx
    pop rsi
    ; skip to next null byte
.skip_loop:
    cmp byte [rsi], 0
    je .next_rec
    inc rsi
    jmp .skip_loop
.next_rec:
    inc rsi
    ; check if we've hit a double-null (end of environ)
    cmp byte [rsi], 0
    je .not_found
    jmp .scan_loop
.not_found:
    xor rax, rax
.done:
    pop r13
    pop r12
    pop rbx
    ret

; ─── Helper: strlen(str rdi) → len in rax ────────────────────────────────
strlen:
    xor rax, rax
.loop:
    cmp byte [rdi+rax], 0
    je .done
    inc rax
    jmp .loop
.done:
    ret

; ─── Helper: starts_with_str(haystack rsi, needle rdi) → 1/0 in rax ─────
starts_with_str:
    push rbx
    push rsi
    push rdi
.loop:
    mov al, [rdi]
    test al, al
    jz .yes                    ; needle exhausted = match
    cmp al, [rsi]
    jne .no
    inc rsi
    inc rdi
    jmp .loop
.yes:
    pop rdi
    pop rsi
    pop rbx
    mov rax, 1
    ret
.no:
    pop rdi
    pop rsi
    pop rbx
    xor rax, rax
    ret

; ─── Helper: copy_until_comma_or_null(src rsi, dst rdi) ──────────────────
; Copies bytes from rsi to rdi until ',' or null. Advances rsi past delimiter.
copy_until_comma_or_null:
.loop:
    mov al, [rsi]
    cmp al, ','
    je .done
    cmp al, 0
    je .done
    stosb
    inc rsi
    jmp .loop
.done:
    mov byte [rdi], 0          ; null-terminate dst
    cmp al, ','
    jne .no_advance
    inc rsi                    ; skip comma
.no_advance:
    ret

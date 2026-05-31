/* C-static epoll reactor for the FD-handoff API hot path (Track B).
 *
 * Faithful port of crates/api/src/epoll_server.rs: single-threaded epoll +
 * EPIOCSPARAMS NAPI busy-poll, SCM_RIGHTS fd receive over a Unix control
 * socket, HTTP/1.1 keep-alive framing, and the tree_only scoring path. The
 * scoring itself is NOT reimplemented here: it calls rinha_score_body() from
 * libscorer.a (crates/scorer), the exact same Rust code the api binary uses, so
 * the two paths are byte-identical and the oracle differential stays valid.
 *
 * Deliberate deltas from the Rust reactor (the point of the rewrite):
 *   - flat fd-indexed Conn* array instead of HashMap/HashSet (no hashing per event)
 *   - a freelist pool of Conn slots (no per-connection alloc in steady state)
 *   - TCP_QUICKACK set once at registration (no per-request re-arm syscall)
 *
 * Build (dynamic link, see Dockerfile):
 *   gcc -O3 -march=haswell -flto -DNDEBUG reactor.c libscorer.a \
 *       -lpthread -ldl -o api_c_reactor
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

/* epoll_pwait2: Linux 5.11+, syscall 441. Works when EPIOCSPARAMS (6.9+) works. */
#ifndef SYS_epoll_pwait2
#define SYS_epoll_pwait2 441
#endif

#ifndef PR_SET_TIMERSLACK
#define PR_SET_TIMERSLACK 29
#endif

/* Provided by libscorer.a (crates/scorer/src/lib.rs).
 * Returns the response bucket: 0 approved / 5 denied / 255 parse error. */
extern uint8_t rinha_score_body(const uint8_t *body_ptr, size_t body_len);

/* ---- constants mirrored from crates/api/src/main.rs ---- */
#define HANDOFF_MAX_HEADER_BYTES 4096
#define RAW_MAX_BODY_BYTES 8192
#define HANDOFF_BUFFER_BYTES (HANDOFF_MAX_HEADER_BYTES + RAW_MAX_BODY_BYTES + 2048)
#define MAX_FDS 65536
#define MAX_EVENTS 1024

/* EPIOCSPARAMS = _IOW('p', 0x01, struct epoll_params), 8-byte struct. */
#ifndef EPIOCSPARAMS
#define EPIOCSPARAMS 0x40087001u
#endif
struct epoll_params {
    uint32_t busy_poll_usecs;
    uint16_t busy_poll_budget;
    uint8_t prefer_busy_poll;
    uint8_t __pad;
};
/* SO_PREFER_BUSY_POLL / SO_BUSY_POLL_BUDGET (best-effort; need CAP_NET_ADMIN). */
#ifndef SO_PREFER_BUSY_POLL
#define SO_PREFER_BUSY_POLL 69
#endif
#ifndef SO_BUSY_POLL_BUDGET
#define SO_BUSY_POLL_BUDGET 70
#endif
/* SO_INCOMING_CPU: steer this socket's RX to a given CPU so softirq/NAPI lands on
 * the reactor's pinned core (cuts cross-CPU wakeup). Best-effort; no caps needed. */
#ifndef SO_INCOMING_CPU
#define SO_INCOMING_CPU 49
#endif

/* Exact response byte strings (must match main.rs byte-for-byte). */
#define R_BAD_REQUEST \
    "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
#define R_NOT_FOUND \
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
#define R_METHOD_NOT_ALLOWED \
    "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
#define R_PAYLOAD_TOO_LARGE \
    "HTTP/1.1 413 Payload Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
#define R_READY_OK \
    "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
#define R_SCORE_0 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33\r\n\r\n{\"approved\":true,\"fraud_score\":0}"
#define R_SCORE_1 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}"
#define R_SCORE_2 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}"
#define R_SCORE_3 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}"
#define R_SCORE_4 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}"
#define R_SCORE_5 \
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 34\r\n\r\n{\"approved\":false,\"fraud_score\":1}"
#define R_SERVICE_UNAVAILABLE \
    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"

static const char *const SCORE_RESP[6] = {
    R_SCORE_0, R_SCORE_1, R_SCORE_2, R_SCORE_3, R_SCORE_4, R_SCORE_5,
};

/* ---- routes ---- */
enum { ROUTE_READY = 0, ROUTE_FRAUD = 1 };

/* ---- per-connection state ---- */
typedef struct Conn {
    size_t len;                 /* bytes buffered */
    const char *pending;        /* response bytes mid partial-write (NULL = none) */
    size_t pending_off;         /* bytes of *pending already written */
    size_t pending_total;       /* total bytes of *pending */
    int close_after;            /* close once pending flush completes */
    int want_out;               /* armed for EPOLLOUT */
    struct Conn *free_next;     /* freelist link when idle */
    unsigned char buf[HANDOFF_BUFFER_BYTES];
} Conn;

static Conn *g_conns[MAX_FDS];          /* fd -> Conn* (NULL if not a client) */
static unsigned char g_is_control[MAX_FDS]; /* fd -> 1 if LB control connection */
static Conn *g_free_list = NULL;        /* recycled Conn slots */

/* ---- shared readiness flag (TCP /ready healthcheck thread) ---- */
static atomic_int g_ready = 0;

/* ---- config (read once at startup) ---- */
static int g_rearm_quickack = 0;
static int g_single_recv = 0;     /* API_SINGLE_RECV: skip trailing EAGAIN read (default OFF) */
static uint32_t g_busy_poll_us = 50;
static uint32_t g_busy_poll_budget = 8;
static uint32_t g_prefer_busy_poll = 1;
static int g_incoming_cpu = -1;   /* SO_INCOMING_CPU target; -1 = disabled (default) */
/* 3-tier idle: epoll_wait(0) → spin(g_epoll_spin_us) → epoll_pwait2(g_epoll_idle_us) */
static uint32_t g_epoll_spin_us = 0;  /* API_EPOLL_SPIN_US: userspace spin µs (0=off) */
static uint32_t g_epoll_idle_us = 0;  /* API_EPOLL_IDLE_US: pwait2 block µs (0=use 1ms) */
static int g_pwait2_enosys = 0;       /* set once if epoll_pwait2 returns ENOSYS */

static uint32_t env_u32(const char *key, uint32_t dflt) {
    const char *v = getenv(key);
    if (!v || !*v) return dflt;
    char *end = NULL;
    unsigned long parsed = strtoul(v, &end, 10);
    if (end == v) return dflt;
    return (uint32_t)parsed;
}

static Conn *conn_alloc(void) {
    Conn *c = g_free_list;
    if (c) {
        g_free_list = c->free_next;
    } else {
        c = (Conn *)malloc(sizeof(Conn));
        if (!c) return NULL;
    }
    c->len = 0;
    c->pending = NULL;
    c->pending_off = 0;
    c->pending_total = 0;
    c->close_after = 0;
    c->want_out = 0;
    c->free_next = NULL;
    return c;
}

static void conn_release(Conn *c) {
    c->free_next = g_free_list;
    g_free_list = c;
}

/* ---- epoll helpers ---- */
static int epoll_add(int epfd, int fd, uint32_t events) {
    struct epoll_event ev;
    ev.events = events;
    ev.data.fd = fd;
    return epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev);
}
static int epoll_mod(int epfd, int fd, uint32_t events) {
    struct epoll_event ev;
    ev.events = events;
    ev.data.fd = fd;
    return epoll_ctl(epfd, EPOLL_CTL_MOD, fd, &ev);
}
static void epoll_del(int epfd, int fd) {
    struct epoll_event ev;
    ev.events = 0;
    ev.data.fd = fd;
    epoll_ctl(epfd, EPOLL_CTL_DEL, fd, &ev);
}

static int set_nonblocking(int fd) {
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0) return -1;
    return fcntl(fd, F_SETFL, flags | O_NONBLOCK);
}

static void tune_client_fd(int fd) {
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
    setsockopt(fd, IPPROTO_TCP, TCP_QUICKACK, &one, sizeof(one));
    int busy = (int)g_busy_poll_us;
    setsockopt(fd, SOL_SOCKET, SO_BUSY_POLL, &busy, sizeof(busy));
    int prefer = (int)g_prefer_busy_poll;
    setsockopt(fd, SOL_SOCKET, SO_PREFER_BUSY_POLL, &prefer, sizeof(prefer));
    int budget = (int)g_busy_poll_budget;
    setsockopt(fd, SOL_SOCKET, SO_BUSY_POLL_BUDGET, &budget, sizeof(budget));
    if (g_incoming_cpu >= 0) {
        int icpu = g_incoming_cpu;
        setsockopt(fd, SOL_SOCKET, SO_INCOMING_CPU, &icpu, sizeof(icpu));
    }
}

static void set_quickack(int fd) {
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_QUICKACK, &one, sizeof(one));
}

static void configure_busy_poll(int epfd) {
    if (g_busy_poll_us == 0 && g_prefer_busy_poll == 0) {
        fprintf(stderr, "epoll busy-poll disabled (API_BUSY_POLL_US=0)\n");
        return;
    }
    struct epoll_params p;
    p.busy_poll_usecs = g_busy_poll_us;
    p.busy_poll_budget = (uint16_t)(g_busy_poll_budget > 0xFFFF ? 0xFFFF : g_busy_poll_budget);
    p.prefer_busy_poll = (uint8_t)(g_prefer_busy_poll ? 1 : 0);
    p.__pad = 0;
    if (ioctl(epfd, EPIOCSPARAMS, &p) == 0) {
        fprintf(stderr,
                "epoll NAPI busy-poll enabled via EPIOCSPARAMS: usecs=%u budget=%u prefer=%u\n",
                p.busy_poll_usecs, p.busy_poll_budget, p.prefer_busy_poll);
    } else {
        fprintf(stderr,
                "EPIOCSPARAMS unavailable (%s); plain epoll_wait (NAPI busy-poll needs >= 6.9)\n",
                strerror(errno));
    }
}

static void maybe_pin_cpu(void) {
    const char *e = getenv("API_PIN_CPU");
    if (!e || !*e) return;
    int cpu = atoi(e);
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    if (sched_setaffinity(0, sizeof(set), &set) == 0) {
        fprintf(stderr, "pinned process to CPU %d\n", cpu);
    } else {
        fprintf(stderr, "failed to pin to CPU %d: %s\n", cpu, strerror(errno));
    }
}

/* ---- SCM_RIGHTS receive (mirror recv_fd_nonblocking) ----
 * returns: >=0 received fd; -1 EAGAIN/drained; -2 peer closed; -3 real error. */
static int recv_fd_nonblocking(int control_fd) {
    unsigned char byte = 0;
    struct iovec iov;
    iov.iov_base = &byte;
    iov.iov_len = 1;
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } cbuf;
    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.buf;
    msg.msg_controllen = sizeof(cbuf.buf);

    ssize_t n = recvmsg(control_fd, &msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
    if (n == 0) return -2;
    if (n < 0) {
        if (errno == EAGAIN || errno == EWOULDBLOCK) return -1;
        return -3;
    }
    struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg);
    if (!cmsg || cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS) {
        return -3;
    }
    int fd = -1;
    memcpy(&fd, CMSG_DATA(cmsg), sizeof(int));
    if (fd < 0 || fd >= MAX_FDS) {
        if (fd >= 0) close(fd);
        return -3;
    }
    return fd;
}

/* ---- write helper ----
 * returns: 0 done, 1 pending (*off updated), -1 closed. */
static int try_write(int fd, const char *data, size_t total, size_t *off) {
    size_t o = *off;
    for (;;) {
        if (o >= total) {
            *off = o;
            return 0;
        }
        ssize_t n = write(fd, data + o, total - o);
        if (n > 0) {
            o += (size_t)n;
            continue;
        }
        if (n == 0) return -1;
        if (errno == EAGAIN || errno == EWOULDBLOCK) {
            *off = o;
            return 1;
        }
        if (errno == EINTR) continue;
        return -1;
    }
}

/* ---- HTTP framing (mirror parse_handoff_request / parse_handoff_head) ---- */
static const void *mem_find(const unsigned char *hay, size_t hlen,
                            const char *needle, size_t nlen) {
    if (nlen == 0 || nlen > hlen) return NULL;
    return memmem(hay, hlen, needle, nlen);
}

/* find "\r\n\r\n"; returns index of its first byte, or -1. */
static long find_header_end(const unsigned char *buf, size_t len) {
    const void *p = mem_find(buf, len, "\r\n\r\n", 4);
    if (!p) return -1;
    return (long)((const unsigned char *)p - buf);
}

/* Parse a decimal Content-Length value starting at header[pos]; mirror
 * parse_usize_decimal_header. returns 0 ok (*out set), -1 bad. */
static int parse_cl_decimal(const unsigned char *h, size_t hlen, size_t pos, size_t *out) {
    size_t value = 0;
    int found = 0;
    while (pos < hlen) {
        unsigned char b = h[pos];
        if (b >= '0' && b <= '9') {
            found = 1;
            value = value * 10 + (size_t)(b - '0');
        } else if (b == '\r' || b == '\n') {
            break;
        } else if ((b == ' ' || b == '\t') && !found) {
            /* skip leading ws */
        } else {
            return -1;
        }
        pos++;
    }
    if (!found) return -1;
    *out = value;
    return 0;
}

/* find Content-Length value start (after the field name + ws). returns -1 if absent. */
static long find_content_length_value(const unsigned char *h, size_t hlen) {
    const void *p = mem_find(h, hlen, "\r\nContent-Length:", 17);
    if (!p) p = mem_find(h, hlen, "\r\ncontent-length:", 17);
    if (!p) return -1;
    size_t pos = (size_t)((const unsigned char *)p - h) + 17;
    while (pos < hlen && (h[pos] == ' ' || h[pos] == '\t')) pos++;
    return (long)pos;
}

/* head parse result */
typedef struct {
    int route;
    size_t content_length;
    int close_after;
} Head;

/* returns 0 ok, -1 error (*err set to response). */
static int parse_handoff_head(const unsigned char *h, size_t hlen,
                              Head *out, const char **err) {
    static const char P11[] = "POST /fraud-score HTTP/1.1\r\n";
    static const char P10[] = "POST /fraud-score HTTP/1.0\r\n";
    static const char G11[] = "GET /ready HTTP/1.1\r\n";
    static const char G10[] = "GET /ready HTTP/1.0\r\n";

    int route, http10;
    if (hlen >= 27 && memcmp(h, P11, 27) == 0) { route = ROUTE_FRAUD; http10 = 0; }
    else if (hlen >= 27 && memcmp(h, P10, 27) == 0) { route = ROUTE_FRAUD; http10 = 1; }
    else if (hlen >= 20 && memcmp(h, G11, 20) == 0) { route = ROUTE_READY; http10 = 0; }
    else if (hlen >= 20 && memcmp(h, G10, 20) == 0) { route = ROUTE_READY; http10 = 1; }
    else {
        /* Generic fallback: faithful to parse_handoff_head_generic. */
        /* request line up to first \r\n */
        const void *eol = mem_find(h, hlen, "\r\n", 2);
        size_t line_len = eol ? (size_t)((const unsigned char *)eol - h) : hlen;
        /* split request line into method/path/version on spaces */
        const unsigned char *L = h;
        size_t i = 0, n = line_len;
        /* method */
        while (i < n && L[i] != ' ') i++;
        size_t m0 = 0, m1 = i;
        while (i < n && L[i] == ' ') i++;
        size_t p0 = i;
        while (i < n && L[i] != ' ') i++;
        size_t p1 = i;
        while (i < n && L[i] == ' ') i++;
        size_t v0 = i;
        while (i < n && L[i] != ' ') i++;
        size_t v1 = i;
        size_t mlen = m1 - m0, plen = p1 - p0, vlen = v1 - v0;
        int is_v11 = (vlen == 8 && memcmp(L + v0, "HTTP/1.1", 8) == 0);
        int is_v10 = (vlen == 8 && memcmp(L + v0, "HTTP/1.0", 8) == 0);
        if (!is_v11 && !is_v10) { *err = R_BAD_REQUEST; return -1; }
        int is_get = (mlen == 3 && memcmp(L + m0, "GET", 3) == 0);
        int is_post = (mlen == 4 && memcmp(L + m0, "POST", 4) == 0);
        int is_ready = (plen == 6 && memcmp(L + p0, "/ready", 6) == 0);
        int is_fraud = (plen == 12 && memcmp(L + p0, "/fraud-score", 12) == 0);
        if (is_get && is_ready) route = ROUTE_READY;
        else if (is_post && is_fraud) route = ROUTE_FRAUD;
        else if (is_get || is_post) { *err = R_NOT_FOUND; return -1; }
        else { *err = R_METHOD_NOT_ALLOWED; return -1; }

        int close_after = is_v10;
        long have_cl = -1;
        size_t cl = 0;
        int chunked = 0;
        /* iterate header lines */
        size_t off = line_len + 2;
        while (off < hlen) {
            const void *e2 = mem_find(h + off, hlen - off, "\r\n", 2);
            size_t llen = e2 ? (size_t)((const unsigned char *)e2 - (h + off)) : (hlen - off);
            if (llen == 0) break;
            const unsigned char *ln = h + off;
            const void *colon = memchr(ln, ':', llen);
            if (!colon) { *err = R_BAD_REQUEST; return -1; }
            size_t nlen = (size_t)((const unsigned char *)colon - ln);
            const unsigned char *val = (const unsigned char *)colon + 1;
            size_t vl = llen - nlen - 1;
            /* trim value ws */
            while (vl > 0 && (val[0] == ' ' || val[0] == '\t')) { val++; vl--; }
            while (vl > 0 && (val[vl - 1] == ' ' || val[vl - 1] == '\t' ||
                              val[vl - 1] == '\r')) vl--;
            if (nlen == 14 && strncasecmp((const char *)ln, "content-length", 14) == 0) {
                size_t parsed = 0; int ok = 1;
                if (vl == 0) ok = 0;
                for (size_t k = 0; k < vl; k++) {
                    if (val[k] < '0' || val[k] > '9') { ok = 0; break; }
                    parsed = parsed * 10 + (size_t)(val[k] - '0');
                }
                if (!ok) { *err = R_BAD_REQUEST; return -1; }
                have_cl = 1; cl = parsed;
            } else if (nlen == 10 && strncasecmp((const char *)ln, "connection", 10) == 0) {
                if (vl == 5 && strncasecmp((const char *)val, "close", 5) == 0) close_after = 1;
            } else if (nlen == 17 && strncasecmp((const char *)ln, "transfer-encoding", 17) == 0) {
                /* contains "chunked" (case-insensitive) */
                for (size_t k = 0; k + 7 <= vl; k++) {
                    if (strncasecmp((const char *)val + k, "chunked", 7) == 0) { chunked = 1; break; }
                }
            }
            off += llen + 2;
        }
        if (chunked) { *err = R_BAD_REQUEST; return -1; }
        if (have_cl < 0) cl = 0;
        if (route == ROUTE_FRAUD && cl == 0) { *err = R_BAD_REQUEST; return -1; }
        if (cl > RAW_MAX_BODY_BYTES) { *err = R_PAYLOAD_TOO_LARGE; return -1; }
        out->route = route;
        out->content_length = cl;
        out->close_after = close_after;
        return 0;
    }

    /* fast path */
    if (mem_find(h, hlen, "\r\nTransfer-Encoding:", 20) ||
        mem_find(h, hlen, "\r\ntransfer-encoding:", 20)) {
        *err = R_BAD_REQUEST;
        return -1;
    }
    size_t cl = 0;
    if (route == ROUTE_FRAUD) {
        long vstart = find_content_length_value(h, hlen);
        if (vstart < 0) { *err = R_BAD_REQUEST; return -1; }
        if (parse_cl_decimal(h, hlen, (size_t)vstart, &cl) != 0) {
            *err = R_BAD_REQUEST;
            return -1;
        }
    }
    if (route == ROUTE_FRAUD && cl == 0) { *err = R_BAD_REQUEST; return -1; }
    if (cl > RAW_MAX_BODY_BYTES) { *err = R_PAYLOAD_TOO_LARGE; return -1; }

    int close_after = http10 ||
        (mem_find(h, hlen, "\r\nConnection: close", 19) != NULL) ||
        (mem_find(h, hlen, "\r\nconnection: close", 19) != NULL);

    out->route = route;
    out->content_length = cl;
    out->close_after = close_after;
    return 0;
}

/* Parse one request from the buffer.
 * returns: 1 complete (fills *route,*header_len,*total_len,*close); 0 need more;
 *          -1 error (*err set to response). */
static int parse_request(const unsigned char *buf, size_t len, int *route,
                         size_t *header_len, size_t *total_len, int *close,
                         const char **err) {
    long he = find_header_end(buf, len);
    if (he < 0) {
        if (len > HANDOFF_MAX_HEADER_BYTES) { *err = R_PAYLOAD_TOO_LARGE; return -1; }
        return 0;
    }
    size_t hlen = (size_t)he + 4;
    if (hlen > HANDOFF_MAX_HEADER_BYTES) { *err = R_PAYLOAD_TOO_LARGE; return -1; }
    Head head;
    if (parse_handoff_head(buf, hlen, &head, err) != 0) return -1;
    size_t total = hlen + head.content_length;
    if (total > HANDOFF_MAX_HEADER_BYTES + RAW_MAX_BODY_BYTES) {
        *err = R_PAYLOAD_TOO_LARGE;
        return -1;
    }
    if (len < total) return 0;
    *route = head.route;
    *header_len = hlen;
    *total_len = total;
    *close = head.close_after;
    return 1;
}

/* Compute the response for a complete request. */
static const char *process_request(int route, const unsigned char *body, size_t body_len) {
    if (route == ROUTE_READY) return R_READY_OK;
    uint8_t bucket = rinha_score_body(body, body_len);
    if (bucket == 255) return R_BAD_REQUEST;
    if (bucket > 5) bucket = 5;
    return SCORE_RESP[bucket];
}

static void conn_compact(Conn *c, size_t consumed) {
    if (consumed >= c->len) {
        c->len = 0;
    } else {
        memmove(c->buf, c->buf + consumed, c->len - consumed);
        c->len -= consumed;
    }
}

/* Outcome: 0 keep, 1 close. */
enum { OUT_KEEP = 0, OUT_CLOSE = 1 };

/* Drain & respond to all buffered requests; returns OUT_* or -1 (need more,
 * keep). returns: 0 keep(need read), 1 close. */
static int process_buffered(int epfd, int fd, Conn *c) {
    for (;;) {
        if (c->len == 0) return OUT_KEEP;
        int route, close_after;
        size_t header_len, total_len;
        const char *err = NULL;
        int r = parse_request(c->buf, c->len, &route, &header_len, &total_len,
                              &close_after, &err);
        if (r == 0) {
            /* need more; if buffer full with no complete request -> 413 + close */
            if (c->len == HANDOFF_BUFFER_BYTES) {
                size_t off = 0;
                try_write(fd, R_PAYLOAD_TOO_LARGE, strlen(R_PAYLOAD_TOO_LARGE), &off);
                return OUT_CLOSE;
            }
            return OUT_KEEP;
        }
        if (r < 0) {
            size_t off = 0;
            try_write(fd, err, strlen(err), &off);
            return OUT_CLOSE;
        }
        const char *resp = process_request(route, c->buf + header_len,
                                           total_len - header_len);
        size_t resp_total = strlen(resp);
        size_t off = 0;
        int ws = try_write(fd, resp, resp_total, &off);
        if (ws == 0) {
            conn_compact(c, total_len);
            if (close_after) return OUT_CLOSE;
            /* continue draining pipelined requests */
        } else if (ws == 1) {
            conn_compact(c, total_len);
            c->pending = resp;
            c->pending_off = off;
            c->pending_total = resp_total;
            c->close_after = close_after;
            if (!c->want_out) {
                if (epoll_mod(epfd, fd, EPOLLOUT) < 0) return OUT_CLOSE;
                c->want_out = 1;
            }
            return OUT_KEEP;
        } else {
            return OUT_CLOSE;
        }
    }
}

/* Drive one connection on an epoll event. returns OUT_KEEP / OUT_CLOSE. */
static int drive(int epfd, int fd, Conn *c) {
    /* 1. flush pending write first */
    if (c->pending) {
        size_t off = c->pending_off;
        int ws = try_write(fd, c->pending, c->pending_total, &off);
        c->pending_off = off;
        if (ws == 1) return OUT_KEEP;
        if (ws < 0) return OUT_CLOSE;
        /* done */
        c->pending = NULL;
        if (c->close_after) return OUT_CLOSE;
        if (c->want_out) {
            if (epoll_mod(epfd, fd, EPOLLIN) < 0) return OUT_CLOSE;
            c->want_out = 0;
        }
    }
    /* 2. process buffered, then read more, repeat until EAGAIN.
     *
     * API_SINGLE_RECV optimisation: in closed-loop keepalive the client cannot
     * send request N+1 until it receives response N, so after sending a response
     * the socket is always empty — the trailing read() would always return EAGAIN,
     * wasting a syscall.  When g_single_recv is set we skip that trailing read:
     * after a successful read that returned fewer bytes than the space offered
     * (socket is drained) we return OUT_KEEP and rely on level-triggered epoll
     * + NAPI busy-poll to re-fire EPOLLIN when the next request arrives.
     *
     * Invariant (critical): did_read and last_read_partial are LOCAL to this
     * drive() call, set only after a successful read() in this invocation.  We
     * never consult a stale per-Conn flag.  A fresh drive() always performs the
     * mandatory first read (did_read starts 0). */
    int did_read = 0;
    int last_read_partial = 0;
    for (;;) {
        int outcome = process_buffered(epfd, fd, c);
        if (outcome == OUT_CLOSE) return OUT_CLOSE;
        /* OUT_KEEP from process_buffered means "buffer drained / waiting"; read more */
        if (c->pending) return OUT_KEEP; /* a write went pending; wait on EPOLLOUT */
        /* Skip trailing EAGAIN read when socket is already known-drained. */
        if (g_single_recv && did_read && last_read_partial) return OUT_KEEP;
        size_t cap = HANDOFF_BUFFER_BYTES;
        if (c->len >= cap) return OUT_CLOSE;
        size_t space = cap - c->len;
        ssize_t n = read(fd, c->buf + c->len, space);
        if (n == 0) return OUT_CLOSE;
        if (n > 0) {
            c->len += (size_t)n;
            did_read = 1;
            last_read_partial = ((size_t)n < space);
            if (g_rearm_quickack) set_quickack(fd);
            continue;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) return OUT_KEEP;
        if (errno == EINTR) continue;
        return OUT_CLOSE;
    }
}

static void close_client(int epfd, int fd) {
    epoll_del(epfd, fd);
    Conn *c = g_conns[fd];
    if (c) {
        conn_release(c);
        g_conns[fd] = NULL;
    }
    close(fd);
}

static int register_client(int epfd, int client_fd) {
    if (client_fd < 0 || client_fd >= MAX_FDS) return -1;
    if (set_nonblocking(client_fd) < 0) return -1;
    tune_client_fd(client_fd);
    Conn *c = conn_alloc();
    if (!c) return -1;
    if (epoll_add(epfd, client_fd, EPOLLIN) < 0) {
        conn_release(c);
        return -1;
    }
    g_conns[client_fd] = c;
    return 0;
}

static void accept_control(int epfd, int listener_fd) {
    for (;;) {
        int cfd = accept4(listener_fd, NULL, NULL, SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (cfd < 0) {
            return; /* EAGAIN / EINTR / other -> stop accepting this round */
        }
        if (cfd >= MAX_FDS) { close(cfd); continue; }
        if (epoll_add(epfd, cfd, EPOLLIN) < 0) { close(cfd); continue; }
        g_is_control[cfd] = 1;
    }
}

static void drain_control(int epfd, int control_fd) {
    for (;;) {
        int r = recv_fd_nonblocking(control_fd);
        if (r >= 0) {
            if (register_client(epfd, r) != 0) close(r);
        } else if (r == -1) {
            return; /* EAGAIN */
        } else {
            /* peer closed (-2) or error (-3): drop the control connection */
            epoll_del(epfd, control_fd);
            g_is_control[control_fd] = 0;
            close(control_fd);
            return;
        }
    }
}

/* ---- /ready TCP server thread (mirror run_minimal_ready_server) ---- */
static void *ready_server_thread(void *arg) {
    int port = (int)(intptr_t)arg;
    int lfd = socket(AF_INET, SOCK_STREAM, 0);
    if (lfd < 0) { perror("ready socket"); return NULL; }
    int one = 1;
    setsockopt(lfd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons((uint16_t)port);
    if (bind(lfd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("ready bind");
        close(lfd);
        return NULL;
    }
    if (listen(lfd, 128) < 0) { perror("ready listen"); close(lfd); return NULL; }
    fprintf(stderr, "Minimal ready server listening on port %d\n", port);

    char buf[512];
    for (;;) {
        int cfd = accept(lfd, NULL, NULL);
        if (cfd < 0) {
            if (errno == EINTR) continue;
            continue;
        }
        ssize_t n = read(cfd, buf, sizeof(buf));
        const char *resp;
        if (n > 0 && ((n >= 11 && memcmp(buf, "GET /ready ", 11) == 0) ||
                      (n >= 12 && memcmp(buf, "HEAD /ready ", 12) == 0))) {
            resp = atomic_load(&g_ready) ? R_READY_OK : R_SERVICE_UNAVAILABLE;
        } else if (n > 0 && (memcmp(buf, "GET ", 4) == 0 || memcmp(buf, "HEAD ", 5) == 0)) {
            resp = R_NOT_FOUND;
        } else {
            resp = R_METHOD_NOT_ALLOWED;
        }
        size_t off = 0, total = strlen(resp);
        while (off < total) {
            ssize_t w = write(cfd, resp + off, total - off);
            if (w <= 0) break;
            off += (size_t)w;
        }
        close(cfd);
    }
    return NULL;
}

/* ---- classifier warm-up (mirror warm_classifier intent) ---- */
static void warm_classifier(uint32_t iters) {
    static const char SAMPLE[] =
        "{\"transaction\":{\"amount\":41.12,\"installments\":2,\"requested_at\":"
        "\"2026-03-11T18:45:53Z\"},\"customer\":{\"avg_amount\":82.24,"
        "\"tx_count_24h\":3,\"known_merchants\":[\"MERC-003\",\"MERC-016\"]},"
        "\"merchant\":{\"id\":\"MERC-016\",\"mcc\":\"5411\",\"avg_amount\":60.25},"
        "\"terminal\":{\"is_online\":false,\"card_present\":true,"
        "\"km_from_home\":29.23},\"last_transaction\":null}";
    volatile uint8_t sink = 0;
    for (uint32_t i = 0; i < iters; i++) {
        sink ^= rinha_score_body((const uint8_t *)SAMPLE, sizeof(SAMPLE) - 1);
    }
    (void)sink;
}

/* ---- 3-tier epoll idle strategy ----------------------------------------
 * Phase 1+2: userspace spin — only when g_epoll_spin_us > 0. Calls epoll_wait(0)
 *            repeatedly with PAUSE for up to g_epoll_spin_us µs.
 *            WARNING: with EPIOCSPARAMS busy_poll_usecs > 0, each epoll_wait(0)
 *            may trigger a full NAPI busy-poll cycle, burning g_busy_poll_us µs
 *            per call. Only enable spin when EPIOCSPARAMS is disabled (us=0).
 * Phase 3: epoll_pwait2   — nanosecond-precision block for g_epoll_idle_us µs,
 *                           fallback to epoll_wait(fallback_ms) on ENOSYS.
 * When g_epoll_spin_us==0 and g_epoll_idle_us==0, equivalent to the old
 * epoll_wait(fallback_ms) — zero behaviour change by default. */
static int epoll_wait_tiered(int epfd, struct epoll_event *evs, int maxev,
                              int fallback_ms) {
    int n;

    /* Phase 1+2: userspace spin (only when explicitly enabled AND NAPI off).
     * Skipped by default (g_epoll_spin_us=0). */
    if (g_epoll_spin_us > 0) {
        /* non-blocking probe first */
        n = epoll_wait(epfd, evs, maxev, 0);
        if (n != 0) return n;

        struct timespec t0, t1;
        clock_gettime(CLOCK_MONOTONIC, &t0);
        long end_ns = (long)t0.tv_sec * 1000000000L + t0.tv_nsec
                      + (long)g_epoll_spin_us * 1000L;
        for (;;) {
            n = epoll_wait(epfd, evs, maxev, 0);
            if (n != 0) return n;
            __asm__ volatile("pause" ::: "memory");
            clock_gettime(CLOCK_MONOTONIC, &t1);
            if ((long)t1.tv_sec * 1000000000L + t1.tv_nsec >= end_ns) break;
        }
    }

    /* Phase 3: nanosecond-precision block — replaces epoll_wait(1ms) with a
     * tight g_epoll_idle_us µs timeout. The NAPI busy-poll (EPIOCSPARAMS) fires
     * once inside this blocking call before sleeping, which is the intended path:
     * one NAPI poll per request gap, not one per spin iteration. */
    if (g_epoll_idle_us > 0 && !g_pwait2_enosys) {
        struct timespec ts = { .tv_sec = 0,
                               .tv_nsec = (long)g_epoll_idle_us * 1000L };
        n = (int)syscall(SYS_epoll_pwait2, epfd, evs, maxev, &ts, NULL,
                         (size_t)0);
        if (n < 0 && errno == ENOSYS) {
            g_pwait2_enosys = 1;  /* kernel too old; fall through */
        } else {
            return n;
        }
    }

    return epoll_wait(epfd, evs, maxev, fallback_ms);
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);

    const char *fd_listen = getenv("API_FD_LISTEN");
    if (!fd_listen || !*fd_listen) {
        fprintf(stderr, "API_FD_LISTEN not set; the C reactor only serves the FD-handoff path\n");
        return 1;
    }
    /* strip optional unix: prefix */
    const char *path = fd_listen;
    if (strncmp(path, "unix:", 5) == 0) path += 5;

    int port = (int)env_u32("PORT", 8080);
    g_rearm_quickack   = env_u32("API_QUICKACK_REARM",   0) != 0;
    g_single_recv      = env_u32("API_SINGLE_RECV",      0) != 0;
    g_busy_poll_us     = env_u32("API_BUSY_POLL_US",     50);
    g_busy_poll_budget = env_u32("API_BUSY_POLL_BUDGET",  8);
    g_prefer_busy_poll = env_u32("API_PREFER_BUSY_POLL",  1);
    g_epoll_spin_us    = env_u32("API_EPOLL_SPIN_US",     0);
    g_epoll_idle_us    = env_u32("API_EPOLL_IDLE_US",     0);
    uint32_t warm_iters = env_u32("API_WARM_ITERS", 50000);

    /* SO_INCOMING_CPU (default OFF -> banked behavior unchanged). API_INCOMING_CPU=
     * "pin" aligns RX steering to this instance's API_PIN_CPU; an integer sets it
     * explicitly. */
    {
        const char *ic = getenv("API_INCOMING_CPU");
        if (ic && *ic) {
            if (strcmp(ic, "pin") == 0) {
                const char *p = getenv("API_PIN_CPU");
                if (p && *p) g_incoming_cpu = atoi(p);
            } else {
                g_incoming_cpu = atoi(ic);
            }
        }
        fprintf(stderr, "SO_INCOMING_CPU: %s (%d)\n",
                g_incoming_cpu >= 0 ? "on" : "off (default)", g_incoming_cpu);
    }

    maybe_pin_cpu();

    /* mlockall: pin all current pages into RAM, killing page-fault tail.
     * Best-effort — silently fails inside non-privileged containers. */
    if (mlockall(MCL_CURRENT) == 0)
        fprintf(stderr, "mlockall(MCL_CURRENT): OK\n");
    else
        fprintf(stderr, "mlockall(MCL_CURRENT): %s (non-fatal)\n", strerror(errno));

    /* PR_SET_TIMERSLACK=1ns: tighten the wake-jitter ceiling from the default
     * 50µs.  No caps needed; always returns 0 on supported kernels. */
    if (prctl(PR_SET_TIMERSLACK, 1, 0, 0, 0) == 0)
        fprintf(stderr, "timerslack: set to 1 ns\n");
    else
        fprintf(stderr, "timerslack: prctl(%d) = %s (non-fatal)\n",
                PR_SET_TIMERSLACK, strerror(errno));

    fprintf(stderr, "single-recv optimisation: %s\n",
            g_single_recv ? "on (API_SINGLE_RECV=1)" : "off (default)");
    fprintf(stderr, "per-request QUICKACK re-arm: %s\n",
            g_rearm_quickack ? "on" : "off (default)");
    fprintf(stderr, "3-tier epoll idle: spin=%uus idle=%uus%s\n",
            g_epoll_spin_us, g_epoll_idle_us,
            (g_epoll_spin_us == 0 && g_epoll_idle_us == 0) ? " (off, plain epoll_wait)" : "");

    /* /ready healthcheck server on a thread */
    pthread_t ready_tid;
    if (pthread_create(&ready_tid, NULL, ready_server_thread,
                       (void *)(intptr_t)port) != 0) {
        fprintf(stderr, "failed to spawn ready server: %s\n", strerror(errno));
        return 1;
    }
    pthread_detach(ready_tid);

    if (warm_iters > 0) {
        fprintf(stderr, "Warming classifier hot path (%u iters)...\n", warm_iters);
        warm_classifier(warm_iters);
    }
    atomic_store(&g_ready, 1);
    fprintf(stderr, "C reactor (classifier-only handoff) ready on port %d\n", port);

    /* bind the Unix control socket */
    unlink(path);
    int listener_fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (listener_fd < 0) { perror("socket"); return 1; }
    struct sockaddr_un un;
    memset(&un, 0, sizeof(un));
    un.sun_family = AF_UNIX;
    strncpy(un.sun_path, path, sizeof(un.sun_path) - 1);
    if (bind(listener_fd, (struct sockaddr *)&un, sizeof(un)) < 0) {
        perror("bind");
        return 1;
    }
    if (listen(listener_fd, 128) < 0) { perror("listen"); return 1; }
    set_nonblocking(listener_fd);
    fprintf(stderr, "FD handoff server (C epoll) listening on %s\n", path);

    int epfd = epoll_create1(EPOLL_CLOEXEC);
    if (epfd < 0) { perror("epoll_create1"); return 1; }
    configure_busy_poll(epfd);
    if (epoll_add(epfd, listener_fd, EPOLLIN) < 0) { perror("epoll_add listener"); return 1; }

    int timeout_ms = (int)env_u32("API_EPOLL_TIMEOUT_MS", 1);
    struct epoll_event events[MAX_EVENTS];

    for (;;) {
        int n = epoll_wait_tiered(epfd, events, MAX_EVENTS, timeout_ms);
        if (n < 0) {
            if (errno == EINTR) continue;
            perror("epoll_wait");
            return 1;
        }
        for (int i = 0; i < n; i++) {
            int fd = events[i].data.fd;
            if (fd == listener_fd) {
                accept_control(epfd, listener_fd);
                continue;
            }
            if (g_is_control[fd]) {
                drain_control(epfd, fd);
                continue;
            }
            Conn *c = g_conns[fd];
            if (c) {
                if (drive(epfd, fd, c) == OUT_CLOSE) {
                    close_client(epfd, fd);
                }
            } else {
                epoll_del(epfd, fd);
            }
        }
    }
    return 0;
}

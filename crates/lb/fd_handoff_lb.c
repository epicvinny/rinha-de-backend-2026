#define _GNU_SOURCE

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define MAX_UPSTREAMS 8
#define BACKLOG 65535

typedef struct {
    char path[108];
    int fd;
} upstream_t;

static upstream_t upstreams[MAX_UPSTREAMS];
static int upstream_count = 0;
static uint32_t rr_next = 0;
static int startup_health_mode = 1;

static const char ready_response[] =
    "HTTP/1.1 200 OK\r\n"
    "content-type: application/json\r\n"
    "content-length: 15\r\n"
    "connection: close\r\n"
    "\r\n"
    "{\"ready\":true}\n";

static void sleep_ms(long ms) {
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (ms % 1000) * 1000000L;
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {}
}

static void add_upstream(const char* begin, size_t len) {
    while (len > 0 && (*begin == ' ' || *begin == '\t')) {
        ++begin;
        --len;
    }
    while (len > 0 && (begin[len - 1] == ' ' || begin[len - 1] == '\t' || begin[len - 1] == '\n')) {
        --len;
    }
    if (len >= 5 && memcmp(begin, "unix:", 5) == 0) {
        begin += 5;
        len -= 5;
    }
    if (len == 0 || upstream_count >= MAX_UPSTREAMS) return;

    upstream_t* u = &upstreams[upstream_count++];
    memset(u, 0, sizeof(*u));
    if (len >= sizeof(u->path)) _Exit(1);
    memcpy(u->path, begin, len);
    u->path[len] = '\0';
    u->fd = -1;
}

static void parse_upstreams(void) {
    const char* env = getenv("BACKENDS");
    if (env == NULL || env[0] == '\0') env = getenv("UPSTREAMS");
    if (env == NULL || env[0] == '\0') env = "/tmp/rinha-api1.sock,/tmp/rinha-api2.sock";

    const char* start = env;
    for (const char* p = env;; ++p) {
        if (*p == ',' || *p == '\0') {
            add_upstream(start, (size_t)(p - start));
            if (*p == '\0') break;
            start = p + 1;
        }
    }
    if (upstream_count == 0) _Exit(1);
}

static int connect_once(const char* path) {
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;

    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    if (connect(fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int connect_wait(const char* path) {
    for (;;) {
        int fd = connect_once(path);
        if (fd >= 0) return fd;
        sleep_ms(5);
    }
}

static void connect_all(void) {
    for (int i = 0; i < upstream_count; ++i) upstreams[i].fd = connect_wait(upstreams[i].path);
}

static int reconnect_one(int idx) {
    if (upstreams[idx].fd >= 0) close(upstreams[idx].fd);
    upstreams[idx].fd = -1;
    for (int tries = 0; tries < 20; ++tries) {
        int fd = connect_once(upstreams[idx].path);
        if (fd >= 0) {
            upstreams[idx].fd = fd;
            return 0;
        }
        sleep_ms(2);
    }
    return -1;
}

static int send_fd_once(int ctrl_fd, int client_fd) {
    char byte = 1;
    struct iovec iov = { .iov_base = &byte, .iov_len = 1 };
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    memset(&control, 0, sizeof(control));

    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.buf;
    msg.msg_controllen = sizeof(control.buf);

    struct cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
    cmsg->cmsg_level = SOL_SOCKET;
    cmsg->cmsg_type = SCM_RIGHTS;
    cmsg->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(cmsg), &client_fd, sizeof(client_fd));

    for (;;) {
        ssize_t n = sendmsg(ctrl_fd, &msg, MSG_NOSIGNAL);
        if (n == 1) return 0;
        if (n < 0 && errno == EINTR) continue;
        return -1;
    }
}

static int has_header_end(const char* buf, size_t len) {
    if (len < 4) return 0;
    for (size_t i = 3; i < len; ++i) {
        if (buf[i - 3] == '\r' && buf[i - 2] == '\n' && buf[i - 1] == '\r' && buf[i] == '\n') return 1;
    }
    return 0;
}

static int send_all(int fd, const char* buf, size_t len) {
    size_t sent = 0;
    while (sent < len) {
        ssize_t n = send(fd, buf + sent, len - sent, MSG_NOSIGNAL);
        if (n > 0) {
            sent += (size_t)n;
            continue;
        }
        if (n < 0 && errno == EINTR) continue;
        return -1;
    }
    return 0;
}

static int consume_health_request(int client_fd, char* buf, size_t* len) {
    while (*len < 2048 && !has_header_end(buf, *len)) {
        struct pollfd pfd = { .fd = client_fd, .events = POLLIN, .revents = 0 };
        int pr;
        do {
            pr = poll(&pfd, 1, 1000);
        } while (pr < 0 && errno == EINTR);

        if (pr <= 0 || !(pfd.revents & POLLIN)) break;

        ssize_t n = recv(client_fd, buf + *len, 2048 - *len, 0);
        if (n > 0) {
            *len += (size_t)n;
            continue;
        }
        if (n == 0) return 0;
        if (errno == EINTR) continue;
        if (errno == EAGAIN || errno == EWOULDBLOCK) continue;
        return -1;
    }
    return has_header_end(buf, *len) ? 0 : -1;
}

static int maybe_handle_startup_health(int client_fd) {
    if (!startup_health_mode) return 0;

    struct pollfd pfd = { .fd = client_fd, .events = POLLIN, .revents = 0 };
    int pr;
    do {
        pr = poll(&pfd, 1, 1000);
    } while (pr < 0 && errno == EINTR);

    if (pr <= 0 || !(pfd.revents & POLLIN)) return 0;

    char buf[2048];
    ssize_t n = recv(client_fd, buf, sizeof(buf), MSG_PEEK | MSG_DONTWAIT);
    if (n <= 0) return 0;

    if ((n >= 10 && memcmp(buf, "GET /ready", 10) == 0) ||
        (n >= 11 && memcmp(buf, "HEAD /ready", 11) == 0)) {
        size_t len = 0;
        if (consume_health_request(client_fd, buf, &len) == 0) {
            (void)send_all(client_fd, ready_response, sizeof(ready_response) - 1);
        }
        startup_health_mode = 0;
        return 1;
    }

    if (n >= 5 && memcmp(buf, "POST ", 5) == 0) {
        startup_health_mode = 0;
    }
    return 0;
}

static int handoff(int idx, int client_fd) {
    if (upstreams[idx].fd < 0 && reconnect_one(idx) != 0) return -1;
    if (send_fd_once(upstreams[idx].fd, client_fd) == 0) return 0;
    if (reconnect_one(idx) != 0) return -1;
    return send_fd_once(upstreams[idx].fd, client_fd);
}

static void tune_client_socket(int fd) {
    int one = 1;
    (void)setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
}

static int listen_tcp(int port) {
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;

    int one = 1;
    (void)setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
    (void)setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &one, sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_ANY);
    addr.sin_port = htons((uint16_t)port);
    if (bind(fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) return -1;
    if (listen(fd, BACKLOG) != 0) return -1;
    return fd;
}

static int listen_port(void) {
    const char* listen = getenv("LB_LISTEN");
    if (listen != NULL && listen[0] != '\0') {
        const char* colon = strrchr(listen, ':');
        if (colon != NULL && colon[1] != '\0') return atoi(colon + 1);
        return atoi(listen);
    }
    const char* port = getenv("PORT");
    if (port != NULL && port[0] != '\0') return atoi(port);
    return 9999;
}

int main(void) {
    signal(SIGPIPE, SIG_IGN);
    parse_upstreams();
    connect_all();

    int server_fd = listen_tcp(listen_port());
    if (server_fd < 0) {
        perror("listen");
        return 1;
    }

    for (;;) {
        int client_fd = accept4(server_fd, NULL, NULL, SOCK_CLOEXEC);
        if (client_fd < 0) {
            if (errno == EINTR) continue;
            continue;
        }
        tune_client_socket(client_fd);

        if (maybe_handle_startup_health(client_fd)) {
            close(client_fd);
            continue;
        }
        int first = (int)(rr_next++ % (uint32_t)upstream_count);
        if (handoff(first, client_fd) != 0) {
            for (int offset = 1; offset < upstream_count; ++offset) {
                if (handoff((first + offset) % upstream_count, client_fd) == 0) break;
            }
        }
        close(client_fd);
    }
}

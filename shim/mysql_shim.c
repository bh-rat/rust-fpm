/*
 * MySQL socket interception shim for rust-fpm.
 *
 * Loaded via LD_PRELOAD, intercepts libc socket functions to identify and
 * measure MySQL I/O from PHP's mysqlnd driver. mysqlnd uses sendto/recvfrom
 * with MSG_DONTWAIT on TCP connections to port 3306.
 *
 * Build: gcc -shared -fPIC -o libmysql_shim.so mysql_shim.c -ldl
 * Usage: LD_PRELOAD=./libmysql_shim.so rust-fpm ...
 */

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <stdlib.h>
#include <unistd.h>

/* Max fd we track. Linux default ulimit is 1024. */
#define MAX_FDS 4096

/* Per-fd tracking */
static int mysql_fds[MAX_FDS];

/* Per-request stats (reset on each new MySQL connection) */
static __thread int query_count;
static __thread double total_send_ms;
static __thread double total_recv_ms;
static __thread double total_poll_ms;

/* Configurable MySQL port (default 3306) */
static int mysql_port = 3306;

/* Real libc function pointers */
static int    (*real_connect)(int, const struct sockaddr *, socklen_t);
static ssize_t (*real_sendto)(int, const void *, size_t, int,
                              const struct sockaddr *, socklen_t);
static ssize_t (*real_recvfrom)(int, void *, size_t, int,
                                struct sockaddr *, socklen_t *);
static ssize_t (*real_send)(int, const void *, size_t, int);
static ssize_t (*real_recv)(int, void *, size_t, int);
static ssize_t (*real_read)(int, void *, size_t);
static ssize_t (*real_write)(int, const void *, size_t);
static int    (*real_poll)(struct pollfd *, nfds_t, int);
static int    (*real_close)(int);

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

__attribute__((constructor))
static void shim_init(void) {
    real_connect  = dlsym(RTLD_NEXT, "connect");
    real_sendto   = dlsym(RTLD_NEXT, "sendto");
    real_recvfrom = dlsym(RTLD_NEXT, "recvfrom");
    real_send     = dlsym(RTLD_NEXT, "send");
    real_recv     = dlsym(RTLD_NEXT, "recv");
    real_read     = dlsym(RTLD_NEXT, "read");
    real_write    = dlsym(RTLD_NEXT, "write");
    real_poll     = dlsym(RTLD_NEXT, "poll");
    real_close    = dlsym(RTLD_NEXT, "close");
    memset(mysql_fds, 0, sizeof(mysql_fds));

    const char *port_env = getenv("MYSQL_SHIM_PORT");
    if (port_env) mysql_port = atoi(port_env);
}

/* --- connect: identify MySQL fds --- */

int connect(int fd, const struct sockaddr *addr, socklen_t len) {
    int result = real_connect(fd, addr, len);

    if ((result == 0 || errno == EINPROGRESS) && fd >= 0 && fd < MAX_FDS) {
        if (addr->sa_family == AF_INET) {
            const struct sockaddr_in *in = (const struct sockaddr_in *)addr;
            if (ntohs(in->sin_port) == mysql_port) {
                mysql_fds[fd] = 1;
                query_count = 0;
                total_send_ms = 0;
                total_recv_ms = 0;
                total_poll_ms = 0;
                fprintf(stderr, "[mysql_shim] MySQL fd=%d connected (port %d)\n",
                        fd, mysql_port);
            }
        } else if (addr->sa_family == AF_UNIX) {
            const struct sockaddr_un *un = (const struct sockaddr_un *)addr;
            if (strstr(un->sun_path, "mysql") || strstr(un->sun_path, "mariadb")) {
                mysql_fds[fd] = 1;
                query_count = 0;
                total_send_ms = 0;
                total_recv_ms = 0;
                total_poll_ms = 0;
                fprintf(stderr, "[mysql_shim] MySQL fd=%d connected (socket %s)\n",
                        fd, un->sun_path);
            }
        }
    }
    return result;
}

/* --- sendto: intercept MySQL query sends --- */

ssize_t sendto(int fd, const void *buf, size_t len, int flags,
               const struct sockaddr *dest, socklen_t addrlen) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_sendto(fd, buf, len, flags, dest, addrlen);
        double dt = now_ms() - t0;
        total_send_ms += dt;
        query_count++;
        return r;
    }
    return real_sendto(fd, buf, len, flags, dest, addrlen);
}

/* --- recvfrom: intercept MySQL response reads --- */

ssize_t recvfrom(int fd, void *buf, size_t len, int flags,
                 struct sockaddr *src, socklen_t *addrlen) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_recvfrom(fd, buf, len, flags, src, addrlen);
        double dt = now_ms() - t0;
        if (r > 0) total_recv_ms += dt;
        return r;
    }
    return real_recvfrom(fd, buf, len, flags, src, addrlen);
}

/* --- send/recv: alternate paths mysqlnd might use --- */

ssize_t send(int fd, const void *buf, size_t len, int flags) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_send(fd, buf, len, flags);
        double dt = now_ms() - t0;
        total_send_ms += dt;
        query_count++;
        return r;
    }
    return real_send(fd, buf, len, flags);
}

ssize_t recv(int fd, void *buf, size_t len, int flags) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_recv(fd, buf, len, flags);
        double dt = now_ms() - t0;
        if (r > 0) total_recv_ms += dt;
        return r;
    }
    return real_recv(fd, buf, len, flags);
}

/* --- read/write: low-level fallback paths --- */

ssize_t read(int fd, void *buf, size_t len) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_read(fd, buf, len);
        double dt = now_ms() - t0;
        if (r > 0) total_recv_ms += dt;
        return r;
    }
    return real_read(fd, buf, len);
}

ssize_t write(int fd, const void *buf, size_t len) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        double t0 = now_ms();
        ssize_t r = real_write(fd, buf, len);
        double dt = now_ms() - t0;
        total_send_ms += dt;
        return r;
    }
    return real_write(fd, buf, len);
}

/* --- poll: intercept MySQL I/O wait --- */

int poll(struct pollfd *fds, nfds_t nfds, int timeout) {
    int has_mysql = 0;
    for (nfds_t i = 0; i < nfds; i++) {
        if (fds[i].fd >= 0 && fds[i].fd < MAX_FDS && mysql_fds[fds[i].fd]) {
            has_mysql = 1;
            break;
        }
    }

    if (has_mysql) {
        double t0 = now_ms();
        int r = real_poll(fds, nfds, timeout);
        double dt = now_ms() - t0;
        total_poll_ms += dt;
        return r;
    }
    return real_poll(fds, nfds, timeout);
}

/* --- close: cleanup fd tracking, print stats --- */

int close(int fd) {
    if (fd >= 0 && fd < MAX_FDS && mysql_fds[fd]) {
        fprintf(stderr,
                "[mysql_shim] MySQL fd=%d closed: queries=%d "
                "send=%.2fms recv=%.2fms poll=%.2fms total_io=%.2fms\n",
                fd, query_count, total_send_ms, total_recv_ms,
                total_poll_ms, total_send_ms + total_recv_ms + total_poll_ms);
        mysql_fds[fd] = 0;
    }
    return real_close(fd);
}

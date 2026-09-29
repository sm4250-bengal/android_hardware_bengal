/* SPDX-License-Identifier: Apache-2.0 */
#include "loc.h"
#include <libqrtr.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
struct transport { int fd; uint32_t local_node, client; };
static int lookup(void *ctx)
{ return qrtr_new_lookup(((struct transport *)ctx)->fd, 16, 0, 0); }
static int transmit(void *ctx, uint32_t node, uint32_t port, const void *data, size_t n)
{ return qrtr_sendto(((struct transport *)ctx)->fd, node, port, data, n); }
static int receive(void *ctx, struct loc_packet *out, unsigned ms)
{
    struct transport *t = ctx;
    struct pollfd p = {.fd = t->fd, .events = POLLIN};
    int r = poll(&p, 1, (int)ms);
    if (r < 0 && errno == EINTR) return 0; /* engine checks cancellation */
    if (r <= 0) return r;
    if (p.revents & (POLLERR | POLLHUP | POLLNVAL)) return -1;
    struct sockaddr_qrtr from = {0};
    struct iovec iov = {.iov_base = out->data, .iov_len = LOC_CAP};
    struct msghdr msg = {.msg_name = &from, .msg_namelen = sizeof(from), .msg_iov = &iov, .msg_iovlen = 1};
    ssize_t n = recvmsg(t->fd, &msg, MSG_DONTWAIT | MSG_TRUNC);
    if (n < 0 || n > LOC_CAP || msg.msg_flags & MSG_TRUNC || msg.msg_namelen != sizeof(from) ||
        from.sq_family != AF_QIPCRTR) return -1;
    if (from.sq_port == QRTR_PORT_CTRL && from.sq_node != t->local_node) {
        memset(out, 0, offsetof(struct loc_packet, data));
        out->client = t->client;
        return 1; /* unrelated control packet, not a receive timeout */
    }
    struct qrtr_packet pkt = {0};
    if (qrtr_decode(&pkt, out->data, n, &from)) return -1;
    out->type = pkt.type; out->node = pkt.node; out->port = pkt.port;
    out->service = pkt.service; out->instance = pkt.instance; out->client = t->client;
    out->length = n;
    return 1;
}
static void finish(void *ctx)
{
    struct transport *t = ctx;
    /* Closing destroys this client's lookup and event subscription, even if
     * explicit cleanup requests were refused. It does not prove engine stop. */
    qrtr_close(t->fd); free(t);
}
int loc_qrtr_open(struct loc_io *io)
{
    struct transport *t = calloc(1, sizeof(*t));
    if (!t) return -1;
    t->fd = qrtr_open(0);
    if (t->fd < 0) { free(t); return -1; }
    struct sockaddr_qrtr local = {.sq_family = AF_QIPCRTR};
    socklen_t n = sizeof(local);
    if (getsockname(t->fd, (struct sockaddr *)&local, &n) ||
        bind(t->fd, (struct sockaddr *)&local, sizeof(local)) ||
        getsockname(t->fd, (struct sockaddr *)&local, &n) ||
        fcntl(t->fd, F_SETFD, FD_CLOEXEC) || fcntl(t->fd, F_SETFL, O_NONBLOCK)) {
        finish(t); return -1;
    }
    t->local_node = local.sq_node; t->client = local.sq_port;
    *io = (struct loc_io){t, t->client, lookup, transmit, receive, finish};
    return 0;
}

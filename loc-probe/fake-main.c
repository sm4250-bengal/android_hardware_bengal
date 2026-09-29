/* SPDX-License-Identifier: Apache-2.0
 * Test-only AF_UNIX SOCK_SEQPACKET transport, never included in Android.bp.
 * Envelope: six little-endian u32 type/node/port/service/instance/client,
 * followed by raw QMI (or no payload for name-service events). */
#define _GNU_SOURCE
#include "loc.h"
#include <libqrtr.h>
#include <errno.h>
#include <poll.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static volatile sig_atomic_t cancelled;
struct sink { const char *marker; int failed; };
static ssize_t sink_write(void *ctx, const char *data, size_t n)
{
    struct sink *sink = ctx;
    if (sink->failed || memmem(data, n, sink->marker, strlen(sink->marker))) {
        sink->failed = 1; errno = ENOSPC; return -1;
    }
    return fwrite(data, 1, n, stdout) == n ? (ssize_t)n : -1;
}
static void stop(int n) { cancelled = n; }
static int transmit(void *ctx, uint32_t node, uint32_t port, const void *data, size_t n)
{
    uint32_t hdr[6] = {QRTR_TYPE_DATA, node, port, 16, 0, 55};
    struct iovec v[] = {{hdr, sizeof(hdr)}, {(void *)data, n}};
    struct msghdr msg = {.msg_iov = v, .msg_iovlen = 2};
    return sendmsg(*(int *)ctx, &msg, MSG_NOSIGNAL | MSG_DONTWAIT) == (ssize_t)(sizeof(hdr)+n) ? 0 : -1;
}
static int lookup(void *ctx)
{
    uint32_t hdr[6] = {QRTR_TYPE_NEW_LOOKUP, 0, QRTR_PORT_CTRL, 16, 0, 55};
    return send(*(int *)ctx, hdr, sizeof(hdr), MSG_NOSIGNAL) == sizeof(hdr) ? 0 : -1;
}
static int receive(void *ctx, struct loc_packet *p, unsigned ms)
{
    struct pollfd fd = {*(int *)ctx, POLLIN, 0};
    int r = poll(&fd, 1, ms);
    if (r < 0 && errno == EINTR) return 0;
    if (r <= 0) return r;
    uint8_t data[LOC_CAP+24];
    ssize_t n = recv(fd.fd, data, sizeof(data), MSG_TRUNC);
    if (n < 24 || n > (ssize_t)sizeof(data)) return -1;
    memcpy(p, data, 24); p->length = n-24; memcpy(p->data, data+24, p->length);
    if (p->type == QRTR_TYPE_NEW_SERVER && getenv("LOC_TEST_CANCEL_BEFORE_DELETE")) cancelled = 1;
    return 1;
}
static void finish(void *ctx) { close(*(int *)ctx); }
static void fixed(void *ctx, const struct loc_fix *fix)
{
    unsigned *count = ctx;
    ++*count;
    printf("CALLBACK fix=%u utc=%llu\n", *count, (unsigned long long)fix->utc_ms);
    if (getenv("LOC_TEST_CANCEL_FIX") ||
        (getenv("LOC_TEST_CANCEL_TWO_FIXES") && *count == 2)) cancelled = 1;
}
static int observer_cancelled(void *ctx) { (void)ctx; return cancelled != 0; }
static void observer_started(void *ctx) { (void)ctx; puts("CALLBACK started"); }
static void satellites(void *ctx, const struct loc_sv_report *report) {
    (void)ctx;
    printf("CALLBACK satellites=%u\n", report->count);
    if (getenv("LOC_TEST_CANCEL_SV")) cancelled = 1;
}
static void sv_metadata(void *ctx, int present, unsigned count) {
    (void)ctx;
    printf("CALLBACK sv-metadata present=%d count=%u\n", present, count);
}
static void engine_state(void *ctx, uint32_t raw_state) {
    (void)ctx;
    printf("CALLBACK engine-raw=%u\n", raw_state);
}
static int take_time(void *ctx, struct loc_time_sample *sample) {
    (void)ctx;
    static int taken;
    /* Continuity peers deliberately keep offering samples after a timeout. */
    if (taken++ && strcmp(getenv("LOC_TEST_TIME"), "continuity")) return 0;
    *sample = (struct loc_time_sample){INT64_C(1700000000000), loc_boottime_ms()-5000, 27};
    if (!strcmp(getenv("LOC_TEST_TIME"), "expired")) sample->reference_ms = loc_boottime_ms() + 100000;
    return 1;
}
static int time_refused;
static void request_time(void *ctx) {
    (void)ctx; puts("CALLBACK time-request");
    if (time_refused) { puts("CALLBACK standalone-after-refusal"); cancelled = 1; }
}
static void time_result(void *ctx, int result) {
    (void)ctx; printf("CALLBACK time-result=%d\n", result);
    if (!strcmp(getenv("LOC_TEST_TIME"), "continuity")) return;
    if (result == 1) time_refused = 1; else cancelled = 1;
}
int main(int argc, char **argv)
{
    if (argc != 2 && (argc != 3 || (strcmp(argv[2], "--query-only") && strcmp(argv[2], "--receiver-info") && strcmp(argv[2], "--rf-metrics") && strcmp(argv[2], "--acquisition-config") && strcmp(argv[2], "--delete-aiding-all")))) return 2;
    int fd = atoi(argv[1]);
    struct sigaction a = {.sa_handler = stop}; sigemptyset(&a.sa_mask);
    sigaction(SIGTERM, &a, NULL); sigaction(SIGINT, &a, NULL); sigaction(SIGHUP, &a, NULL);
    setvbuf(stdout, NULL, _IOLBF, 0);
    struct loc_io io = {&fd, 55, lookup, transmit, receive, finish};
    struct sink sink = {.marker = getenv("LOC_TEST_LOG_FAIL")};
    FILE *log = stdout;
    if (sink.marker) {
        log = fopencookie(&sink, "w", (cookie_io_functions_t){.write = sink_write});
        if (!log) return 2;
        setvbuf(log, NULL, _IOLBF, 0);
    }
    unsigned fixes = 0;
    struct loc_observer observer = {.ctx=&fixes, .cancelled=observer_cancelled,
        .started=observer_started, .fix=fixed, .interval_ms=1500, .satellites=satellites,
        .sv_metadata=sv_metadata, .engine_state=engine_state};
    if (getenv("LOC_TEST_TIME")) {
        observer.take_time=take_time; observer.request_time=request_time; observer.time_result=time_result;
    }
    int ret = argc == 3 && !strcmp(argv[2], "--rf-metrics") ? loc_rf_metrics(&io, log, 70, 60, &cancelled)
              : argc == 3 && !strcmp(argv[2], "--delete-aiding-all") ? loc_delete_aiding_all(&io, log, 60, &cancelled)
              : argc == 3 && !strcmp(argv[2], "--acquisition-config") ? loc_acquisition_config(&io, log, 60, &cancelled)
              : argc == 3 && !strcmp(argv[2], "--receiver-info") ? loc_receiver_info(&io, log, 60, &cancelled)
              : getenv("LOC_TEST_FOLLOW") ? loc_follow(&io, log, &observer)
              : argc == 3 ? loc_query(&io, log, 60, &cancelled)
                       : loc_run_observed(&io, log, 70, 60, &cancelled, fixed, &fixes);
    if (log != stdout) fclose(log);
    return ret;
}

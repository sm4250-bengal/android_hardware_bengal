/* SPDX-License-Identifier: Apache-2.0 */
#include "loc.h"
#include <libqrtr.h>
#include <inttypes.h>
#include <stdbool.h>
#include <string.h>
#include <time.h>
#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "The pinned libqrtr QMI codec requires the target little-endian byte order"
#endif

#define EI(t, s, tag, field) {t, 1, sizeof(((s *)0)->field), NO_ARRAY, tag, offsetof(s, field), NULL}
struct value { uint32_t value; };
struct get_answer { uint32_t status, value; };
struct setting { uint64_t value; bool lock_client; };
struct lock_request { uint32_t type; uint64_t clients; };
struct mask { uint64_t value; };
struct registration { uint64_t value; uint32_t client_type; };
struct start { uint8_t session; uint32_t recurrence, accuracy, interval; };
struct utc_time { uint64_t utc; uint32_t uncertainty; };
struct delete_aiding { uint8_t all; };
struct answer { struct qmi_response_type_v01 result; };
/* OEM LOC0x44 / IDL2.134 descriptor3504: mandatory01/u8 only. */
static struct qmi_elem_info delete_aiding_ei[] = {
    EI(QMI_UNSIGNED_1_BYTE, struct delete_aiding, 1, all), {0}};
static struct qmi_elem_info utc_ei[] = {
    EI(QMI_UNSIGNED_8_BYTE, struct utc_time, 1, utc),
    EI(QMI_UNSIGNED_4_BYTE, struct utc_time, 2, uncertainty), {0}};
static struct qmi_elem_info empty[] = {{.data_type = QMI_EOTI}};
static struct qmi_elem_info val_ei[] = {EI(QMI_UNSIGNED_4_BYTE, struct value, 1, value), {0}};
static struct qmi_elem_info get_ei[] = {EI(QMI_UNSIGNED_4_BYTE, struct get_answer, 1, status),
    EI(QMI_UNSIGNED_4_BYTE, struct get_answer, 0x10, value), {0}};
static struct qmi_elem_info mask_ei[] = {EI(QMI_UNSIGNED_8_BYTE, struct mask, 1, value), {0}};
static struct qmi_elem_info registration_ei[] = {
    EI(QMI_UNSIGNED_8_BYTE, struct registration, 1, value),
    EI(QMI_UNSIGNED_4_BYTE, struct registration, 0x11, client_type), {0}};
static struct qmi_elem_info lock_ei[] = {
    EI(QMI_UNSIGNED_4_BYTE, struct lock_request, 1, type),
    EI(QMI_UNSIGNED_8_BYTE, struct lock_request, 0x11, clients), {0}};
static struct qmi_elem_info start_ei[] = {
    EI(QMI_UNSIGNED_1_BYTE, struct start, 1, session),
    EI(QMI_UNSIGNED_4_BYTE, struct start, 0x10, recurrence),
    EI(QMI_UNSIGNED_4_BYTE, struct start, 0x11, accuracy),
    EI(QMI_UNSIGNED_4_BYTE, struct start, 0x13, interval), {0}};
static struct qmi_elem_info stop_ei[] = {EI(QMI_UNSIGNED_1_BYTE, struct start, 1, session), {0}};
static struct qmi_elem_info result_ei[] = {
    {QMI_STRUCT, 1, sizeof(struct qmi_response_type_v01), NO_ARRAY, 2,
     offsetof(struct answer, result), qmi_response_type_v01_ei}, {0}};
struct state {
    struct loc_io *io;
    FILE *log;
    volatile sig_atomic_t *cancelled;
    uint32_t node, port, instance;
    uint16_t txn;
    unsigned timeout, events;
    bool cleanup, gone, log_failed, attempted, delivering;
    uint64_t last_fix_utc;
    loc_fix_callback on_fix;
    void *fix_ctx;
    const struct loc_observer *observer;
    bool time_disabled;
    uint16_t timed_out_time_txn;
    bool rf_metrics, rf_tick_changed, rf_truncated;
    unsigned rf_reports, rf_raw_logs;
    uint64_t rf_first_tick, rf_last_tick;
};
static bool cancelled(struct state *s)
{
    return (s->cancelled && *s->cancelled) ||
           (s->observer && s->observer->cancelled(s->observer->ctx));
}
/* Flush catches buffered sink failures, not just immediate fprintf errors.
 * Failure remains sticky. Cleanup must still reach the modem without a log. */
static int log_health(struct state *s)
{
    if (fflush(s->log) || ferror(s->log)) {
        if (!s->log_failed)
            fputs("LOC FAIL log-io: normal work aborted; bounded cleanup still attempted\n", stderr);
        s->log_failed = true;
    }
    return s->log_failed && !s->cleanup ? -1 : 0;
}
static uint64_t now_ms(void)
{
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (uint64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static unsigned le16(const uint8_t *p) { return p[0] | ((unsigned)p[1] << 8); }
static int raw(struct state *s, const char *dir, const uint8_t *p, size_t n)
{
    fprintf(s->log, "LOC %s raw=", dir);
    for (size_t i = 0; i < n; ++i) fprintf(s->log, "%02x", p[i]);
    fputc('\n', s->log);
    return log_health(s);
}
/* Validate lengths and duplicates BEFORE entering libqrtr's permissive codec. */
static int tlvs(const struct loc_packet *p, const uint8_t **value, unsigned *length)
{
    if (p->length < 7 || p->length > LOC_CAP || le16(p->data + 5) != p->length - 7)
        return -1;
    memset(value, 0, 256 * sizeof(*value));
    memset(length, 0, 256 * sizeof(*length));
    for (size_t i = 7; i < p->length;) {
        if (p->length - i < 3) return -1;
        unsigned tag = p->data[i], n = le16(p->data + i + 1);
        if (n > p->length - i - 3 || value[tag]) return -1;
        value[tag] = p->data + i + 3; length[tag] = n; i += 3 + n;
    }
    return 0;
}
/* IDL2.134: bit42/indication0xCE, descriptors1828/7691. Scalar byte copies
 * avoid alignment/aliasing assumptions. No signal thresholds or RF verdict. */
static int rf_event(struct state *s, const uint8_t **v, const unsigned *n)
{
    if (!s->delivering || s->cleanup || cancelled(s)) return 0;
    if (!v[1] || n[1] != 8 || !v[2] || n[2] != 4) goto malformed;
    for (unsigned tag = 0x10; tag <= 0x19; ++tag)
        if (v[tag] && n[tag] != (tag == 0x17 ? 4u : 8u)) goto malformed;
    uint64_t tick; int32_t gain;
    memcpy(&tick, v[1], 8); memcpy(&gain, v[2], 4);
    if (!s->rf_reports) s->rf_first_tick = tick;
    else if (tick != s->rf_last_tick) s->rf_tick_changed = true;
    s->rf_last_tick = tick;
    /* At most256 decoded summaries/raw RF packets per session. Still validate
     * later reports and retain the latest tick, without unbounded counters. */
    if (s->rf_reports == 256) { s->rf_truncated = true; return 0; }
    ++s->rf_reports;
    fprintf(s->log, "LOC RF tick=%" PRIu64 " band1-pga-db=%" PRId32, tick, gain);
    static const char *names[] = {"gps-l1", "glo-g1", "bds-b1", "gal-e1", "gps-l2",
                                 "glo-g2", "bds-b2", "band5", "gps-l5", "gal-e5a"};
    for (unsigned tag = 0x10; tag <= 0x19; ++tag) {
        const char *name = names[tag-0x10];
        if (!v[tag]) fprintf(s->log, " %s=unavailable", name);
        else if (tag == 0x17) {
            memcpy(&gain, v[tag], 4);
            fprintf(s->log, " band5-pga-db=%" PRId32, gain);
        } else {
            uint16_t amp_i, amp_q; int32_t jammer;
            memcpy(&amp_i, v[tag], 2); memcpy(&amp_q, v[tag]+2, 2);
            memcpy(&jammer, v[tag]+4, 4);
            fprintf(s->log, " %s=i-raw:%u,q-raw:%u,jammer-db:%" PRId32,
                    name, (unsigned)amp_i, (unsigned)amp_q, jammer);
        }
    }
    for (unsigned tag = 0; tag < 256; ++tag)
        if (v[tag] && tag != 1 && tag != 2 && (tag < 0x10 || tag > 0x19))
            fprintf(s->log, " unknown-tlv=%02x/len%u", tag, n[tag]);
    fputc('\n', s->log);
    return log_health(s);
malformed:
    fprintf(s->log, "LOC RF malformed\n");
    return -1;
}
static int event(struct state *s, unsigned id, const uint8_t **v, unsigned *n)
{
    if (id == 0xce && s->rf_metrics) return rf_event(s, v, n);
    if (id == 0x26) {
        if (!v[1] || n[1] < 7 || n[1] > 256 || v[1][0] != '$') return -1;
        unsigned end = n[1];
        while (end && (v[1][end-1] == '\r' || v[1][end-1] == '\n')) --end;
        if (end < 4 || v[1][end-3] != '*') return -1;
        unsigned checksum = 0, expected = 0;
        for (unsigned j = 1; j < end-3; ++j) {
            if (v[1][j] < 32 || v[1][j] > 126) return -1;
            checksum ^= v[1][j];
        }
        for (unsigned j = end-2; j < end; ++j) {
            unsigned c = v[1][j];
            if (c >= '0' && c <= '9') c -= '0';
            else if (c >= 'A' && c <= 'F') c = c - 'A' + 10;
            else return -1;
            expected = expected * 16 + c;
        }
        if (checksum != expected) return -1;
        fprintf(s->log, "LOC NMEA %.*s\n", (int)end, v[1]);
    } else if (id == 0x24) {
        struct loc_fix fix;
        int decoded = loc_position_decode(v, n, 1, &fix);
        if (decoded < 0) return -1;
        if (v[2][0] != 1) { fprintf(s->log, "LOC IGNORE other-session\n"); return log_health(s); }
        uint32_t status;
        memcpy(&status, v[1], sizeof(status));
        fprintf(s->log, "LOC POSITION status=%u native-fix=%s\n", status, decoded ? "available" : "none");
        if (log_health(s)) return -1;
        if (decoded && s->delivering && !s->cleanup && !cancelled(s) &&
            fix.utc_ms > s->last_fix_utc) {
            s->last_fix_utc = fix.utc_ms;
            if (s->on_fix) s->on_fix(s->fix_ctx, &fix);
        }
    } else if (id == 0x25) {
        struct loc_sv_report report;
        int decoded = loc_satellite_decode(v, n, &report);
        if (decoded < 0) return -1;
        if (log_health(s)) return -1;
        if (s->delivering && !s->cleanup && !cancelled(s) && s->observer && s->observer->sv_metadata)
            s->observer->sv_metadata(s->observer->ctx, decoded, report.count);
        if (!decoded) {
            fprintf(s->log, "LOC SV list=absent\n");
        } else {
            unsigned searching = 0, tracked = 0, cn0_count = 0;
            float max_cn0 = 0;
            for (unsigned i = 0; i < report.count; ++i) {
                const struct loc_sv *sv = &report.sv[i];
                if (sv->valid & LOC_SV_STATUS) {
                    searching += sv->status == 2; tracked += sv->status == 3;
                }
                if (sv->valid & LOC_SV_CN0) {
                    ++cn0_count;
                    if (sv->cn0 > max_cn0) max_cn0 = sv->cn0;
                }
            }
            fprintf(s->log, "LOC SV count=%u searching=%u tracked=%u cn0-valid=%u max-cn0=",
                    report.count, searching, tracked, cn0_count);
            if (cn0_count) fprintf(s->log, "%.1f\n", max_cn0); else fputs("unavailable\n", s->log);
            if (log_health(s)) return -1;
            if (s->delivering && !s->cleanup && !cancelled(s) && s->observer && s->observer->satellites)
                s->observer->satellites(s->observer->ctx, &report);
        }
    } else if (id == 0x28) {
        /* Optional server URLs are advisory; framework owns trusted NTP. */
        if (!s->observer || !s->observer->take_time || !s->observer->request_time) return 0;
        if (!s->time_disabled && !s->cleanup && !cancelled(s))
            s->observer->request_time(s->observer->ctx);
        fprintf(s->log, "LOC TIME %s\n", s->time_disabled ? "request ignored (UTC disabled for this client)" : "requested");
    } else if (id == 0x2b) {
        if (!v[1] || n[1] != 4) return -1;
        fprintf(s->log, "LOC ENGINE raw-only\n");
        if (log_health(s)) return -1;
        if (s->delivering && !s->cleanup && !cancelled(s) && s->observer && s->observer->engine_state) {
            uint32_t raw_state;
            memcpy(&raw_state, v[1], sizeof(raw_state));
            s->observer->engine_state(s->observer->ctx, raw_state);
        }
    } else return 0;
    ++s->events;
    return log_health(s);
}
static int receive(struct state *s, struct loc_packet *p, uint64_t deadline)
{
    for (;;) {
        if (log_health(s)) return -1;
        if (!s->cleanup && cancelled(s)) { fprintf(s->log, "LOC CANCEL requested\n"); return -1; }
        uint64_t now = now_ms();
        if (now >= deadline) return 0;
        unsigned wait_ms = (unsigned)(deadline - now);
        if (wait_ms > 100) wait_ms = 100;
        int r = s->io->recv(s->io->ctx, p, wait_ms);
        if (r < 0) return r;
        if (!r) continue;
        if (p->client != s->io->client) { fprintf(s->log, "LOC IGNORE wrong-client\n"); continue; }
        if ((p->type == QRTR_TYPE_DEL_SERVER && p->service == 16 &&
             p->node == s->node && p->port == s->port) ||
            (p->type == QRTR_TYPE_BYE && p->node == s->node)) {
            s->gone = true; fprintf(s->log, "LOC FAIL service-disappeared\n"); return -1;
        }
        if (p->type != QRTR_TYPE_DATA || p->node != s->node || p->port != s->port) {
            fprintf(s->log, "LOC IGNORE unrelated-source\n"); continue;
        }
        bool rf_packet = s->rf_metrics && p->length >= 7 &&
                         p->data[0] == QMI_INDICATION && le16(p->data+3) == 0xce;
        if (!rf_packet || s->rf_raw_logs < 256) {
            if (raw(s, "RX", p->data, p->length <= LOC_CAP ? p->length : LOC_CAP)) return -1;
            if (rf_packet) ++s->rf_raw_logs;
        }
        return 1;
    }
}
/* Read-only receiver facts. Never reinterpret missing optional fields as zero.
 * IDL2.134 descriptors2921/7415: strings are raw TLV values, not C structs. */
struct receiver_info {
    uint32_t revision, constellation[6];
    bool present[6], version_present[3];
    char version[3][256];
};
static int decode_receiver_info(unsigned id, const uint8_t **v, const unsigned *n,
                                struct receiver_info *info)
{
    if (id == 0x32) {
        if (!v[2] || n[2] != 4) return -1;
        memcpy(&info->revision, v[2], 4);
        for (unsigned i = 0; i < 3; ++i) if (v[0x10+i]) {
            unsigned length = n[0x10+i], limit = i == 2 ? 255 : 127;
            if (length > limit) return -1;
            for (unsigned j = 0; j < length; ++j)
                if (v[0x10+i][j] < 32 || v[0x10+i][j] > 126) return -1;
            memcpy(info->version[i], v[0x10+i], length);
            info->version[i][length] = 0;
            info->version_present[i] = true;
        }
    } else {
        for (unsigned i = 0; i < 6; ++i) if (v[0x10+i]) {
            if (n[0x10+i] != 4) return -1;
            memcpy(&info->constellation[i], v[0x10+i], 4);
            info->present[i] = true;
        }
    }
    return 0;
}
static void print_receiver_info(FILE *log, unsigned id, const struct receiver_info *info)
{
    if (id == 0x32) {
        static const char *names[] = {"measurement-firmware", "host-software", "gnss-software"};
        fprintf(log, "LOC RECEIVER revision=%u\n", info->revision);
        for (unsigned i = 0; i < 3; ++i) {
            fprintf(log, "LOC RECEIVER %s=", names[i]);
            if (info->version_present[i]) fprintf(log, "\"%s\"\n", info->version[i]);
            else fputs("unavailable\n", log);
        }
    } else {
        static const char *names[] = {"GPS", "GLONASS", "BDS", "QZSS", "Galileo", "NavIC"};
        for (unsigned i = 0; i < 6; ++i) {
            fprintf(log, "LOC CONSTELLATION %s=", names[i]);
            if (!info->present[i]) { fputs("unavailable\n", log); continue; }
            const char *status;
            switch (info->constellation[i]) {
                case 0: status="enabled-mandatory"; break;
                case 1: status="enabled-internally"; break;
                case 2: status="enabled-by-client"; break;
                case 100: status="unsupported"; break;
                case 101: status="disabled-internally"; break;
                case 102: status="disabled-by-client"; break;
                case 103: status="disabled-no-memory"; break;
                default: status="unknown"; break;
            }
            fprintf(log, "%s raw=%u\n", status, info->constellation[i]);
        }
    }
}
/* LOC IDL2.134: D5 optional10/u16; B7 optional10..15/u64;
 * D8 mandatory02/u32 parameter type1 and optional10/u8 degrees0..90.
 * Keep all blacklist bits: the IDL does not declare a reserved-bit rejection
 * rule. No GPS bitmap exists in B7. Missing optional data is not zero. */
struct acquisition_config { bool present[6]; uint64_t value[6]; };
static int decode_acquisition_config(unsigned id,
        const uint8_t **v, const unsigned *n, struct acquisition_config *a)
{
    if (id == 0xd8) {
        uint32_t parameter = 0;
        if (v[2]) {
            if (n[2] != 4) return -1;
            memcpy(&parameter, v[2], 4);
            if (parameter != 1) return -1;
        } else return -1;
    }
    unsigned count = id == 0xb7 ? 6 : 1;
    unsigned width = id == 0xb7 ? 8 : id == 0xd5 ? 2 : 1;
    for (unsigned i = 0; i < count; ++i) if (v[0x10+i]) {
        if (n[0x10+i] != width) return -1;
        memcpy(&a->value[i], v[0x10+i], width);
        if (id == 0xd8 && a->value[i] > 90) return -1;
        a->present[i] = true;
    }
    return 0;
}
static bool print_acquisition_config(FILE *log, unsigned id, const struct acquisition_config *a)
{
    static const char *names[] = {"GLONASS", "BDS", "QZSS", "Galileo", "SBAS", "NavIC"};
    bool missing = false;
    for (unsigned i = 0; i < (id == 0xb7 ? 6u : 1u); ++i) {
        if (id == 0xb7) fprintf(log, "LOC ACQUISITION blacklist %s=", names[i]);
        else fprintf(log, "LOC ACQUISITION %s=", id == 0xd5 ? "minimum-gps-week" : "minimum-sv-elevation-deg");
        if (!a->present[i]) { fputs("unavailable\n", log); missing = true; }
        else if (id == 0xb7) fprintf(log, "0x%016" PRIx64 "\n", a->value[i]);
        else fprintf(log, "%" PRIu64 "\n", a->value[i]);
    }
    return missing;
}
struct fix_criteria {
    bool present[9];
    uint32_t value[9], tbm;
    unsigned unknown_length[256];
    bool unknown[256];
};
static int decode_fix_criteria(const uint8_t **v, const unsigned *n, struct fix_criteria *f)
{
    for (unsigned tag = 0x10; tag <= 0x18; ++tag) if (v[tag]) {
        f->present[tag-0x10] = true;
        if (tag == 0x13) {
            /* Aggregate strings: provider<=24, name<=32, validity:u8,
             * version<=8. Each string has an inner u8 length (c:42). */
            unsigned offset = 0;
            const unsigned limits[] = {24, 32, 8};
            for (unsigned i = 0; i < 3; ++i) {
                if (i == 2 && (offset >= n[tag] || v[tag][offset++] > 1)) return -1;
                if (offset >= n[tag]) return -1;
                unsigned length = v[tag][offset++];
                if (length > limits[i] || length > n[tag]-offset) return -1;
                offset += length;
            }
            if (offset != n[tag]) return -1;
        } else {
            unsigned width = tag == 0x17 ? 1 : tag == 0x18 ? 8 : 4;
            if (n[tag] != width || (tag == 0x17 && v[tag][0] > 1)) return -1;
            if (tag == 0x17) f->value[tag-0x10] = v[tag][0];
            else memcpy(&f->value[tag-0x10], v[tag], 4);
            if (tag == 0x18) memcpy(&f->tbm, v[tag]+4, 4);
        }
    }
    for (unsigned tag = 0; tag < 256; ++tag)
        if (v[tag] && tag != 1 && (tag < 0x10 || tag > 0x18)) {
            f->unknown[tag] = true; f->unknown_length[tag] = n[tag];
        }
    return 0;
}
static void print_fix_criteria(FILE *log, const struct fix_criteria *f)
{
    static const char *names[] = {"accuracy-raw", "intermediate-raw", "interval-ms",
        "application-id", "altitude-raw", "intermediate-ms", "timeout-ms", "share", "power-mode-raw"};
    fputs("LOC CRITERIA accepted", log);
    for (unsigned i = 0; i < 9; ++i) {
        fprintf(log, " %s=", names[i]);
        if (!f->present[i]) fputs("unavailable", log);
        else if (i == 3) fputs("present", log);
        else if (i == 8) {
            int32_t mode; memcpy(&mode, &f->value[i], 4);
            fprintf(log, "%" PRId32 " tbm-ms=%u", mode, f->tbm);
        } else fprintf(log, "%u", f->value[i]);
    }
    for (unsigned tag = 0; tag < 256; ++tag) if (f->unknown[tag])
        fprintf(log, " unknown-tlv=%02x/len%u", tag, f->unknown_length[tag]);
    fputc('\n', log);
}
/* A timed-out UTC request is optional, but its completion can still arrive.
 * No further UTC command is allowed on this client, so same-peer indications
 * (whose sequence is independent) cannot be mistaken for a newer request.
 * Call only after normal peer/framing validation, also while draining cleanup. */
static int late_time(struct state *s, const struct loc_packet *p,
                     const uint8_t **v, const unsigned *n)
{
    if (!s->time_disabled || le16(p->data + 3) != 0x38) return 0;
    unsigned type = p->data[0];
    struct qrtr_packet q = {.data = (void *)p->data, .data_len = p->length};
    if (type == QMI_RESPONSE) {
        if (le16(p->data + 1) != s->timed_out_time_txn) {
            fprintf(s->log, "LOC TIME unrelated-transaction ignored\n");
            return log_health(s) ? -1 : 1;
        }
        struct answer a = {0};
        if (!v[2] || n[2] != 4 ||
            qmi_decode_message(&a, NULL, &q, type, 0x38, result_ei) < 0) return -1;
    } else if (type == QMI_INDICATION) {
        struct value status = {0};
        if (!v[1] || n[1] != 4 ||
            qmi_decode_message(&status, NULL, &q, type, 0x38, val_ei) < 0) return -1;
    } else return -1;
    fprintf(s->log, "LOC TIME late-completion ignored (UTC remains disabled for this client)\n");
    return log_health(s) ? -1 : 1;
}
enum { COMMAND_TIMEOUT = -2 };
static int command(struct state *s, unsigned id, const void *input,
                   struct qmi_elem_info *ei, bool paired, struct setting *get)
{
    const bool receiver_query = id == 0x32 || id == 0xbf;
    const bool criteria_query = s->rf_metrics && id == 0x33;
    const bool acquisition_query = id == 0xd5 || id == 0xb7 || id == 0xd8;
    struct acquisition_config acquisition = {0};
    bool missing = false;
    bool unsupported = false;
    struct receiver_info info = {0};
    struct fix_criteria criteria = {0};
    uint32_t criteria_status = 0;
    uint8_t buf[128];
    struct qrtr_packet q = {.data = buf, .data_len = sizeof(buf)};
    unsigned txn = ++s->txn;
    s->attempted = false;
    if (log_health(s)) return -1;
    if ((!s->cleanup && cancelled(s)) || s->gone) return -1;
    if (qmi_encode_message(&q, QMI_REQUEST, id, txn, input, ei) < 0) return -1;
    if (raw(s, "TX", q.data, q.data_len)) return -1;
    s->attempted = true; /* even send failure can mean uncertain acceptance */
    if (s->io->send(s->io->ctx, s->node, s->port, q.data, q.data_len)) return -1;
    uint64_t deadline = now_ms() + s->timeout;
    bool response = false, indication = !paired;
    while (!response || !indication) {
        struct loc_packet p;
        int r = receive(s, &p, deadline);
        if (r <= 0) { fprintf(s->log, "LOC FAIL id=%04x %s response=%d indication=%d\n",
                              id, r ? "transport" : "timeout", response, indication);
            return r ? -1 : COMMAND_TIMEOUT; }
        const uint8_t *v[256]; unsigned n[256], msg;
        q = (struct qrtr_packet){.data = p.data, .data_len = p.length};
        if (tlvs(&p, v, n) || qmi_decode_header(&q, &msg)) goto malformed;
        int late = late_time(s, &p, v, n);
        if (late < 0) goto malformed;
        if (late) continue;
        unsigned type = p.data[0], rxn = le16(p.data + 1);
        /* Only responses carry request transactions. Device3 increments an
         * independent indication sequence, including unsolicited events.
         * Correlate completions by peer + serialized pending message ID;
         * readback remains required after restoration (no concurrent owner). */
        if (type == QMI_RESPONSE && rxn != txn) {
            fprintf(s->log, "LOC IGNORE wrong-transaction\n"); continue;
        }
        if (type == QMI_INDICATION && (msg == 0x26 || msg == 0x24 || msg == 0x25 || msg == 0x28 || msg == 0x2b || (s->rf_metrics && msg == 0xce))) {
            if (event(s, msg, v, n)) goto malformed;
            continue;
        }
        if (msg != id) { fprintf(s->log, "LOC IGNORE unrelated-message\n"); continue; }
        if (type == QMI_RESPONSE) {
            struct answer a = {0};
            if (!v[2] || n[2] != 4 || qmi_decode_message(&a, NULL, &q, type, id, result_ei) < 0)
                goto malformed;
            fprintf(s->log, "LOC RESPONSE id=%04x txn=%u result=%u error=%u\n", id, txn, a.result.result, a.result.error);
            if (log_health(s)) return -1;
            if (a.result.result || a.result.error) {
                if (criteria_query) return 1; /* explicit optional query refusal */
                if ((receiver_query || acquisition_query) && a.result.result == 1 && a.result.error == 94) {
                    fprintf(s->log, "LOC %s id=%04x unsupported=response\n", acquisition_query ? "ACQUISITION" : "RECEIVER", id);
                    return log_health(s) ? -1 : 1;
                }
                fprintf(s->log, "LOC FAIL %s\n", a.result.error == 94 ? "QMI_ERR_NOT_SUPPORTED" : "QMI-response-refused");
                return id == 0x38 ? 1 : -1; /* optional UTC refusal is nonfatal */
            }
            response = true;
        } else if (type == QMI_INDICATION && paired) {
            struct value status = {0}; struct get_answer value = {0};
            if (!v[1] || n[1] != 4) goto malformed;
            // Revision has a second mandatory TLV (02); the generic status
            // codec rejects that unknown mandatory tag. Decode bounded bytes.
            if (receiver_query || criteria_query || acquisition_query) memcpy(&status.value, v[1], 4);
            else if (qmi_decode_message(&status, NULL, &q, type, id, val_ei) < 0) goto malformed;
            fprintf(s->log, "LOC INDICATION id=%04x sequence=%u status=%u\n", id, rxn, status.value);
            if (log_health(s)) return -1;
            if (criteria_query) {
                criteria_status = status.value;
                if (!status.value && decode_fix_criteria(v, n, &criteria)) goto malformed;
            } else if (acquisition_query) {
                if (decode_acquisition_config(id, v, n, &acquisition)) goto malformed;
                if (status.value == 2) unsupported = true;
                else if (status.value) return -1;
            } else if (receiver_query && status.value == 2) unsupported = true;
            else if (status.value) return id == 0x38 ? 1 : -1;
            else if (receiver_query && decode_receiver_info(id, v, n, &info)) goto malformed;
            if (get) {
                *get = (struct setting){0};
                if (id == 0x3b) {
                    /* Default Get/Set subscription is DD(2). Never save one
                     * subscription and unknowingly restore a different one. */
                    uint32_t sub = 2;
                    if (v[0x11]) {
                        if (n[0x11] != sizeof(sub)) goto malformed;
                        memcpy(&sub, v[0x11], sizeof(sub));
                    }
                    if (sub != 2 || (v[0x10] && n[0x10] != 4)) goto malformed;
                }
                if (id == 0x3b && v[0x12]) {
                    /* LOC IDL v2.134: Get12/u64, Set11/u64 overrides
                     * mandatory legacy lockType. AFW=1, NFW=2, privileged=4.
                     * Preserve the whole mask; no enum cast/truncation. */
                    if (n[0x12] != sizeof(get->value)) goto malformed;
                    memcpy(&get->value, v[0x12], sizeof(get->value));
                    if (get->value & ~UINT64_C(7)) {
                        fprintf(s->log, "LOC FAIL unsupported-lock-client-mask\n");
                        return -1;
                    }
                    get->lock_client = true;
                } else {
                    if (!v[0x10] || n[0x10] != 4 || qmi_decode_message(&value, NULL, &q, type, id, get_ei) < 0)
                        goto malformed;
                    get->value = value.value;
                }
            }
            indication = true;
        } else goto malformed;
    }
    if (receiver_query) {
        if (unsupported) fprintf(s->log, "LOC RECEIVER id=%04x unsupported=indication\n", id);
        else print_receiver_info(s->log, id, &info);
    }
    if (criteria_query) {
        if (criteria_status) return log_health(s) ? -1 : 1;
        print_fix_criteria(s->log, &criteria);
    }
    if (acquisition_query) {
        if (unsupported) fprintf(s->log, "LOC ACQUISITION id=%04x unsupported=indication\n", id);
        else missing = print_acquisition_config(s->log, id, &acquisition);
    }
    return log_health(s) ? -1 : (unsupported || missing) ? 1 : 0;
malformed:
    fprintf(s->log, "LOC FAIL malformed-QMI-or-NMEA id=%04x\n", id);
    return -1;
}
static int get_value(struct state *s, unsigned id, struct setting *v)
{ return command(s, id, NULL, empty, true, v); }
static int set_value(struct state *s, unsigned id, struct setting v)
{
    if (id == 0x3a && v.lock_client) {
        struct lock_request a = {.type = 3, .clients = v.value};
        return command(s, id, &a, lock_ei, true, NULL);
    }
    struct value a = {(uint32_t)v.value};
    return command(s, id, &a, val_ei, true, NULL);
}
/* Only the serialized transport owner calls this, outside event/command
 * callbacks. Consume one sample per loop; never loop on a producer's queue. */
static int aid_time(struct state *s)
{
    const struct loc_observer *o = s->observer;
    struct loc_time_sample sample;
    if (!o || !o->take_time || !o->request_time || s->time_disabled || cancelled(s) ||
        !o->take_time(o->ctx, &sample)) return 0;
    struct utc_time request;
    int result;
    if (loc_time_adjust(&sample, loc_boottime_ms(), &request.utc)) result = 2;
    else {
        request.uncertainty = (uint32_t)sample.uncertainty_ms;
        /* LOC injectUTC is wire ID0x38, not generated table index46.
         * Both epochs are UTC Unix ms; no leap/GPS-epoch adjustment. */
        result = command(s, 0x38, &request, utc_ei, true, NULL);
        if (result == COMMAND_TIMEOUT && !cancelled(s)) {
            s->time_disabled = true;
            s->timed_out_time_txn = s->txn;
            result = 3;
            fprintf(s->log, "LOC TIME timeout: UTC disabled for this client; standalone session continues\n");
        }
        if (result < 0) return -1;
    }
    fprintf(s->log, "LOC TIME result=%d (0=accepted,1=refused,2=expired-invalid,3=timeout-disabled)\n", result);
    if (o->time_result && !cancelled(s)) o->time_result(o->ctx, result);
    return log_health(s);
}
enum run_mode { SESSION, QUERY, RECEIVER_INFO, RF_METRICS, ACQUISITION_CONFIG, DELETE_AIDING_ALL };
static int loc_execute(struct loc_io *io, FILE *log, unsigned run_ms, unsigned timeout,
                       volatile sig_atomic_t *signal_cancelled, enum run_mode mode,
                       loc_fix_callback on_fix, void *fix_ctx, const struct loc_observer *observer)
{
    struct state s = {.io = io, .log = log, .cancelled = signal_cancelled, .timeout = timeout,
                      .on_fix = on_fix, .fix_ctx = fix_ctx, .observer = observer, .rf_metrics = mode == RF_METRICS};
    struct setting saved[3] = {{0}};
    const unsigned gets[] = {0x3b, 0x3f, 0x4b}, sets[] = {0x3a, 0x3e, 0x4a};
    struct setting desired[] = {{3, false}, {0xffff, false}, {4, false}};
    bool changed[3] = {false}, registered = false, started = false;
    int result = 1, cleanup_error = 0;
    bool info_partial = false;
    const char *criteria_result = "not-requested";
    struct registration mask = {0x87, 1}; struct start session = {1, 1, 3, 1000};
    struct qmi_elem_info *reg_ei = mask_ei;
    if (log_health(&s) || (!observer && (mode == SESSION || mode == RF_METRICS) && !run_ms) || run_ms > 180000 ||
        !timeout || timeout > 2000 || (observer && (!observer->cancelled ||
        observer->interval_ms < 1000 || observer->interval_ms > INT32_MAX)) ||
        io->lookup(io->ctx)) goto done;
    if (observer) session.interval = observer->interval_ms;
    uint64_t deadline = now_ms() + timeout;
    while (!cancelled(&s)) {
        struct loc_packet p;
        uint64_t now = now_ms();
        if (now >= deadline) break;
        unsigned wait_ms = (unsigned)(deadline - now);
        if (wait_ms > 100) wait_ms = 100;
        int r = io->recv(io->ctx, &p, wait_ms);
        if (r < 0) goto done;
        if (r == 1 && p.client == io->client && p.type == QRTR_TYPE_NEW_SERVER && p.service == 16 &&
            p.port && p.port != QRTR_PORT_CTRL) {
            s.node = p.node; s.port = p.port; s.instance = p.instance; break;
        }
    }
    if (!s.port) { fprintf(log, "LOC FAIL discovery\n"); goto done; }
    fprintf(log, "LOC svc=0x10 inst=%u node=%u port=%u client=%u\n", s.instance, s.node, s.port, io->client);
    if (mode == DELETE_AIDING_ALL) {
        const struct delete_aiding request = {.all = 1};
        /* One serialized request only. Never retry, read/set configuration,
         * register/start a session or claim to restore deleted receiver state. */
        result = command(&s, 0x44, &request, delete_aiding_ei, true, NULL) != 0;
        goto done;
    }
    if (mode == ACQUISITION_CONFIG) {
        const unsigned ids[] = {0xd5, 0xb7, 0xd8};
        struct value parameter = {1};
        for (unsigned i = 0; i < 3; ++i) {
            int ret = command(&s, ids[i], i == 2 ? &parameter : NULL,
                              i == 2 ? val_ei : empty, true, NULL);
            if (ret < 0) goto done;
            info_partial |= ret > 0;
        }
        result = 0;
        goto done; /* Read-only; never Register, Start, Set or restore. */
    }
    if (mode == RECEIVER_INFO) {
        const unsigned ids[] = {0x32, 0xbf};
        for (unsigned i = 0; i < 2; ++i) {
            int ret = command(&s, ids[i], NULL, empty, true, NULL);
            if (ret < 0) goto done;
            info_partial |= ret > 0;
        }
        result = 0;
        goto done; // No settings read, no Register, Start, Set or restoration.
    }
    for (unsigned i = 0; i < 3; ++i) if (get_value(&s, gets[i], &saved[i])) goto done;
    if ((!saved[0].lock_client && (saved[0].value < 1 || saved[0].value > 4)) ||
        saved[2].value < 1 || saved[2].value > 6) goto done;
    if (saved[0].lock_client) {
        /* Permit AFW only, keep NFW blocked and preserve privileged lock. */
        desired[0] = (struct setting){(saved[0].value & UINT64_C(4)) | 2, true};
        /* Native position/engine events need no NMEA reconfiguration. Device3
         * accepts NMEA Set but same-client post-Stop readback differed from
         * saved state; don't mutate that unproven setting just to get fixes. */
        desired[1] = saved[1];
        mask.value = 0x83;
        /* Modern LOC defaults unspecified clients to NFW, which we keep
         * locked. Explicitly register AFW; never use privileged bypass. */
        reg_ei = registration_ei;
    }
    fprintf(log, "LOC SAVED lock=%" PRIu64 " nmea=%" PRIu64 " mode=%" PRIu64 " lock-format=%s\n",
            saved[0].value, saved[1].value, saved[2].value, saved[0].lock_client ? "client-mask" : "legacy-enum");
    if (log_health(&s)) goto done;
    if (mode == QUERY) { result = 0; goto done; }
    /* A persistent provider must not restore a saved global configuration
     * over another owner's later choice. Its supported starting state is
     * the measured modern AFW-enabled/NFW-blocked standalone state. All
     * three settings must already match; leave legacy/manual changes to the
     * explicitly bounded diagnostic path. No best-effort unlock or Set. */
    if ((observer || s.rf_metrics) && (!saved[0].lock_client || saved[0].value != desired[0].value ||
                     saved[2].value != desired[2].value)) {
        fprintf(log, "LOC FAIL %s-configuration-not-ready\n", s.rf_metrics ? "rf-diagnostic" : "provider");
        goto done;
    }
    if (s.rf_metrics) mask.value |= UINT64_C(0x40000000000);
    if (observer && observer->take_time && observer->request_time) mask.value |= 0x10;
    for (unsigned i = 0; i < 3; ++i) {
        if (i == 2) {
            int ret = command(&s, 0x21, &mask, reg_ei, false, NULL);
            registered = s.attempted;
            if (ret) goto done;
        }
        if (saved[i].value != desired[i].value) {
            int ret = set_value(&s, sets[i], desired[i]);
            changed[i] = s.attempted;
            if (ret) goto done;
        }
    }
    s.events = 0;
    int start_result = command(&s, 0x22, &session, start_ei, false, NULL);
    started = s.attempted;
    if (start_result) goto done;
    s.delivering = true;
    fprintf(log, "LOC START acknowledged-no-fix-claim\n");
    if (observer && observer->started && !cancelled(&s)) observer->started(observer->ctx);
    if (observer && observer->take_time && observer->request_time && !cancelled(&s))
        observer->request_time(observer->ctx);
    deadline = now_ms() + (observer ? 1000 : run_ms);
    if (s.rf_metrics) {
        int ret = command(&s, 0x33, NULL, empty, true, NULL);
        criteria_result = ret < 0 ? "failed" : ret ? "refused" : "accepted";
        fprintf(log, "LOC CRITERIA result=%s\n", criteria_result);
        if (ret < 0) goto done; /* malformed/timeout preserves normal cleanup */
        info_partial = ret != 0;
    }
    while (observer || now_ms() < deadline) {
        if (observer && cancelled(&s)) { result = 0; goto done; }
        if (aid_time(&s)) {
            if (observer && cancelled(&s) && !s.gone && !s.log_failed) result = 0;
            goto done;
        }
        if (observer && cancelled(&s)) { result = 0; goto done; }
        struct loc_packet p;
        int r = receive(&s, &p, deadline);
        if (r < 0) {
            if (observer && cancelled(&s) && !s.gone && !s.log_failed) result = 0;
            goto done;
        }
        if (!r) {
            if (observer) { deadline = now_ms() + 1000; continue; }
            break;
        }
        const uint8_t *v[256]; unsigned n[256];
        if (tlvs(&p, v, n)) { fprintf(log, "LOC FAIL malformed-QMI\n"); goto done; }
        int late = late_time(&s, &p, v, n);
        if (late < 0) { fprintf(log, "LOC FAIL malformed-late-UTC\n"); goto done; }
        if (late) continue;
        if (p.data[0] != QMI_INDICATION) continue;
        if (event(&s, le16(p.data+3), v, n)) { fprintf(log, "LOC FAIL malformed-NMEA-or-event\n"); goto done; }
    }
    if (cancelled(&s) || (!s.events && !s.rf_reports)) {
        fprintf(log, "LOC FAIL %s\n", s.rf_metrics ? "no-active-events-or-cancelled" : "no-events-or-signal");
        goto done;
    }
    if (log_health(&s)) goto done;
    result = 0;
done:
    s.delivering = false;
    s.cleanup = true;
    if (started && command(&s, 0x23, &session, stop_ei, false, NULL)) {
        fprintf(log, "LOC CLEANUP_FAIL Stop session-may-remain-active\n"); cleanup_error = 1;
    }
    if (registered) {
        mask.value = 0;
        if (command(&s, 0x21, &mask, reg_ei, false, NULL)) {
            fprintf(log, "LOC CLEANUP_FAIL unregister\n"); cleanup_error = 1;
        }
    }
    for (int i = 2; i >= 0; --i) if (changed[i]) {
        struct setting observed = {0};
        int set_ret = set_value(&s, sets[i], saved[i]);
        int get_ret = get_value(&s, gets[i], &observed);
        if (set_ret || get_ret || observed.value != saved[i].value || observed.lock_client != saved[i].lock_client) {
            fprintf(log, "LOC CLEANUP_FAIL restore id=%04x observed=%" PRIu64 "\n", sets[i], observed.value); cleanup_error = 1;
        } else fprintf(log, "LOC RESTORED id=%04x value=%" PRIu64 "\n", sets[i], observed.value);
    }
    io->close(io->ctx);
    if (mode == DELETE_AIDING_ALL) {
        const bool failed = result || s.log_failed;
        fprintf(log, "LOC DELETE_AIDING_ALL reset=%s receiver-assistance-only no-restoration\n",
                !failed ? "ACCEPTED" : s.attempted ? "UNKNOWN" : "NOT_SENT");
        fprintf(log, "LOC CLOSED outcome=%s events=%u cleanup=NOT_REQUIRED\n",
                failed ? "FAIL" : "DELETE_AIDING_ALL", s.events);
        log_health(&s);
        return failed || s.log_failed;
    }
    if (s.rf_metrics) {
        fprintf(log, "LOC RF SUMMARY reports=%u capped=%s observation=%s criteria=%s",
                s.rf_reports, s.rf_truncated ? "yes" : "no",
                s.rf_reports ? "received" : "unavailable", criteria_result);
        if (s.rf_reports) fprintf(log, " first-tick=%" PRIu64 " last-tick=%" PRIu64 " tick-changed=%s",
                s.rf_first_tick, s.rf_last_tick, s.rf_tick_changed ? "yes" : "no");
        fputs(" (no-RF-health-claim)\n", log);
    }
    fprintf(log, "LOC CLOSED outcome=%s events=%u cleanup=%s\n", result || cleanup_error || s.log_failed ? "FAIL" : mode == ACQUISITION_CONFIG ? (info_partial ? "ACQUISITION_CONFIG_PARTIAL" : "ACQUISITION_CONFIG") : mode == RECEIVER_INFO ? (info_partial ? "RECEIVER_INFO_PARTIAL" : "RECEIVER_INFO") : mode == RF_METRICS ? (info_partial ? "RF_DIAGNOSTIC_PARTIAL" : "RF_DIAGNOSTIC") : mode == QUERY ? "QUERY" : observer ? "STOPPED" : "SESSION_EVENTS",
            s.events, cleanup_error ? "UNCONFIRMED" : "CONFIRMED");
    log_health(&s);
    if (observer && cleanup_error) return 2;
    return result || cleanup_error || s.log_failed || info_partial;
}
int loc_run(struct loc_io *io, FILE *log, unsigned run_ms, unsigned timeout,
            volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, run_ms, timeout, cancelled, SESSION, NULL, NULL, NULL); }
int loc_run_observed(struct loc_io *io, FILE *log, unsigned run_ms, unsigned timeout,
                     volatile sig_atomic_t *cancelled, loc_fix_callback cb, void *ctx)
{ return loc_execute(io, log, run_ms, timeout, cancelled, SESSION, cb, ctx, NULL); }
int loc_query(struct loc_io *io, FILE *log, unsigned timeout,
              volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, 0, timeout, cancelled, QUERY, NULL, NULL, NULL); }
int loc_follow(struct loc_io *io, FILE *log, const struct loc_observer *observer)
{
    if (!observer) { io->close(io->ctx); return 1; }
    return loc_execute(io, log, 0, 2000, NULL, SESSION, observer->fix, observer->ctx, observer);
}

int loc_receiver_info(struct loc_io *io, FILE *log, unsigned timeout,
                      volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, 0, timeout, cancelled, RECEIVER_INFO, NULL, NULL, NULL); }
int loc_rf_metrics(struct loc_io *io, FILE *log, unsigned run_ms, unsigned timeout,
                   volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, run_ms, timeout, cancelled, RF_METRICS, NULL, NULL, NULL); }

int loc_acquisition_config(struct loc_io *io, FILE *log, unsigned timeout,
                           volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, 0, timeout, cancelled, ACQUISITION_CONFIG, NULL, NULL, NULL); }

int loc_delete_aiding_all(struct loc_io *io, FILE *log, unsigned timeout,
                          volatile sig_atomic_t *cancelled)
{ return loc_execute(io, log, 0, timeout, cancelled, DELETE_AIDING_ALL, NULL, NULL, NULL); }

/* SPDX-License-Identifier: Apache-2.0 */
#ifndef BENGAL_LOC_H
#define BENGAL_LOC_H
#include <stdint.h>
#include <stddef.h>
#include <stdio.h>
#include <signal.h>
#include "position.h"
#include "satellite.h"
#include "time-aiding.h"
#ifdef __cplusplus
extern "C" {
#endif
#define LOC_CAP 16384 /* full expanded SV + signal/RF-loss arrays */
struct loc_packet {
    uint32_t type, node, port, service, instance, client;
    size_t length;
    uint8_t data[LOC_CAP];
};
/* recv: 1 packet, 0 deadline, negative error. All operations bounded.
 * client identifies the local socket, not a nonexistent QMUX header field. */
struct loc_io {
    void *ctx;
    uint32_t client;
    int (*lookup)(void *);
    int (*send)(void *, uint32_t, uint32_t, const void *, size_t);
    int (*recv)(void *, struct loc_packet *, unsigned);
    void (*close)(void *);
};
int loc_run(struct loc_io *, FILE *, unsigned run_ms, unsigned timeout_ms,
            volatile sig_atomic_t *cancelled);
/* Callback runs on this session's thread, only after Start acknowledgement
 * and before cleanup/cancellation. It must return promptly and must not call
 * back into the LOC session. Fix pointer is valid for the call only.
 * UTC duplicates/out-of-order reports are suppressed within one session. */
typedef void (*loc_fix_callback)(void *, const struct loc_fix *);
int loc_run_observed(struct loc_io *, FILE *, unsigned run_ms, unsigned timeout_ms,
                     volatile sig_atomic_t *cancelled, loc_fix_callback, void *);
int loc_query(struct loc_io *, FILE *, unsigned timeout_ms,
              volatile sig_atomic_t *cancelled);
/* Read-only discovery + revision/constellation Gets only. Return nonzero if
 * any query is unsupported, malformed, refused or incomplete. No fallback. */
int loc_receiver_info(struct loc_io *, FILE *, unsigned timeout_ms,
                      volatile sig_atomic_t *cancelled);
/* Explicit finite diagnostic: RF event bit42 + same-client GetFixCriteria.
 * Requires already-ready modern standalone configuration; never sends Sets.
 * No provider callbacks or fabricated aiding; missing telemetry stays unknown. */
int loc_rf_metrics(struct loc_io *, FILE *, unsigned run_ms, unsigned timeout_ms,
                   volatile sig_atomic_t *cancelled);
/* Read-only D5/B7/D8 diagnostic; return1 for absent/unsupported partial data
 * or failure, distinguished by the final log outcome. Never starts LOC. */
int loc_acquisition_config(struct loc_io *, FILE *, unsigned, volatile sig_atomic_t *);
/* Explicit one-shot receiver assistance reset (old OEM LOC0x44 deleteAll).
 * Requires exclusive command ownership. No retry, Start/Register/Set or
 * restoration. Return0 only after successful response AND completion; any
 * failure after a send attempt leaves reset outcome unknown. Not raw NV/EFS
 * or factory-calibration restoration. Lookup/command each bounded <=2s. */
int loc_delete_aiding_all(struct loc_io *, FILE *, unsigned, volatile sig_atomic_t *);
int loc_qrtr_open(struct loc_io *);
/* Long-lived native session for a single owner. Hooks run on the caller's
 * transport thread and must not block or re-enter LOC. cancelled must be
 * thread-safe (e.g. read a C++ atomic); volatile sig_atomic_t is NOT a
 * cross-thread cancellation primitive. No automatic retry on modem loss.
 * Commands/cleanup remain <=2s each; cancellation is checked every <=100ms.
 * Modern client-mask protocol only, with NO NMEA/configuration mutation. */
struct loc_observer {
    void *ctx;
    int (*cancelled)(void *);
    void (*started)(void *);
    loc_fix_callback fix;
    unsigned interval_ms;
    void (*satellites)(void *, const struct loc_sv_report *);
    /* Optional pair enables UTC request events (mask bit0x10). take_time
     * removes at most one sample, returns1 if available,0 otherwise. The
     * owner validates/ages it at send, using BOOTTIME, never wall clock.
     * Requests occur at acknowledged Start and modem event0x28. */
    int (*take_time)(void *, struct loc_time_sample *);
    void (*request_time)(void *);
    /* 0 accepted by both response+indication,1 refused,2 invalid/expired,
     * 3 completion timeout: UTC disabled for this client, tracking continues.
     * Transport/malformed failures still end the session with cleanup. */
    void (*time_result)(void *, int);
    /* Optional coordinate-free metadata, validated on the same event seam.
     * Only after Start acknowledgement and before cancellation/cleanup, like
     * satellites. Absent list means count is unavailable (count argument 0),
     * not an empty report. Engine state is the modem's uninterpreted u32.
     * SV metadata precedes the corresponding optional satellites callback. */
    void (*sv_metadata)(void *, int list_present, unsigned count);
    void (*engine_state)(void *, uint32_t raw_state);

};
/* Return0 normal cancellation+confirmed cleanup,1 failed operation with
 * confirmed cleanup,2 unconfirmed cleanup (caller must not auto-retry). */
int loc_follow(struct loc_io *, FILE *, const struct loc_observer *);
#ifdef __cplusplus
}
#endif
#endif

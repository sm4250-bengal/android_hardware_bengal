/* SPDX-License-Identifier: Apache-2.0 */
#ifndef BENGAL_TIME_AIDING_H
#define BENGAL_TIME_AIDING_H
#include <stdint.h>
#include <time.h>
/* AIDL UTC Unix milliseconds and Android elapsedRealtime (CLOCK_BOOTTIME).
 * Match framework NtpNetworkTimeHelper's strict <24h cached-sample limit. */
#define LOC_TIME_MAX_AGE_MS INT64_C(86400000)
struct loc_time_sample { int64_t utc_ms, reference_ms; int32_t uncertainty_ms; };
static inline int64_t loc_boottime_ms(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_BOOTTIME, &t)) return -1;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static inline int loc_time_adjust(const struct loc_time_sample *s, int64_t now, uint64_t *utc)
{
    if (s->utc_ms <= 0 || s->reference_ms < 0 || s->uncertainty_ms < 0 || now < s->reference_ms)
        return -1;
    int64_t age = now - s->reference_ms;
    if (age >= LOC_TIME_MAX_AGE_MS || s->utc_ms > INT64_MAX - age) return -1;
    *utc = (uint64_t)(s->utc_ms + age);
    return 0;
}
#endif

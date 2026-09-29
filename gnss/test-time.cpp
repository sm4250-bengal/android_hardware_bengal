/* SPDX-License-Identifier: Apache-2.0 */
#include "TimeAiding.h"
#include <cassert>
#include <cstdio>
using bengal::gnss::TimeAiding;
int main() {
    loc_time_sample sample{1700000000000, 1000, 27}, result{};
    uint64_t utc = 0;
    assert(!loc_time_adjust(&sample, 6000, &utc) && utc == 1700000005000);
    // Android elapsedRealtime includes suspend. No walltime/GPS leap shifts.
    assert(!loc_time_adjust(&sample, 3601000, &utc) && utc == 1700003600000);
    assert(loc_time_adjust(&sample, 999, &utc));
    assert(loc_time_adjust(&sample, -1, &utc));
    assert(!loc_time_adjust(&sample, 1000 + LOC_TIME_MAX_AGE_MS - 1, &utc));
    assert(loc_time_adjust(&sample, 1000 + LOC_TIME_MAX_AGE_MS, &utc));
    auto bad = sample; bad.utc_ms=0; assert(loc_time_adjust(&bad, 6000, &utc));
    bad=sample; bad.reference_ms=-1; assert(loc_time_adjust(&bad, 6000, &utc));
    bad=sample; bad.uncertainty_ms=-1; assert(loc_time_adjust(&bad, 6000, &utc));
    bad=sample; bad.utc_ms=INT64_MAX; assert(loc_time_adjust(&bad, 6000, &utc));
    TimeAiding pending;
    assert(pending.offer(1, sample, 6000));
    // Invalid replacement must not destroy the valid pending sample.
    assert(!pending.offer(1, bad, 6000));
    assert(pending.take(1, result) && result.utc_ms == sample.utc_ms);
    assert(!pending.take(1, result));
    // Latest wins; bounded even if Binder produces faster than LOC drains.
    for (int i=0; i<10000; ++i) {
        sample.utc_ms++; assert(pending.offer(1, sample, 6000));
    }
    assert(pending.take(1, result) && result.utc_ms == sample.utc_ms);
    assert(!pending.take(1, result));
    assert(pending.offer(1, sample, 6000));
    assert(!pending.take(2, result)); // callback replacement cannot inherit
    assert(!pending.take(1, result));
    assert(pending.offer(2, sample, 6000));
    pending.clear(); assert(!pending.take(2, result)); // death/close
    assert(pending.offer(3, sample, 6000));
    // Session generations deliberately are not sample ownership tokens.
    assert(pending.take(3, result)); // ordinary stop/start, same registration
    assert(loc_time_adjust(&result, 1000 + LOC_TIME_MAX_AGE_MS, &utc)); // expires while queued
    puts("PASS UTC conversion, bounds, latest-only queue and registration lifetime");
}

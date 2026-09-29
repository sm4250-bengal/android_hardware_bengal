/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "time-aiding.h"
#include <optional>
namespace bengal::gnss {
// Caller holds State::mutex. One latest sample, owned by callback registration,
// not session generation: ordinary stop/start preserves a still-valid sample.
class TimeAiding {
    std::optional<loc_time_sample> sample_;
    uint64_t registration_ = 0;
public:
    void clear() { sample_.reset(); }
    bool offer(uint64_t registration, loc_time_sample sample, int64_t now) {
        uint64_t utc;
        if (loc_time_adjust(&sample, now, &utc)) return false;
        registration_ = registration;
        sample_ = sample;
        return true;
    }
    bool take(uint64_t registration, loc_time_sample& sample) {
        if (!sample_) return false;
        auto pending = *sample_;
        clear();
        if (registration != registration_) return false;
        sample = pending;
        return true;
    }
};
} // namespace bengal::gnss

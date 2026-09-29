// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <array>
#include <cstdint>
#include <string>

namespace bengal::power::client {
constexpr int64_t kPeriodUs = 10000;
constexpr int64_t kMaxGapUs = 50000;
constexpr int64_t kBaselineUs = 500000;
constexpr int64_t kObserveUs = 1500000;
constexpr int32_t kHintMs = 250;
constexpr std::array<int, 2> kPolicies{0, 4};
constexpr std::array<int, 2> kTargets{864000, 902400};
constexpr std::array<int, 2> kCaps{1804800, 2016000};

inline bool validBootId(const std::string& value) {
    if (value.size() != 36) return false;
    for (size_t i = 0; i < value.size(); ++i) {
        if (i == 8 || i == 13 || i == 18 || i == 23) {
            if (value[i] != '-') return false;
        } else if (!((value[i] >= '0' && value[i] <= '9') ||
                     (value[i] >= 'a' && value[i] <= 'f'))) return false;
    }
    return value != "00000000-0000-0000-0000-000000000000";
}

inline bool identityMatches(const std::string& expected, const std::string& actual,
                            const std::string& device, const std::string& serial,
                            const std::string& release, const std::string& context,
                            unsigned uid, unsigned euid, const std::string& bootCompleted) {
    return validBootId(expected) && actual == expected && device == "bengal3" &&
           serial == "16cc8af" && release == "6.18.32-4k-gd84cfa4479d9" &&
           context == "u:r:su:s0" && uid == 0 && euid == 0 && bootCompleted == "1";
}

struct Policy {
    int min = 0;
    int max = 0;
    std::string governor;
    bool targetAvailable = false;
};
struct Sample {
    // CLOCK_MONOTONIC offsets relative to the phase's start, bounding sequential reads.
    int64_t beginUs = 0;
    int64_t endUs = 0;
    std::array<Policy, 2> policies;
};

inline bool normalControls(const Sample& sample) {
    for (size_t i = 0; i < 2; ++i) {
        const auto& p = sample.policies[i];
        if (p.max != kCaps[i] || p.governor != "schedutil" || !p.targetAvailable) return false;
    }
    return true;
}
inline bool bothIdle(const Sample& sample) {
    return sample.policies[0].min == 300000 && sample.policies[1].min == 300000;
}
inline bool bothRaised(const Sample& sample) {
    return sample.policies[0].min == kTargets[0] && sample.policies[1].min == kTargets[1];
}

// Conservative observation, not attribution: another client can issue identical hints.
class Observation {
  public:
    bool baseline(const Sample& sample) {
        if (!timely(sample, kBaselineUs + kMaxGapUs) || !normalControls(sample) ||
            !bothIdle(sample)) return fail("BASELINE_NOT_STABLE");
        if (firstUs_ < 0) firstUs_ = sample.beginUs;
        baselineReady_ = sample.endUs - firstUs_ >= kBaselineUs;
        return true;
    }
    bool baselineReady() const { return baselineReady_ && error_.empty(); }
    void beginObservation() { previousEndUs_ = -1; }
    bool observe(const Sample& sample) {
        if (!baselineReady()) return fail("BASELINE_NOT_READY");
        if (!timely(sample, kObserveUs) || !normalControls(sample))
            return fail("CONTAMINATED_OR_TIMING_GAP");
        for (size_t i = 0; i < 2; ++i) {
            const int floor = sample.policies[i].min;
            if (floor != 300000 && floor != kTargets[i]) return fail("FOREIGN_FLOOR");
        }
        if (restoredUs_ >= 0 && !bothIdle(sample)) return fail("RISE_AFTER_RESTORE");
        if (raisedUs_ < 0 && bothRaised(sample)) raisedUs_ = sample.endUs;
        if (raisedUs_ >= 0 && restoredUs_ < 0 && bothIdle(sample)) restoredUs_ = sample.endUs;
        return true;
    }
    const char* result() const {
        if (!error_.empty()) return error_.c_str();
        if (!baselineReady_) return "BASELINE_NOT_READY";
        if (raisedUs_ < 0) return "NO_OBSERVED_BOTH_RISE";
        if (restoredUs_ < 0) return "NO_AUTONOMOUS_BOTH_RESTORE_OBSERVED";
        return "OBSERVED_BOTH_RISE_AND_RESTORE";
    }
    bool observed() const { return error_.empty() && raisedUs_ >= 0 && restoredUs_ >= 0; }
    bool finish(int64_t nowUs) {
        if (nowUs < kObserveUs || nowUs > kObserveUs + kMaxGapUs ||
            previousEndUs_ < kObserveUs - kMaxGapUs || nowUs - previousEndUs_ > kMaxGapUs)
            return fail("OBSERVATION_DEADLINE_OR_FINAL_GAP");
        return error_.empty();
    }
    int64_t raisedUs() const { return raisedUs_; }
    int64_t restoredUs() const { return restoredUs_; }
  private:
    bool fail(const char* reason) {
        if (error_.empty()) error_ = reason;
        return false;
    }
    bool timely(const Sample& sample, int64_t bound) {
        if (!error_.empty() || sample.beginUs < 0 || sample.endUs < sample.beginUs ||
            sample.endUs > bound || sample.endUs - sample.beginUs > kMaxGapUs ||
            (previousEndUs_ < 0 ? sample.endUs > kMaxGapUs :
             sample.beginUs < previousEndUs_ || sample.endUs - previousEndUs_ > kMaxGapUs))
            return false;
        previousEndUs_ = sample.endUs;
        return true;
    }
    int64_t firstUs_ = -1;
    int64_t previousEndUs_ = -1;
    int64_t raisedUs_ = -1;
    int64_t restoredUs_ = -1;
    bool baselineReady_ = false;
    std::string error_;
};
}  // namespace bengal::power::client

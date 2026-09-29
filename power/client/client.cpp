// SPDX-License-Identifier: Apache-2.0
#include "Observation.h"

#include <aidl/android/hardware/power/IPower.h>
#include <android/binder_manager.h>
#include <sys/system_properties.h>
#include <sys/utsname.h>
#include <unistd.h>

#include <chrono>
#include <cstdio>
#include <fstream>
#include <sstream>
#include <thread>

namespace p = aidl::android::hardware::power;
namespace c = bengal::power::client;
using Clock = std::chrono::steady_clock;

static int64_t elapsedUs(Clock::time_point start) {
    return std::chrono::duration_cast<std::chrono::microseconds>(Clock::now() - start).count();
}
static bool readText(const std::string& path, std::string* out) {
    std::ifstream input(path);
    if (!input.is_open()) return false;
    *out = std::string(std::istreambuf_iterator<char>(input), std::istreambuf_iterator<char>());
    if (input.bad() || out->empty() || out->size() > 4096) return false;
    while (!out->empty() && (out->back() == '\n' || out->back() == '\0')) out->pop_back();
    return !out->empty();
}
static bool number(const std::string& path, int* out) {
    std::string text;
    if (!readText(path, &text)) return false;
    std::istringstream stream(text);
    std::string trailing;
    return bool(stream >> *out) && *out > 0 && !(stream >> trailing);
}
static std::string property(const char* name) {
    char value[PROP_VALUE_MAX]{};
    return __system_property_get(name, value) > 0 ? value : "";
}
static bool identity(const std::string& expected) {
    std::string boot, context;
    utsname kernel{};
    return readText("/proc/sys/kernel/random/boot_id", &boot) &&
           readText("/proc/self/attr/current", &context) && uname(&kernel) == 0 &&
           c::identityMatches(expected, boot, property("ro.product.device"),
                              property("ro.boot.serialno"), kernel.release, context,
                              getuid(), geteuid(), property("sys.boot_completed"));
}
static bool sample(Clock::time_point start, const char* phase, c::Sample* out) {
    out->beginUs = elapsedUs(start);
    for (size_t i = 0; i < 2; ++i) {
        const auto base = "/sys/devices/system/cpu/cpufreq/policy" +
                          std::to_string(c::kPolicies[i]) + "/";
        auto& policy = out->policies[i];
        std::string frequencies;
        if (!number(base + "scaling_min_freq", &policy.min) ||
            !number(base + "scaling_max_freq", &policy.max) ||
            !readText(base + "scaling_governor", &policy.governor) ||
            !readText(base + "scaling_available_frequencies", &frequencies)) return false;
        std::istringstream stream(frequencies);
        int frequency;
        while (stream >> frequency) {
            if (frequency <= 0) return false;
            if (frequency == c::kTargets[i]) policy.targetAvailable = true;
        }
        if (!stream.eof()) return false;
    }
    out->endUs = elapsedUs(start);
    std::printf("ROW %s begin_us=%lld end_us=%lld min0=%d max0=%d gov0=%s opp0=%d "
                "min4=%d max4=%d gov4=%s opp4=%d\n", phase,
                static_cast<long long>(out->beginUs), static_cast<long long>(out->endUs),
                out->policies[0].min, out->policies[0].max, out->policies[0].governor.c_str(),
                out->policies[0].targetAvailable, out->policies[1].min, out->policies[1].max,
                out->policies[1].governor.c_str(), out->policies[1].targetAvailable);
    return true;
}
static int stop(const char* reason, bool submitted = false) {
    std::printf("RESULT NOT_PROVED reason=%s hint_submitted=%d retries=0\n", reason, submitted);
    return 1;
}

int main(int argc, char** argv) {
    // Flush each row even if the lead's external watchdog kills this client.
    setvbuf(stdout, nullptr, _IOLBF, 0);
    if (argc != 3 || std::string(argv[1]) != "--expected-boot-id" || !c::validBootId(argv[2])) {
        std::fprintf(stderr, "usage: bengal-power-hint-client --expected-boot-id UUID\n"
                             "Requires lead-owned outer 5 s watchdog; never run in a retry loop.\n");
        return 64;
    }
    const std::string expected = argv[2];
    if (!identity(expected)) return stop("IDENTITY_UID_OR_CONTEXT_REFUSED");
    std::printf("IDENTITY boot_id=%s device=bengal3 serial=16cc8af "
                "release=6.18.32-4k-gd84cfa4479d9 context=u:r:su:s0 uid=0\n", expected.c_str());
    // checkService avoids waiting for service registration. Binder queries themselves
    // can still block; only the external watchdog supplies a process-level bound.
    auto power = p::IPower::fromBinder(ndk::SpAIBinder(AServiceManager_checkService(
            "android.hardware.power.IPower/default")));
    if (!power) return stop("SERVICE_MISSING");
    int32_t version = 0;
    if (!power->getInterfaceVersion(&version).isOk()) return stop("VERSION_QUERY_FAILED");
    if (version != 6) return stop("INTERFACE_NOT_V6");
    bool supported = false;
    if (!power->isBoostSupported(p::Boost::INTERACTION, &supported).isOk())
        return stop("SUPPORT_QUERY_FAILED");
    if (!supported) return stop("INTERACTION_UNSUPPORTED");
    p::SupportInfo info;
    if (!power->getSupportInfo(&info).isOk()) return stop("SUPPORT_INFO_QUERY_FAILED");
    if (info.boosts != (int64_t{1} << static_cast<int>(p::Boost::INTERACTION)) ||
        info.usesSessions || info.modes != 0) return stop("SUPPORT_INFO_MISMATCH");
    std::puts("SUPPORT interface_version=6 sole_boost=INTERACTION modes=0 sessions=0");

    c::Observation observation;
    const auto baselineStart = Clock::now();
    std::printf("PHASE baseline monotonic_origin_us=%lld\n", static_cast<long long>(
            std::chrono::duration_cast<std::chrono::microseconds>(
                    baselineStart.time_since_epoch()).count()));
    auto next = baselineStart;
    while (!observation.baselineReady()) {
        c::Sample row;
        if (!sample(baselineStart, "baseline", &row)) return stop("BASELINE_READ_FAILED");
        if (!observation.baseline(row)) return stop(observation.result());
        if (observation.baselineReady()) break;
        next += std::chrono::microseconds(c::kPeriodUs);
        std::this_thread::sleep_until(next);
    }
    if (!identity(expected)) return stop("PRE_SUBMIT_IDENTITY_CHANGED");
    // Last read after identity checks closes a long scheduling/read gap before submission.
    c::Sample last;
    if (!sample(baselineStart, "pre_submit", &last)) return stop("PRE_SUBMIT_READ_FAILED");
    if (!observation.baseline(last)) return stop(observation.result());

    observation.beginObservation();
    const auto hintStart = Clock::now();
    // EXACTLY ONE state-changing Binder call. ONEWAY isOk means submission ONLY.
    const auto status = power->setBoost(p::Boost::INTERACTION, c::kHintMs);
    if (!status.isOk()) {
        std::puts("BINDER submission=FAILED admission=UNKNOWN no_retry=1");
        return stop("SUBMISSION_FAILED");
    }
    std::printf("PHASE after_hint monotonic_origin_us=%lld\n", static_cast<long long>(
            std::chrono::duration_cast<std::chrono::microseconds>(
                    hintStart.time_since_epoch()).count()));
    std::printf("HINT SUBMITTED_ONLY type=INTERACTION duration_ms=250 "
                "return_us=%lld admission=UNKNOWN\n", static_cast<long long>(elapsedUs(hintStart)));
    next = hintStart;
    while (elapsedUs(hintStart) < c::kObserveUs) {
        c::Sample row;
        if (!sample(hintStart, "after_hint", &row)) return stop("OBSERVATION_READ_FAILED", true);
        if (!observation.observe(row)) return stop(observation.result(), true);
        next += std::chrono::microseconds(c::kPeriodUs);
        std::this_thread::sleep_until(next);
    }
    // A scheduling stall after the final row must not turn a partial trace into success.
    if (!observation.finish(elapsedUs(hintStart))) return stop(observation.result(), true);
    if (!identity(expected)) return stop("POST_OBSERVATION_IDENTITY_CHANGED", true);
    if (!observation.observed()) return stop(observation.result(), true);
    std::printf("RESULT OBSERVED_BOTH_RISE_AND_RESTORE first_both_rise_us=%lld "
                "first_both_restore_us=%lld hint_submitted=1 retries=0 "
                "causal_attribution=NOT_PROVED hard_realtime=NO\n",
                static_cast<long long>(observation.raisedUs()),
                static_cast<long long>(observation.restoredUs()));
    return 0;
}

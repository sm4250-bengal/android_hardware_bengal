// SPDX-License-Identifier: Apache-2.0
#include "Observation.h"

#include <cassert>
#include <cstdio>
#include <cstring>

namespace c = bengal::power::client;
static c::Sample row(int64_t us, int min0 = 300000, int min4 = 300000) {
    return {us, us + 100, {{{min0, c::kCaps[0], "schedutil", true},
                           {min4, c::kCaps[1], "schedutil", true}}}};
}
static c::Observation baseline() {
    c::Observation o;
    for (int64_t us = 0; us <= c::kBaselineUs; us += c::kPeriodUs) assert(o.baseline(row(us)));
    assert(o.baselineReady());
    o.beginObservation();
    return o;
}
static c::Observation trace(int kind) {
    auto o = baseline();
    for (int64_t us = 0; us < c::kObserveUs; us += c::kPeriodUs) {
        bool raised = us >= 10000 && (us < 270000 || kind == 1);
        if (kind == 2) raised = false;
        assert(o.observe(row(us, raised ? c::kTargets[0] : 300000,
                            raised ? c::kTargets[1] : 300000)));
    }
    assert(o.finish(c::kObserveUs));
    return o;
}
int main() {
    const std::string id = "99199b07-86ae-4de5-b0a3-9d386b725c10";
    auto identity = [&](std::string boot, std::string device, std::string serial,
                        std::string release, std::string context, unsigned uid,
                        unsigned euid, std::string completed) {
        return c::identityMatches(id, boot, device, serial, release, context, uid, euid, completed);
    };
    const std::string release = "6.18.32-4k-gd84cfa4479d9";
    assert(identity(id, "bengal3", "16cc8af", release, "u:r:su:s0", 0, 0, "1"));
    assert(!identity("11199b07-86ae-4de5-b0a3-9d386b725c10", "bengal3", "16cc8af", release, "u:r:su:s0", 0, 0, "1"));
    assert(!identity(id, "bengal2", "16cc8af", release, "u:r:su:s0", 0, 0, "1"));
    assert(!identity(id, "bengal3", "other", release, "u:r:su:s0", 0, 0, "1"));
    assert(!identity(id, "bengal3", "16cc8af", "6.18.32-old", "u:r:su:s0", 0, 0, "1"));
    assert(!identity(id, "bengal3", "16cc8af", release, "u:r:shell:s0", 0, 0, "1"));
    assert(!identity(id, "bengal3", "16cc8af", release, "u:r:su:s0", 2000, 0, "1"));
    assert(!identity(id, "bengal3", "16cc8af", release, "u:r:su:s0", 0, 2000, "1"));
    assert(!identity(id, "bengal3", "16cc8af", release, "u:r:su:s0", 0, 0, "0"));
    assert(!c::validBootId(""));
    assert(!c::validBootId("00000000-0000-0000-0000-000000000000"));
    assert(!c::validBootId("99199b07-86ae-4de5-b0a3-9d386b725c1Z"));
    std::puts("PASS exact identity / UUID / UID / su context refusal controls");
    {
        c::Observation o;
        assert(o.baseline(row(0)));
        assert(!o.baselineReady());
        assert(!o.observe(row(10000, c::kTargets[0], c::kTargets[1])));
    }
    for (int change = 0; change < 6; ++change) {
        c::Observation o;
        auto bad = row(0);
        if (change == 0) bad.policies[0].min = c::kTargets[0];
        if (change == 1) bad.policies[1].min = 400000;
        if (change == 2) bad.policies[0].max = 1800000;
        if (change == 3) bad.policies[1].governor = "performance";
        if (change == 4) bad.policies[0].targetAvailable = false;
        if (change == 5) bad.endUs = 60000;
        assert(!o.baseline(bad));
        assert(!o.baselineReady());
    }
    {
        c::Observation o;
        assert(o.baseline(row(0)));
        assert(!o.baseline(row(500000))); // two samples do not prove 500 ms stability
    }
    std::puts("PASS baseline duration, sample continuity and contamination controls");
    auto good = trace(0);
    assert(good.observed());
    assert(good.raisedUs() == 10100 && good.restoredUs() == 270100);
    assert(std::strcmp(good.result(), "OBSERVED_BOTH_RISE_AND_RESTORE") == 0);
    auto stranded = trace(1);
    assert(!stranded.observed());
    assert(std::strcmp(stranded.result(), "NO_AUTONOMOUS_BOTH_RESTORE_OBSERVED") == 0);
    auto absent = trace(2);
    assert(!absent.observed());
    assert(std::strcmp(absent.result(), "NO_OBSERVED_BOTH_RISE") == 0);
    std::puts("PASS rise/restore positive and distinct no-rise/no-restore controls");
    for (int change = 0; change < 7; ++change) {
        auto o = baseline();
        auto bad = row(0);
        if (change == 0) bad.policies[0].min = 800000;
        if (change == 1) bad.policies[1].max = 1900000;
        if (change == 2) bad.policies[1].governor = "powersave";
        if (change == 3) bad.policies[1].targetAvailable = false;
        if (change == 4) bad.beginUs = -1;
        if (change == 5) bad.endUs = 60000;
        if (change == 6) bad.beginUs = 200;
        assert(!o.observe(bad));
        assert(!o.observe(row(10000, c::kTargets[0], c::kTargets[1])));
        assert(!o.observed());
    }
    {
        auto o = baseline();
        assert(o.observe(row(0, c::kTargets[0], 300000)));
        assert(o.observe(row(10000, 300000, c::kTargets[1])));
        assert(o.observe(row(20000)));
        assert(!o.observed()); // disjoint rises never establish BOTH raised
    }
    {
        auto o = baseline();
        assert(o.observe(row(0, c::kTargets[0], c::kTargets[1])));
        assert(o.observe(row(10000, 300000, c::kTargets[1])));
        assert(!o.observed()); // one restored is not BOTH restored
        assert(o.observe(row(20000)));
        assert(o.observed());
        assert(!o.observe(row(30000, c::kTargets[0], 300000)));
        assert(!o.observed());
    }
    {
        auto o = baseline();
        assert(o.observe(row(0)));
        assert(!o.observe(row(60000)));
    }
    {
        auto o = baseline();
        assert(o.observe(row(0)));
        assert(!o.observe(row(0))); // overlapping/backwards sample timestamps
    }
    {
        auto o = baseline();
        assert(o.observe(row(0, c::kTargets[0], c::kTargets[1])));
        assert(o.observe(row(10000)));
        assert(!o.finish(c::kObserveUs)); // truncated trace may not report success
        assert(!o.observed());
    }
    assert(!good.finish(c::kObserveUs + c::kMaxGapUs + 1));
    assert(!good.observed());
    std::puts("PASS post-hint contamination, sticky failure, partial/renewed floors, final gap");
}

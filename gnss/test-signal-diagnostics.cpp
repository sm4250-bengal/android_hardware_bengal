/* SPDX-License-Identifier: Apache-2.0 */
#include "SignalDiagnostics.h"
#include "Satellite.h"
#include <cassert>
#include <cstdio>
using namespace bengal::gnss;
static void contains(const std::string& s, const char* text) {
    assert(s.find(text) != std::string::npos);
}
int main() {
    SignalDiagnostics d;
    d.startAck();
    auto first = d.summary(100);
    assert(first);
    contains(*first, "start-ack=1 engine-events=0 engine-changes=0 engine-raw=unknown");
    contains(*first, "latest-list-count=unknown");
    d.engine(1); d.engine(1); d.engine(UINT32_MAX);
    loc_sv_report native{};
    d.sv(false, 0); // Valid altitude-only event must not create an Android report.
    d.sv(true, 0);
    auto converted = satellites(native);
    assert(converted && converted->empty());
    d.android(0, converted->size());
    native.count = 1; // Real list, missing mandatory Android fields.
    d.sv(true, native.count);
    converted = satellites(native);
    assert(!converted);
    d.android(native.count, std::nullopt);
    native.count = 2;
    native.sv[1].valid = 255 | LOC_SV_BASEBAND_CN0;
    native.sv[1].system = 1; native.sv[1].id = 7;
    native.sv[1].cn0 = 20; native.sv[1].baseband_cn0 = 18;
    native.sv[1].elevation = 45; native.sv[1].azimuth = 123;
    d.sv(true, native.count);
    converted = satellites(native);
    assert(converted && converted->size() == 1);
    d.android(native.count, converted->size());
    for (uint64_t ms = 100; ms < 30100; ++ms) assert(!d.summary(ms));
    assert(!d.summary(99)); // Clock regression cannot trigger a log storm.
    auto next = d.summary(30100);
    assert(next);
    contains(*next, "engine-events=3 engine-changes=1 engine-raw=4294967295");
    contains(*next, "sv-indications=4 list-absent=1 list-empty=1 list-nonempty=2 latest-list-count=2");
    contains(*next, "android-converted-reports=2 android-converted-entries=1 android-suppressed-reports=1 android-omitted-entries=2");
    d.sv(false, 0);
    auto final = d.summary(30101, true);
    assert(final); contains(*final, "final=1"); contains(*final, "latest-list-count=unknown");
    assert(!d.summary(60100) && !d.summary(60100, true));
    SignalDiagnostics failed;
    final = failed.summary(0, true);
    assert(final); contains(*final, "start-ack=0"); contains(*final, "engine-raw=unknown");
    // A high event rate and oscillating engine value cannot bypass the bound.
    SignalDiagnostics busy;
    unsigned logs = 0;
    for (unsigned ms = 0; ms <= 90000; ++ms) {
        busy.engine(ms % 2); busy.sv(false, 0);
        logs += busy.summary(ms).has_value();
    }
    assert(logs == 4 && busy.summary(90000, true));
    puts("PASS reception diagnostics: unknown/empty, real adapter counts, raw engine, rate/final bounds");
}

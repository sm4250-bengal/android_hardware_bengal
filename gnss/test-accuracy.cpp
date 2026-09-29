/* SPDX-License-Identifier: Apache-2.0 */
#include "Accuracy.h"
#include <cassert>
#include <cfloat>
#include <cstdio>
using namespace bengal::gnss;
int main() {
    loc_fix f{};
    assert(!horizontalAccuracy68(f) && !verticalAccuracy68(f));
    f.fields = LOC_FIX_HORIZONTAL_CIRCLE;
    f.horizontal_circle_m = 10;
    assert(std::abs(*horizontalAccuracy68(f) - 10.72) < 1e-9);
    f.fields |= LOC_FIX_HORIZONTAL_CONFIDENCE | LOC_FIX_HORIZONTAL_ELLIPSE;
    f.horizontal_confidence = 99; f.horizontal_minor_m = 1; f.horizontal_major_m = 2;
    assert(std::abs(*horizontalAccuracy68(f) - 10.72) < 1e-9);
    for (float bad : {0.f, -1.f, INFINITY, NAN, FLT_MAX}) {
        f.horizontal_circle_m = bad;
        assert(!horizontalAccuracy68(f));
    }
    f.fields = LOC_FIX_HORIZONTAL_ELLIPSE | LOC_FIX_HORIZONTAL_CONFIDENCE;
    f.horizontal_minor_m = 3; f.horizontal_major_m = 5; f.horizontal_confidence = 68;
    assert(*horizontalAccuracy68(f) == 5);
    f.horizontal_confidence = 39;
    assert(*horizontalAccuracy68(f) > 7.58 && *horizontalAccuracy68(f) < 7.60);
    f.horizontal_confidence = 95; assert(*horizontalAccuracy68(f) == 5);
    f.horizontal_minor_m = 6; assert(!horizontalAccuracy68(f));
    f.horizontal_minor_m = 3;
    for (unsigned bad : {0u, 100u, 255u}) { f.horizontal_confidence = bad; assert(!horizontalAccuracy68(f)); }
    f.fields = LOC_FIX_ALTITUDE | LOC_FIX_VERTICAL_UNCERTAINTY | LOC_FIX_VERTICAL_CONFIDENCE;
    f.vertical_confidence = 68; f.vertical_uncertainty_m = 7;
    assert(*verticalAccuracy68(f) == 7);
    f.vertical_confidence = 95; assert(*verticalAccuracy68(f) == 7);
    f.vertical_confidence = 39; assert(!verticalAccuracy68(f));
    f.vertical_confidence = 68; f.fields &= ~LOC_FIX_ALTITUDE; assert(!verticalAccuracy68(f));
    puts("PASS: actual Android accuracy conversion, circle/ellipse precedence and invalid values");
}

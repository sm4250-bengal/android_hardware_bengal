/* SPDX-License-Identifier: Apache-2.0 */
#include "position.h"
#include <math.h>
#include <string.h>
#include <limits.h>
#if __BYTE_ORDER__ != __ORDER_LITTLE_ENDIAN__
#error "LOC scalar decoder requires little endian IEEE-754 host"
#endif
_Static_assert(sizeof(float) == 4 && sizeof(double) == 8, "LOC float widths");

static float scalar(const uint8_t *p)
{
    float f;
    memcpy(&f, p, sizeof(f));
    return f;
}

int loc_position_decode(const uint8_t *const v[256], const unsigned n[256],
                        uint8_t session, struct loc_fix *out)
{
    *out = (struct loc_fix){0};
    /* LOC IDL v2.134 position descriptor: mandatory 01/u32,02/u8,
     * optional10/11=f64,25=u64; these other parsed scalar fields are f32,
     * u32 or confidence u8. memcpy handles unaligned packet storage. */
    const uint8_t tags[] = {1,2,0x10,0x11,0x12,0x13,0x14,0x16,0x18,0x19,
                           0x1a,0x1c,0x1d,0x20,0x21,0x23,0x25};
    const uint8_t widths[] = {4,1,8,8,4,4,4,1,4,4,4,4,1,4,4,4,8};
    if (!v[1] || !v[2]) return -1;
    for (unsigned i = 0; i < sizeof(tags); ++i)
        if (v[tags[i]] && n[tags[i]] != widths[i]) return -1;
    uint32_t status;
    memcpy(&status, v[1], sizeof(status));
    if (status > 7) return -1;
    if (v[2][0] != session || status != 0) return 0;
    /* In-progress and network/reference-only reports must never become GPS
     * fixes. A missing coordinate, technology or UTC field is not invented. */
    if (!v[0x10] || !v[0x11] || !v[0x23] || !v[0x25]) return 0;
    struct loc_fix fix = {0};
    memcpy(&fix.latitude, v[0x10], sizeof(fix.latitude));
    memcpy(&fix.longitude, v[0x11], sizeof(fix.longitude));
    memcpy(&fix.utc_ms, v[0x25], sizeof(fix.utc_ms));
    memcpy(&fix.technology, v[0x23], sizeof(fix.technology));
    if (!isfinite(fix.latitude) || fix.latitude < -90 || fix.latitude > 90 ||
        !isfinite(fix.longitude) || fix.longitude < -180 || fix.longitude > 180 ||
        !(fix.technology & 1) || fix.utc_ms < UINT64_C(315964800000) ||
        fix.utc_ms > INT64_MAX) return 0;

    /* Bad optional numeric values lose only their validity bit, not the
     * otherwise valid fix. Never normalize corrupt numbers into plausible ones. */
    if (v[0x1a]) {
        float x = scalar(v[0x1a]);
        if (isfinite(x) && x >= -500 && x <= 15883) {
            fix.altitude_m = x; fix.fields |= LOC_FIX_ALTITUDE;
        }
    }
    if (v[0x18]) {
        float x = scalar(v[0x18]);
        if (isfinite(x) && x >= 0) { fix.speed_mps = x; fix.fields |= LOC_FIX_SPEED; }
    }
    if (v[0x20]) {
        float x = scalar(v[0x20]);
        if (isfinite(x) && x >= 0 && x < 360) { fix.bearing_deg = x; fix.fields |= LOC_FIX_BEARING; }
    }
    if (v[0x12]) {
        float x = scalar(v[0x12]);
        if (isfinite(x) && x > 0) { fix.horizontal_circle_m = x; fix.fields |= LOC_FIX_HORIZONTAL_CIRCLE; }
    }
    if (v[0x13] && v[0x14]) {
        float minor = scalar(v[0x13]), major = scalar(v[0x14]);
        if (isfinite(minor) && isfinite(major) && minor > 0 && major >= minor) {
            fix.horizontal_minor_m = minor; fix.horizontal_major_m = major;
            fix.fields |= LOC_FIX_HORIZONTAL_ELLIPSE;
        }
    }
    if (v[0x16] && v[0x16][0] > 0 && v[0x16][0] <= 99) {
        fix.horizontal_confidence = v[0x16][0]; fix.fields |= LOC_FIX_HORIZONTAL_CONFIDENCE;
    }
    if (v[0x1c] && (fix.fields & LOC_FIX_ALTITUDE)) {
        float x = scalar(v[0x1c]);
        if (isfinite(x) && x > 0) { fix.vertical_uncertainty_m = x; fix.fields |= LOC_FIX_VERTICAL_UNCERTAINTY; }
    }
    if (v[0x1d] && v[0x1d][0] > 0 && v[0x1d][0] <= 99) {
        fix.vertical_confidence = v[0x1d][0]; fix.fields |= LOC_FIX_VERTICAL_CONFIDENCE;
    }
    if (v[0x19] && (fix.fields & LOC_FIX_SPEED)) {
        float x = scalar(v[0x19]);
        if (isfinite(x) && x > 0) { fix.speed_uncertainty_mps = x; fix.fields |= LOC_FIX_SPEED_UNCERTAINTY; }
    }
    if (v[0x21] && (fix.fields & LOC_FIX_BEARING)) {
        float x = scalar(v[0x21]);
        if (isfinite(x) && x > 0 && x < 360) { fix.bearing_uncertainty_deg = x; fix.fields |= LOC_FIX_BEARING_UNCERTAINTY; }
    }
    *out = fix;
    return 1;
}

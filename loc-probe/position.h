/* SPDX-License-Identifier: Apache-2.0 */
#ifndef BENGAL_LOC_POSITION_H
#define BENGAL_LOC_POSITION_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

enum loc_fix_fields {
    LOC_FIX_ALTITUDE = 1u << 0,
    LOC_FIX_SPEED = 1u << 1,
    LOC_FIX_BEARING = 1u << 2,
    LOC_FIX_HORIZONTAL_CIRCLE = 1u << 3,
    LOC_FIX_HORIZONTAL_ELLIPSE = 1u << 4,
    LOC_FIX_HORIZONTAL_CONFIDENCE = 1u << 5,
    LOC_FIX_VERTICAL_UNCERTAINTY = 1u << 6,
    LOC_FIX_VERTICAL_CONFIDENCE = 1u << 7,
    LOC_FIX_SPEED_UNCERTAINTY = 1u << 8,
    LOC_FIX_BEARING_UNCERTAINTY = 1u << 9,
};
/* A native successful SATELLITE report, not an Android GnssLocation. Wire
 * uncertainty/confidence are retained separately: they are NOT automatically
 * the 68%-confidence accuracy required by Android. No fabricated values. */
struct loc_fix {
    uint32_t fields, technology;
    uint64_t utc_ms;
    double latitude, longitude;
    float altitude_m, speed_mps, bearing_deg;
    float horizontal_circle_m, horizontal_minor_m, horizontal_major_m;
    float vertical_uncertainty_m, speed_uncertainty_mps, bearing_uncertainty_deg;
    uint8_t horizontal_confidence, vertical_confidence;
};
/* Caller has already bounded the full QMI datagram and rejected duplicate
 * TLVs. Each non-NULL value points to length[tag] readable bytes.
 * Returns -1 malformed known TLV, 0 no usable fix/other session, 1 fix.
 * Output is zero on every non-fix path (no stale coordinates). */
int loc_position_decode(const uint8_t *const value[256], const unsigned length[256],
                        uint8_t session, struct loc_fix *fix);
#ifdef __cplusplus
}
#endif
#endif

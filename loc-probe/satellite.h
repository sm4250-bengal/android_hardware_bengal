/* SPDX-License-Identifier: Apache-2.0 */
#ifndef BENGAL_LOC_SATELLITE_H
#define BENGAL_LOC_SATELLITE_H
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif
#define LOC_SV_MAX 176
/* Low eight bits are LOC IDL2.134 validity, bit8 marks derived baseband CN0.
 * These are not Android flags. Invalid optional numeric
 * fields lose their bit; fields without a bit must never be interpreted. */
enum { LOC_SV_SYSTEM=1, LOC_SV_ID=2, LOC_SV_HEALTH=4, LOC_SV_STATUS=8,
       LOC_SV_INFO=16, LOC_SV_ELEVATION=32, LOC_SV_AZIMUTH=64, LOC_SV_CN0=128, LOC_SV_BASEBAND_CN0=256 };
struct loc_sv {
    uint32_t valid, system, status;
    uint16_t id;
    uint8_t health, info;
    float elevation, azimuth, cn0, baseband_cn0;
};
struct loc_sv_report {
    unsigned count;
    uint8_t altitude_assumed;
    struct loc_sv sv[LOC_SV_MAX];
};
/* -1 malformed; 0 no list supplied (NOT an empty list); 1 explicit list,
 * including a real empty list. Caller already validated TLV framing/duplicates.
 * Never derives used-in-fix from tracking, nor manufactures unavailable CN0. */
int loc_satellite_decode(const uint8_t *const values[256], const unsigned lengths[256],
                         struct loc_sv_report *);
#ifdef __cplusplus
}
#endif
#endif

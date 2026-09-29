/* SPDX-License-Identifier: Apache-2.0 */
#include "satellite.h"
#include <math.h>
#include <string.h>
static uint32_t u32(const uint8_t *p) {
    return (uint32_t)p[0] | ((uint32_t)p[1]<<8) | ((uint32_t)p[2]<<16) | ((uint32_t)p[3]<<24);
}
static float f32(const uint8_t *p) {
    uint32_t bits = u32(p); float f; memcpy(&f, &bits, sizeof(f)); return f;
}
int loc_satellite_decode(const uint8_t *const v[256], const unsigned n[256],
                         struct loc_sv_report *out) {
    memset(out, 0, sizeof(*out));
    if (!v[1] || n[1] != 1 || v[1][0] > 1) return -1;
    out->altitude_assumed = v[1][0];
    /* The schema says the lists are mutually exclusive. Never concatenate
     * ambiguous lists or report the same SV twice. Wire arrays use count:u8. */
    if (v[0x10] && v[0x11]) return -1;
    if (!v[0x10] && !v[0x11]) return (v[0x12] || v[0x13]) ? -1 : 0;
    unsigned tag = v[0x11] ? 0x11 : 0x10;
    unsigned stride = tag == 0x11 ? 29 : 28;
    unsigned limit = tag == 0x11 ? LOC_SV_MAX : 80;
    if (n[tag] < 1 || v[tag][0] > limit || n[tag] != 1u + v[tag][0]*stride) return -1;
    out->count = v[tag][0];
    /* Optional arrays have the same one-byte wire count and must align with
     * the SV list. RF-loss is specified only for the expanded list. */
    if (v[0x12] && (n[0x12] < 1 || v[0x12][0] != out->count ||
                    n[0x12] != 1u + out->count*8)) return -1;
    if (v[0x13] && (tag != 0x11 || n[0x13] < 1 || v[0x13][0] != out->count ||
                    n[0x13] != 1u + out->count*4)) return -1;
    for (unsigned i=0; i<out->count; ++i) {
        const uint8_t *p = v[tag]+1+i*stride;
        struct loc_sv *s = &out->sv[i];
        s->valid = u32(p) & 0xff; /* unknown validity bits grant no capabilities */
        s->system = u32(p+4); s->id = p[8] | ((uint16_t)p[9]<<8);
        s->health = p[10]; s->status = u32(p+11); s->info = p[15] & 3;
        s->elevation = f32(p+16); s->azimuth = f32(p+20); s->cn0 = f32(p+24);
        if (s->system < 1 || s->system > 8) s->valid &= ~LOC_SV_SYSTEM;
        if (!s->id) s->valid &= ~LOC_SV_ID;
        if (s->health > 1) s->valid &= ~LOC_SV_HEALTH;
        if (s->status < 1 || s->status > 3) s->valid &= ~LOC_SV_STATUS;
        if (!isfinite(s->elevation) || s->elevation < 0 || s->elevation > 90) s->valid &= ~LOC_SV_ELEVATION;
        if (!isfinite(s->azimuth) || s->azimuth < 0 || s->azimuth > 360) s->valid &= ~LOC_SV_AZIMUTH;
        if (!isfinite(s->cn0) || s->cn0 < 0) s->valid &= ~LOC_SV_CN0;
        if (v[0x13] && (s->valid & LOC_SV_CN0)) {
            float loss = f32(v[0x13]+1+i*4);
            if (isfinite(loss) && loss >= 0 && loss <= s->cn0) {
                s->baseband_cn0 = s->cn0 - loss;
                s->valid |= LOC_SV_BASEBAND_CN0;
            }
        }
    }
    return 1;
}

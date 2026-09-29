/* SPDX-License-Identifier: Apache-2.0 */
#include "position.h"
#include <assert.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
static const uint8_t *v[256];
static unsigned n[256];
static uint8_t storage[256][12];
static unsigned checks;
static void put(unsigned tag, const void *p, unsigned size)
{
    assert(size < sizeof(storage[tag]));
    memcpy(storage[tag] + 1, p, size); /* Deliberately unaligned. */
    v[tag] = storage[tag] + 1; n[tag] = size;
}
static void u32(unsigned tag, uint32_t x) { put(tag, &x, sizeof(x)); }
static void u64(unsigned tag, uint64_t x) { put(tag, &x, sizeof(x)); }
static void f64(unsigned tag, double x) { put(tag, &x, sizeof(x)); }
static void f32(unsigned tag, float x) { put(tag, &x, sizeof(x)); }
static void u8(unsigned tag, uint8_t x) { put(tag, &x, sizeof(x)); }
static void baseline(void)
{
    memset(v, 0, sizeof(v)); memset(n, 0, sizeof(n));
    u32(1, 0); u8(2, 1);
    f64(0x10, 12.25); f64(0x11, -34.5);
    u32(0x23, 1); u64(0x25, UINT64_C(1700000000123));
}
static struct loc_fix decode(int expected)
{
    struct loc_fix fix;
    memset(&fix, 0xaa, sizeof(fix));
    assert(loc_position_decode(v, n, 1, &fix) == expected);
    if (expected != 1)
        assert(!fix.fields && fix.latitude == 0 && fix.longitude == 0 && !fix.utc_ms);
    ++checks;
    return fix;
}
int main(void)
{
    baseline();
    struct loc_fix fix = decode(1);
    assert(fix.latitude == 12.25 && fix.longitude == -34.5 && fix.utc_ms == UINT64_C(1700000000123));
    assert(fix.fields == 0); /* No fabricated accuracy/altitude. */
    f64(0x10, 0); f64(0x11, 0); decode(1); /* (0,0) is a real coordinate. */
    f64(0x10, -90); f64(0x11, 180); decode(1);
    f64(0x10, 90); f64(0x11, -180); decode(1);
    for (unsigned status = 1; status <= 8; ++status) {
        baseline(); u32(1, status); decode(status <= 7 ? 0 : -1);
    }
    baseline(); u8(2, 2); decode(0);
    const uint8_t required[] = {1,2,0x10,0x11,0x23,0x25};
    for (unsigned i = 0; i < sizeof(required); ++i) {
        baseline(); v[required[i]] = NULL; decode(i < 2 ? -1 : 0);
    }
    const uint8_t tags[] = {1,2,0x10,0x11,0x12,0x13,0x14,0x16,0x18,0x19,0x1a,0x1c,0x1d,0x20,0x21,0x23,0x25};
    for (unsigned i = 0; i < sizeof(tags); ++i) {
        baseline(); uint8_t bad[3] = {0}; put(tags[i], bad, 3); decode(-1);
    }
    const double bad_lat[] = {NAN, INFINITY, -INFINITY, 90.01, -90.01};
    for (unsigned i = 0; i < sizeof(bad_lat)/sizeof(*bad_lat); ++i) {
        baseline(); f64(0x10, bad_lat[i]); decode(0);
    }
    const double bad_lon[] = {NAN, INFINITY, -INFINITY, 180.01, -180.01};
    for (unsigned i = 0; i < sizeof(bad_lon)/sizeof(*bad_lon); ++i) {
        baseline(); f64(0x11, bad_lon[i]); decode(0);
    }
    baseline(); u32(0x23, 0x102); decode(0); /* Network/dead reckoning only. */
    baseline(); u32(0x23, 0x105); decode(1); /* Real satellite plus others. */
    baseline(); u64(0x25, 0); decode(0);
    baseline(); u64(0x25, UINT64_MAX); decode(0);
    baseline(); u64(0x25, UINT64_C(315964799999)); decode(0);
    baseline(); u64(0x25, UINT64_C(315964800000)); decode(1);
    baseline(); u32(0xfe, 99); decode(1); /* Unknown optional fields are ignored. */
    baseline(); f32(0x1a, 123); f32(0x18, 0); f32(0x20, 0);
    f32(0x12, 9); f32(0x13, 3); f32(0x14, 5); u8(0x16, 39);
    f32(0x1c, 8); u8(0x1d, 68); f32(0x19, 2); f32(0x21, 1);
    fix = decode(1);
    assert(fix.fields == 1023 && fix.horizontal_confidence == 39);
    assert(fix.horizontal_circle_m == 9 && fix.horizontal_minor_m == 3 && fix.horizontal_major_m == 5);
    assert(fix.altitude_m == 123 && fix.speed_mps == 0 && fix.bearing_deg == 0);
    assert(fix.vertical_uncertainty_m == 8 && fix.speed_uncertainty_mps == 2 && fix.bearing_uncertainty_deg == 1);
    /* Optional numeric corruption does not destroy valid coordinates, but it
     * never creates a valid bit for a bad value or an orphan accuracy. */
    const uint8_t floats[] = {0x1a,0x18,0x20,0x12,0x13,0x14,0x1c,0x19,0x21};
    for (unsigned i = 0; i < sizeof(floats); ++i) {
        baseline(); f32(floats[i], NAN); fix = decode(1); assert(!fix.fields);
        baseline(); f32(floats[i], INFINITY); fix = decode(1); assert(!fix.fields);
    }
    baseline(); f32(0x13, 6); f32(0x14, 5); fix = decode(1); assert(!fix.fields);
    baseline(); f32(0x12, 0); f32(0x18, -1); f32(0x20, 360); f32(0x1a, -501);
    u8(0x16, 100); u8(0x1d, 0); fix = decode(1); assert(!fix.fields);
    baseline(); f32(0x1c, 5); f32(0x19, 5); f32(0x21, 5); fix = decode(1); assert(!fix.fields);
    printf("PASS: %u native position decoder cases; synthetic fixtures, no device fix claim\n", checks);
}

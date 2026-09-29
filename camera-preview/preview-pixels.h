/* SPDX-License-Identifier: GPL-2.0-only */
#ifndef BENGAL_PREVIEW_PIXELS_H
#define BENGAL_PREVIEW_PIXELS_H
#include <stdint.h>
#define PREVIEW_WIDTH 640u
#define PREVIEW_HEIGHT 360u
#define PREVIEW_BYTES (PREVIEW_WIDTH * PREVIEW_HEIGHT * 2u)
static inline int preview_clip(int value)
{
	return value < 0 ? 0 : value > 255 ? 255 : value;
}
/* Limited-range BT.709 UYVY -> little-endian RGB565; sample every second
 * pixel/row for the diagnostic view. Capture itself remains1280x720p25. */
static inline void preview_convert(const unsigned char *in, unsigned char *out)
{
	for (unsigned int y = 0; y < PREVIEW_HEIGHT; y++) {
		for (unsigned int x = 0; x < PREVIEW_WIDTH; x++) {
			unsigned int p = y * 2u * 2560u + x * 4u;
			int luma = (int)in[p + 1] - 16;
			int u = (int)in[p] - 128, v = (int)in[p + 2] - 128;
			int r = preview_clip((298 * luma + 459 * v + 128) >> 8);
			int g = preview_clip((298 * luma - 55 * u - 136 * v + 128) >> 8);
			int b = preview_clip((298 * luma + 541 * u + 128) >> 8);
			uint16_t pixel = (uint16_t)(((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3));
			unsigned int dst = (y * PREVIEW_WIDTH + x) * 2;
			out[dst] = pixel & 255; out[dst + 1] = pixel >> 8;
		}
	}
}
#endif

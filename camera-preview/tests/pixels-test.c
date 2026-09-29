#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "../preview-pixels.h"

static void test(unsigned int u, unsigned int y, unsigned int v, uint16_t expected)
{
    unsigned char *input = malloc(1280 * 720 * 2);
    unsigned char *output = malloc(PREVIEW_BYTES + 2);
    if (!input || !output) exit(1);
    for (unsigned int i = 0; i < 1280 * 720 * 2; i += 4) {
        input[i] = u; input[i+1] = y; input[i+2] = v; input[i+3] = y;
    }
    memset(output, 0xa5, PREVIEW_BYTES + 2);
    preview_convert(input, output);
    for (unsigned int i = 0; i < PREVIEW_BYTES; i += 2) {
        if ((output[i] | ((uint16_t)output[i+1] << 8)) != expected) {
            fprintf(stderr, "conversion mismatch at%u\n", i); exit(1);
        }
    }
    if (output[PREVIEW_BYTES] != 0xa5 || output[PREVIEW_BYTES+1] != 0xa5) exit(1);
    free(input); free(output);
}

int main(void)
{
    test(128, 16, 128, 0x0000);
    test(128, 235, 128, 0xffff);
    test(102, 63, 240, 0xf800);
    test(128, 0, 128, 0x0000);
    test(128, 255, 128, 0xffff);
    puts("PASS 5 BT709 RGB565 conversion/bounds cases");
    return 0;
}

#define _GNU_SOURCE
#include "../startup.h"
#include <assert.h>

static int parse(const char *text, unsigned int *camera, unsigned int *power) {
    FILE *f = fmemopen((void *)text, strlen(text), "r");
    assert(f);
    int rc = camera_package_uids(f, camera, power);
    fclose(f);
    return rc;
}
int main(void) {
    unsigned int camera, power;
    assert(!parse("unrelated 10001\ncom.meinte.xyautomcutoolbox 10147 0 /data/user/0/a default\ncom.bengal.power 10132 0 /data/user/0/b platform\n", &camera, &power));
    assert(camera == 10147 && power == 10132);
    assert(parse("com.meinte.xyautomcutoolbox.fake 10147\ncom.bengal.power 10132\n", &camera, &power));
    assert(parse("com.meinte.xyautomcutoolbox 10147\n", &camera, &power));
    assert(parse("com.meinte.xyautomcutoolbox 10147\ncom.bengal.power 10147\n", &camera, &power));
    assert(parse("com.meinte.xyautomcutoolbox 0\ncom.bengal.power 10132\n", &camera, &power));
    assert(parse("com.meinte.xyautomcutoolbox 10147x\ncom.bengal.power 10132\n", &camera, &power));
    assert(parse("com.meinte.xyautomcutoolbox 10147\ncom.bengal.power 10132\ncom.bengal.power 10133\n", &camera, &power));
    assert(parse("com.meinte.hucamera 10147\ncom.bengal.power 10132\n", &camera, &power));
    assert(!parse("com.meinte.hucamera 10146\ncom.meinte.xyautomcutoolbox 10147\ncom.bengal.power 10132\n", &camera, &power));
    assert(camera == 10147);
    puts("PASS camera startup exact-package UID selection (9 cases)");
}

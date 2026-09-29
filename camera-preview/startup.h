/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/* PackageManager's root-owned packages.list is refreshed when packages change.
 * Select exact user0 packages and reject duplicates/invalid UIDs. No shell,
 * package-manager Binder privilege, hardcoded app id or platform key change.
 */
static inline int camera_package_uids(FILE *packages, unsigned int *camera,
                                      unsigned int *power)
{
    char *line = NULL;
    size_t capacity = 0;
    unsigned int camera_seen = 0, power_seen = 0;
    int result = -1;
    *camera = *power = 0;
    while (getline(&line, &capacity, packages) >= 0) {
        char name[256], uid_text[64], *end;
        unsigned long uid;
        if (sscanf(line, "%255s %63s", name, uid_text) != 2) continue;
        if (strcmp(name, "com.meinte.xyautomcutoolbox") && strcmp(name, "com.bengal.power")) continue;
        errno = 0;
        uid = strtoul(uid_text, &end, 10);
        if (errno || *end || uid < 10000 || uid > 19999) goto done;
        if (!strcmp(name, "com.meinte.xyautomcutoolbox")) {
            if (camera_seen++) goto done;
            *camera = (unsigned int)uid;
        } else {
            if (power_seen++) goto done;
            *power = (unsigned int)uid;
        }
    }
    if (!ferror(packages) && camera_seen == 1 && power_seen == 1 && *camera != *power) result = 0;
done:
    free(line);
    return result;
}

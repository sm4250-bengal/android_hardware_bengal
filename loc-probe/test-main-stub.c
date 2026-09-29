/* SPDX-License-Identifier: Apache-2.0
 * Host-only CLI tests: this linker unit replaces the real QRTR transport. */
#include "loc.h"
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>
static int failed_fd = -1;
static void check_closed(void)
{
    errno = 0;
    if (fcntl(failed_fd, F_GETFD) != -1 || errno != EBADF) _exit(99);
    fprintf(stderr, "TEST fdopen-failure descriptor-closed\n");
}
FILE *__real_fdopen(int, const char *);
FILE *__wrap_fdopen(int fd, const char *mode)
{
    if (!(fcntl(fd, F_GETFD) & FD_CLOEXEC)) _exit(98);
    if (getenv("LOC_TEST_FDOPEN_FAIL")) {
        failed_fd = fd; atexit(check_closed); errno = ENOMEM; return NULL;
    }
    return __real_fdopen(fd, mode);
}
int loc_qrtr_open(struct loc_io *io)
{
    (void)io;
    fprintf(stderr, "TEST QRTR_STUB no-socket\n");
    return -1;
}

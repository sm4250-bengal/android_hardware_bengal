/* SPDX-License-Identifier: Apache-2.0 */
#include "loc.h"
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
static volatile sig_atomic_t cancelled;
static void stop(int signo) { cancelled = signo; }
int main(int argc, char **argv)
{
    unsigned seconds = 0;
    const char *path = NULL;
    int manual = 0, query_only = 0, receiver_info = 0, rf_metrics = 0, acquisition_config = 0;
    int delete_aiding_all = 0;
    for (int i = 1; i < argc; ++i) {
        if (!strcmp(argv[i], "--qrtr")) manual = 1;
        else if (!strcmp(argv[i], "--query-only")) query_only = 1;
        else if (!strcmp(argv[i], "--receiver-info")) receiver_info = 1;
        else if (!strcmp(argv[i], "--acquisition-config")) acquisition_config = 1;
        else if (!strcmp(argv[i], "--delete-aiding-all")) delete_aiding_all = 1;
        else if (!strcmp(argv[i], "--rf-metrics")) rf_metrics = 1;
        else if (!strcmp(argv[i], "--seconds") && i + 1 < argc) {
            char *end; errno = 0; unsigned long v = strtoul(argv[++i], &end, 10);
            if (errno || *end || v < 1 || v > 180) goto usage;
            seconds = v;
        } else if (!strcmp(argv[i], "--log") && i + 1 < argc) path = argv[++i];
        else goto usage;
    }
    if (!manual || !path || (query_only + receiver_info + acquisition_config + delete_aiding_all + (seconds != 0) != 1) ||
        (rf_metrics && (!seconds || query_only || receiver_info || acquisition_config || delete_aiding_all))) goto usage;
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0) { perror("LOC log (must be new)"); return 1; }
    FILE *log = fdopen(fd, "w");
    if (!log) { int error = errno; close(fd); errno = error; perror("LOC fdopen"); return 1; }
    setvbuf(log, NULL, _IOLBF, 0);
    struct sigaction action = {.sa_handler = stop};
    sigemptyset(&action.sa_mask);
    sigaction(SIGINT, &action, NULL); sigaction(SIGTERM, &action, NULL); sigaction(SIGHUP, &action, NULL);
    struct loc_io io;
    if (loc_qrtr_open(&io)) { fprintf(log, "LOC FAIL open-qrtr\n"); fclose(log); return 1; }
    int ret = rf_metrics ? loc_rf_metrics(&io, log, seconds * 1000, 2000, &cancelled)
                         : delete_aiding_all ? loc_delete_aiding_all(&io, log, 2000, &cancelled)
                         : acquisition_config ? loc_acquisition_config(&io, log, 2000, &cancelled)
                         : receiver_info ? loc_receiver_info(&io, log, 2000, &cancelled)
                         : query_only ? loc_query(&io, log, 2000, &cancelled)
                         : loc_run(&io, log, seconds * 1000, 2000, &cancelled);
    if (ferror(log)) ret = 1;
    if (fclose(log)) ret = 1;
    return ret;
usage:
    fprintf(stderr, "Manual only: %s --qrtr (--delete-aiding-all | --acquisition-config | --receiver-info | --query-only | --seconds 1..180 [--rf-metrics]) --log NEW_FILE\n", argv[0]);
    return 2;
}

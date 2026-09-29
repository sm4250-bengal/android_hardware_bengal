#define _GNU_SOURCE
#include "../policy.h"
#include <arpa/inet.h>
#include <assert.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

static int connect_policy(void) {
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    const char name[] = "bengal-camera-policy-v1";
    __builtin_memcpy(addr.sun_path + 1, name, sizeof(name) - 1);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    assert(fd >= 0);
    struct timeval timeout = {.tv_sec = 2};
    assert(!setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)));
    assert(!connect(fd, (struct sockaddr *)&addr, offsetof(struct sockaddr_un, sun_path) + sizeof(name)));
    return fd;
}
static void command(int fd, unsigned gen, unsigned awake) {
    uint32_t p[] = {htonl(0x42434331), htonl(gen), htonl(awake)};
    assert(send(fd, p, sizeof(p), MSG_NOSIGNAL) == sizeof(p));
}
static unsigned receipt(int fd, unsigned gen, unsigned flags) {
    for (unsigned n = 0; n < 8; n++) {
        uint32_t p[3];
        assert(recv(fd, p, sizeof(p), MSG_WAITALL) == sizeof(p));
        assert(ntohl(p[0]) == 0x42434331);
        if (ntohl(p[1]) == gen && ntohl(p[2]) == flags) return flags;
    }
    assert(0); return 0;
}
int main(void) {
    assert(!camera_policy_start(getuid(), -1));
    assert(!camera_policy_begin(false));
    int fd = connect_policy();
    command(fd, 1, 1); receipt(fd, 1, 1);
    assert(camera_policy_allowed(false));
    assert(!camera_policy_allowed(true)); // No measured reverse input.
    assert(camera_policy_begin(false));
    assert(!camera_policy_begin(false));
    receipt(fd, 1, 3);
    command(fd, 2, 0); receipt(fd, 2, 2);
    assert(!camera_policy_begin(false));
    camera_policy_streaming(false); receipt(fd, 2, 0);
    command(fd, 3, 1); receipt(fd, 3, 1);
    close(fd);
    for (int n = 0; n < 200 && camera_policy_allowed(false); n++) usleep(1000);
    assert(!camera_policy_allowed(false));
    fd = connect_policy(); command(fd, 1, 1); receipt(fd, 1, 1);
    command(fd, 1, 1); // Replayed command must disconnect and revoke.
    uint32_t p[3]; assert(recv(fd, p, sizeof(p), MSG_WAITALL) <= 0);
    assert(!camera_policy_allowed(false)); close(fd);
    camera_policy_stop();
    puts("PASS camera native policy: held admission, busy receipt, cleanup, loss, replay");
}

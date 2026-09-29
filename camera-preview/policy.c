/* SPDX-License-Identifier: Apache-2.0 */
#define _GNU_SOURCE
#include "policy.h"
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/gpio.h>
#include <poll.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <stddef.h>
#include <time.h>
#include <unistd.h>

/* Core-side policy channel. Only BengalPower's resolved UID can own it.
 * A lost policy connection revokes capture; an idle receipt follows actual
 * STREAMOFF/FD cleanup, never merely a stop request. No hardware writes here
 * apart from requesting the explicitly configured reverse line as INPUT.
 */
#define POLICY_MAGIC 0x42434331u
static atomic_bool allowed, streaming, ending;
static atomic_int reverse_state = -1;
static int changed_fd = -1, listen_fd = -1, gpio_fd = -1;
static unsigned int policy_uid;
static pthread_t worker;
static pthread_mutex_t admission = PTHREAD_MUTEX_INITIALIZER;

static void changed(void) {
    uint64_t one = 1;
    if (changed_fd >= 0) (void)write(changed_fd, &one, sizeof(one));
}
bool camera_policy_allowed(bool automatic) {
    return atomic_load(&allowed) && !atomic_load(&ending)
        && (!automatic || atomic_load(&reverse_state) == 1);
}
bool camera_policy_begin(bool automatic) {
    pthread_mutex_lock(&admission);
    bool ok = camera_policy_allowed(automatic) && !atomic_load(&streaming);
    if (ok) atomic_store(&streaming, true);
    pthread_mutex_unlock(&admission);
    if (ok) changed();
    return ok;
}
void camera_policy_streaming(bool busy) {
    pthread_mutex_lock(&admission); atomic_store(&streaming, busy);
    pthread_mutex_unlock(&admission); changed();
}
static void permit(bool value) {
    pthread_mutex_lock(&admission); atomic_store(&allowed, value);
    pthread_mutex_unlock(&admission);
}

static int reverse_read(void) {
    struct gpio_v2_line_values values = {.mask = 1};
    return gpio_fd >= 0 && !ioctl(gpio_fd, GPIO_V2_LINE_GET_VALUES_IOCTL, &values)
        ? !!(values.bits & 1) : -1;
}
static int reverse_open(int line) {
    if (line < 0) return 0;
    /* Device-3 OEM-derived candidate; no arbitrary-pin interface or sweep. */
    if (line != 42) { errno = EINVAL; return -1; }
    for (int n = 0; n < 4; n++) {
        char path[32]; struct gpiochip_info chip = {0};
        snprintf(path, sizeof(path), "/dev/gpiochip%d", n);
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        if (fd < 0) continue;
        if (ioctl(fd, GPIO_GET_CHIPINFO_IOCTL, &chip)
                || strcmp(chip.label, "500000.pinctrl") || chip.lines != 114) {
            close(fd); continue;
        }
        struct gpio_v2_line_info info = {.offset = (unsigned int)line};
        if (ioctl(fd, GPIO_V2_GET_LINEINFO_IOCTL, &info)
                || (info.flags & (GPIO_V2_LINE_FLAG_USED | GPIO_V2_LINE_FLAG_OUTPUT))) {
            close(fd); errno = EBUSY; return -1;
        }
        struct gpio_v2_line_request req = {.num_lines = 1};
        req.offsets[0] = (unsigned int)line;
        snprintf(req.consumer, sizeof(req.consumer), "bengal-reverse");
        req.config.flags = GPIO_V2_LINE_FLAG_INPUT | GPIO_V2_LINE_FLAG_EDGE_RISING
            | GPIO_V2_LINE_FLAG_EDGE_FALLING;
        int rc = ioctl(fd, GPIO_V2_GET_LINE_IOCTL, &req);
        close(fd);
        if (rc) return -1;
        gpio_fd = req.fd;
        fcntl(gpio_fd, F_SETFD, FD_CLOEXEC);
        fcntl(gpio_fd, F_SETFL, O_NONBLOCK);
        atomic_store(&reverse_state, reverse_read());
        printf("CAMERA_REVERSE line=%d value=%d\n", line, atomic_load(&reverse_state));
        return 0;
    }
    errno = ENODEV; return -1;
}

static int receipt(int fd, uint32_t generation) {
    int reverse = atomic_load(&reverse_state);
    uint32_t flags = (atomic_load(&allowed) ? 1u : 0u)
        | (atomic_load(&streaming) ? 2u : 0u)
        | (reverse >= 0 ? 4u : 0u) | (reverse == 1 ? 8u : 0u);
    uint32_t packet[3] = {htonl(POLICY_MAGIC), htonl(generation), htonl(flags)};
    return send(fd, packet, sizeof(packet), MSG_NOSIGNAL) == sizeof(packet) ? 0 : -1;
}
static int64_t now_ms(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}
static void *policy_main(void *unused) {
    (void)unused;
    int client = -1; uint32_t generation = 0; int64_t debounce = 0;
    while (!atomic_load(&ending)) {
        struct pollfd fds[] = {
            {.fd = client < 0 ? listen_fd : client, .events = POLLIN},
            {.fd = changed_fd, .events = POLLIN},
            {.fd = gpio_fd, .events = POLLIN}
        };
        int timeout = debounce ? (int)(debounce - now_ms()) : -1;
        if (debounce && timeout < 0) timeout = 0;
        int n = poll(fds, 3, timeout);
        if (n < 0) { if (errno == EINTR) continue; break; }
        bool update = false;
        if (fds[1].revents) {
            uint64_t value; (void)read(changed_fd, &value, sizeof(value)); update = true;
        }
        if (fds[2].revents & POLLIN) {
            struct gpio_v2_line_event event;
            while (read(gpio_fd, &event, sizeof(event)) == sizeof(event)) { }
            debounce = now_ms() + 80;
        }
        if (debounce && now_ms() >= debounce) {
            int value = reverse_read();
            if (atomic_exchange(&reverse_state, value) != value) {
                printf("CAMERA_REVERSE value=%d\n", value); update = true;
            }
            debounce = 0;
        }
        if (fds[2].revents & (POLLERR | POLLHUP | POLLNVAL)) {
            close(gpio_fd); gpio_fd = -1;
            atomic_store(&reverse_state, -1); update = true;
        }
        if (fds[0].revents && client < 0) {
            struct ucred peer; socklen_t len = sizeof(peer);
            client = accept4(listen_fd, NULL, NULL, SOCK_CLOEXEC);
            if (client < 0) continue;
            struct timeval timeout = {.tv_sec = 1};
            if (getsockopt(client, SOL_SOCKET, SO_PEERCRED, &peer, &len)
                    || len != sizeof(peer) || peer.uid != policy_uid
                    || setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout))
                    || setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout))) {
                close(client); client = -1; continue;
            }
            generation = 0;
            printf("CAMERA_POLICY_CONNECTED uid=%u\n", peer.uid);
            continue; // No permission until the first complete policy packet.
        }
        if (fds[0].revents && client >= 0) {
            uint32_t packet[3];
            if (recv(client, packet, sizeof(packet), MSG_WAITALL) != sizeof(packet)
                    || ntohl(packet[0]) != POLICY_MAGIC || !ntohl(packet[1])
                    || ntohl(packet[1]) <= generation || ntohl(packet[2]) > 1) goto lost;
            generation = ntohl(packet[1]);
            permit(ntohl(packet[2]) == 1);
            if (atomic_load(&allowed) && gpio_fd >= 0)
                atomic_store(&reverse_state, reverse_read());
            printf("CAMERA_POLICY generation=%u allowed=%d\n", generation, atomic_load(&allowed));
            update = true;
        }
        if (client >= 0 && update && receipt(client, generation)) {
lost:
            permit(false);
            close(client); client = -1; generation = 0;
            printf("CAMERA_POLICY_LOST capture revoked\n");
        }
    }
    permit(false);
    if (client >= 0) close(client);
    return NULL;
}
int camera_policy_start(unsigned int owner_uid, int reverse_line) {
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    const char name[] = "bengal-camera-policy-v1";
    policy_uid = owner_uid;
    if (reverse_open(reverse_line)) return -1;
    changed_fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
    listen_fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    memcpy(addr.sun_path + 1, name, sizeof(name) - 1);
    if (changed_fd < 0 || listen_fd < 0
            || bind(listen_fd, (struct sockaddr *)&addr,
                offsetof(struct sockaddr_un, sun_path) + sizeof(name))
            || listen(listen_fd, 1)) return -1;
    int error = pthread_create(&worker, NULL, policy_main, NULL);
    if (error) { errno = error; return -1; }
    return 0;
}
void camera_policy_stop(void) {
    atomic_store(&ending, true); permit(false); changed();
    pthread_join(worker, NULL);
    if (gpio_fd >= 0) close(gpio_fd);
    close(listen_fd); close(changed_fd);
}

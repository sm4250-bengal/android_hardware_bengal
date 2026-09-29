/* SPDX-License-Identifier: GPL-2.0-only */
/* Derived from kernel/tools/bengal-camera-capture.c at988e02ea.
 * ACC-coordinated bridge; only the explicitly allowed app UID can request
 * capture while authenticated BengalPower permits it. One client; capture
 * closes on disconnect/error/ACC hold and automatic capture on reverse exit.
 * Idle server owns no camera descriptors. No MCU/root-command interface.
 */
#define _GNU_SOURCE
#include "startup.h"
#include <sys/system_properties.h>
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/media.h>
#include <linux/media-bus-format.h>
#include <linux/v4l2-subdev.h>
#include <linux/videodev2.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <stddef.h>
#include <unistd.h>
#include "preview-pixels.h"
#include "policy.h"

static struct media_entity_desc entities[64];
static unsigned int count;
static volatile sig_atomic_t interrupted;
static void stop(int sig) { (void)sig; interrupted = 1; }

static int write_full(int fd, const void *bytes, size_t count)
{
	const unsigned char *p = bytes;
	while (count && !interrupted) {
		ssize_t n = send(fd, p, count, MSG_NOSIGNAL);
		if (n < 0 && errno == EINTR && !interrupted) continue;
		if (n <= 0) return -1;
		p += n; count -= (size_t)n;
	}
	return count ? -1 : 0;
}

static int xio(int fd, unsigned long req, void *p)
{
	int r;
	do { r = ioctl(fd, req, p); } while (r < 0 && errno == EINTR && !interrupted);
	return r;
}
static int entity(const char *name, int prefix)
{
	for (unsigned int i = 0; i < count; ++i)
		if (prefix ? !strncmp(entities[i].name, name, strlen(name)) :
			     !strcmp(entities[i].name, name)) return (int)i;
	return -1;
}
static int open_node(int e)
{
	char path[80];
	struct stat st;
	if (e < 0) { errno = ENODEV; return -1; }
	for (int type = 0; type < 2; ++type)
		for (int i = 0; i < 64; ++i) {
			snprintf(path, sizeof(path), "/dev/%s%d", type ? "video" : "v4l-subdev", i);
			if (!stat(path, &st) && S_ISCHR(st.st_mode) &&
			    major(st.st_rdev) == entities[e].dev.major &&
			    minor(st.st_rdev) == entities[e].dev.minor) {
				printf("NODE %s %s\n", entities[e].name, path);
				return open(path, O_RDWR | O_NONBLOCK | O_CLOEXEC);
			}
		}
	errno = ENOENT;
	return -1;
}
static int link_set(int media, int src, unsigned int sp, int sink,
		    unsigned int dp, unsigned int flags)
{
	struct media_link_desc l = {0};
	if (src < 0 || sink < 0) { errno = ENODEV; return -1; }
	l.source.entity = entities[src].id; l.source.index = sp;
	l.sink.entity = entities[sink].id; l.sink.index = dp;
	l.flags = flags;
	printf("LINK %s:%u -> %s:%u flags=%u\n", entities[src].name, sp,
	       entities[sink].name, dp, flags);
	return xio(media, MEDIA_IOC_SETUP_LINK, &l);
}
static int set_fmt(int fd, unsigned int pad)
{
	struct v4l2_subdev_format fmt = { .which = V4L2_SUBDEV_FORMAT_ACTIVE, .pad = pad };
	fmt.format.width = 1280; fmt.format.height = 720;
	fmt.format.code = MEDIA_BUS_FMT_UYVY8_1X16;
	fmt.format.field = V4L2_FIELD_NONE;
	fmt.format.colorspace = V4L2_COLORSPACE_REC709;
	fmt.format.ycbcr_enc = V4L2_YCBCR_ENC_709;
	fmt.format.quantization = V4L2_QUANTIZATION_LIM_RANGE;
	fmt.format.xfer_func = V4L2_XFER_FUNC_709;
	if (xio(fd, VIDIOC_SUBDEV_S_FMT, &fmt)) return -1;
	printf("FORMAT pad=%u %ux%u code=%x\n", pad, fmt.format.width,
	       fmt.format.height, fmt.format.code);
	if (fmt.format.width != 1280 || fmt.format.height != 720 ||
	    fmt.format.code != MEDIA_BUS_FMT_UYVY8_1X16) { errno = EINVAL; return -1; }
	return 0;
}
static int dump(int fd)
{
	struct media_device_info info;
	struct media_entity_desc e = { .id = MEDIA_ENT_ID_FLAG_NEXT };
	if (xio(fd, MEDIA_IOC_DEVICE_INFO, &info)) return -1;
	printf("MEDIA driver=%s model=%s bus=%s\n", info.driver, info.model, info.bus_info);
	if (strcmp(info.driver, "qcom-camss")) { errno = ENODEV; return -1; }
	while (!xio(fd, MEDIA_IOC_ENUM_ENTITIES, &e)) {
		if (count == 64 || !memchr(e.name, 0, sizeof(e.name))) { errno = E2BIG; return -1; }
		entities[count++] = e;
		printf("ENTITY id=%u name=%s pads=%u links=%u node=%u:%u\n",
		       e.id, e.name, e.pads, e.links, e.dev.major, e.dev.minor);
		e.id |= MEDIA_ENT_ID_FLAG_NEXT;
	}
	if (errno != EINVAL) return -1;
	return 0;
}

static int capture(int client, bool automatic)
{
	int media = -1, video = -1, sd[4] = {-1,-1,-1,-1};
	int result = 1, stream = 0, links = 0;
	unsigned int frames = 0;
	int sensor, phy, csid, vfe, vid;
	void *mem[4] = {MAP_FAILED,MAP_FAILED,MAP_FAILED,MAP_FAILED};
	size_t lengths[4] = {0};
	const enum v4l2_buf_type type = V4L2_BUF_TYPE_VIDEO_CAPTURE_MPLANE;
	struct v4l2_requestbuffers req = { .count = 4, .type = type, .memory = V4L2_MEMORY_MMAP };
	struct v4l2_format fmt = { .type = type };
	unsigned char *rgb = malloc(PREVIEW_BYTES);
	uint32_t header[] = {htonl(0x42435031), htonl(PREVIEW_WIDTH),
		htonl(PREVIEW_HEIGHT), htonl(PREVIEW_BYTES)};
	count = 0;
	if (!rgb || write_full(client, header, sizeof(header))) goto done;
	media = open("/dev/media0", O_RDWR | O_CLOEXEC);
	if (media < 0 || dump(media)) goto done;
	sensor = entity("tp2855-bengal", 1); phy = entity("msm_csiphy0", 0);
	csid = entity("msm_csid0", 0); vfe = entity("msm_vfe0_rdi0", 0);
	vid = entity("msm_vfe0_video0", 0);
	if (sensor < 0 || phy < 0 || csid < 0 || vfe < 0 || vid < 0) {
		errno = ENODEV; goto done;
	}
	/* Fresh manually loaded graph only: mutable links must be disabled.
	 * Refuse an existing capture configuration rather than silently take it. */
	for (unsigned int i = 0; i < count; ++i) {
		struct media_pad_desc pads[16]; struct media_link_desc ll[128];
		struct media_links_enum le = { .entity = entities[i].id, .pads = pads, .links = ll };
		if (entities[i].pads > 16 || entities[i].links > 128) { errno = E2BIG; goto done; }
		memset(pads, 0, sizeof(pads)); memset(ll, 0, sizeof(ll));
		if (xio(media, MEDIA_IOC_ENUM_LINKS, &le)) goto done;
		for (unsigned int j = 0; j < entities[i].links; ++j)
			if ((ll[j].flags & MEDIA_LNK_FL_ENABLED) && !(ll[j].flags & MEDIA_LNK_FL_IMMUTABLE)) {
				errno = EBUSY; goto done;
			}
	}
	if (link_set(media, phy, 1, csid, 0, MEDIA_LNK_FL_ENABLED)) goto done;
	links = 1;
	if (link_set(media, csid, 1, vfe, 0, MEDIA_LNK_FL_ENABLED)) goto done;
	links = 2;
	sd[0] = open_node(sensor); sd[1] = open_node(phy);
	sd[2] = open_node(csid); sd[3] = open_node(vfe);
	for (int i = 0; i < 4; ++i) if (sd[i] < 0) goto done;
	if (set_fmt(sd[0], 0) || set_fmt(sd[1], 0) || set_fmt(sd[1], 1) ||
	    set_fmt(sd[2], 0) || set_fmt(sd[2], 1) || set_fmt(sd[3], 0) ||
	    set_fmt(sd[3], 1)) goto done;
	video = open_node(vid);
	if (video < 0) goto done;
	fmt.fmt.pix_mp.width = 1280; fmt.fmt.pix_mp.height = 720;
	fmt.fmt.pix_mp.pixelformat = V4L2_PIX_FMT_UYVY;
	fmt.fmt.pix_mp.field = V4L2_FIELD_NONE;
	if (xio(video, VIDIOC_S_FMT, &fmt)) goto done;
	printf("VIDEO %ux%u fourcc=%08x stride=%u size=%u planes=%u\n",
	       fmt.fmt.pix_mp.width, fmt.fmt.pix_mp.height, fmt.fmt.pix_mp.pixelformat,
	       fmt.fmt.pix_mp.plane_fmt[0].bytesperline, fmt.fmt.pix_mp.plane_fmt[0].sizeimage,
	       fmt.fmt.pix_mp.num_planes);
	if (fmt.fmt.pix_mp.width != 1280 || fmt.fmt.pix_mp.height != 720 ||
	    fmt.fmt.pix_mp.pixelformat != V4L2_PIX_FMT_UYVY || fmt.fmt.pix_mp.num_planes != 1 ||
	    fmt.fmt.pix_mp.plane_fmt[0].bytesperline != 2560) { errno = EINVAL; goto done; }
	if (xio(video, VIDIOC_REQBUFS, &req) || req.count < 3 || req.count > 4) goto done;
	for (unsigned int i = 0; i < req.count; ++i) {
		struct v4l2_plane plane = {0};
		struct v4l2_buffer b = { .index = i, .type = type, .memory = V4L2_MEMORY_MMAP,
			.length = 1, .m.planes = &plane };
		if (xio(video, VIDIOC_QUERYBUF, &b)) goto done;
		lengths[i] = plane.length;
		if (plane.length < 1280*720*2 || plane.length > 8*1024*1024) { errno = EINVAL; goto done; }
		mem[i] = mmap(NULL, plane.length, PROT_READ | PROT_WRITE, MAP_SHARED, video, plane.m.mem_offset);
		if (mem[i] == MAP_FAILED) goto done;
		memset(mem[i], 0xa5, plane.length);
		if (xio(video, VIDIOC_QBUF, &b)) goto done;
	}
	if (!camera_policy_allowed(automatic)) { result = 0; goto done; }
	if (xio(video, VIDIOC_STREAMON, (void *)&type)) goto done;
	stream = 1;
	while (!interrupted && camera_policy_allowed(automatic)) {
		struct pollfd p = { .fd = video, .events = POLLIN };
		struct v4l2_plane plane = {0};
		struct v4l2_buffer b = { .type = type, .memory = V4L2_MEMORY_MMAP,
			.length = 1, .m.planes = &plane };
		int n = poll(&p, 1, 2000);
		if (n <= 0) { if (!n) errno = ETIMEDOUT; goto done; }
		if (xio(video, VIDIOC_DQBUF, &b)) goto done;
		if (b.index >= req.count || plane.bytesused > lengths[b.index] ||
		    plane.bytesused < 1280*720*2 || plane.data_offset || (b.flags & V4L2_BUF_FLAG_ERROR)) {
			errno = EIO; goto done;
		}
		const unsigned char *data = mem[b.index];
		/* Native capture1280x720p25; lightweight diagnostic view640x360p12.5.
		 * Initial eight frames allow the analogue decoder to settle. */
		if (++frames >= 8 && !(frames & 1)) {
			uint32_t frame_header[] = {htonl(b.sequence), htonl(PREVIEW_BYTES)};
			preview_convert(data, rgb);
			if (write_full(client, frame_header, sizeof(frame_header)) ||
			    write_full(client, rgb, PREVIEW_BYTES)) {
				if (errno == EPIPE || errno == ECONNRESET) result = 0;
				goto done;
			}
			if (frames == 8 || !(frames % 250))
				printf("PREVIEW_FRAME native_seq=%u bytes=%u\n", b.sequence, PREVIEW_BYTES);
		}
		if (xio(video, VIDIOC_QBUF, &b)) goto done;
	}
	result = 0;
done:
	if (result) fprintf(stderr, "CAPTURE_FAIL errno=%d %s frames=%u\n", errno, strerror(errno), frames);
	alarm(0);
	if (stream && xio(video, VIDIOC_STREAMOFF, (void *)&type)) { perror("STREAMOFF"); result = 1; }
	if (video >= 0) close(video);
	for (int i = 0; i < 4; ++i) {
		if (mem[i] != MAP_FAILED) munmap(mem[i], lengths[i]);
		if (sd[i] >= 0) close(sd[i]);
	}
	if (links >= 2 && link_set(media, csid, 1, vfe, 0, 0)) { perror("unlink RDI"); result = 1; }
	if (links >= 1 && link_set(media, phy, 1, csid, 0, 0)) { perror("unlink CSID"); result = 1; }
	if (media >= 0) close(media);
	free(rgb);
	printf("CAPTURE_DONE result=%d frames=%u stopped=1\n", result, frames);
	return result;
}

int main(int argc, char **argv)
{
	char *end;
	unsigned long allowed, policy_uid;
	int reverse_line = -1;
	struct sockaddr_un addr = {.sun_family = AF_UNIX};
	const char name[] = "bengal-camera-preview-v1";
	struct sigaction sa = {.sa_handler = stop};
	int server;
	char camera_text[16], power_text[16], reverse_text[PROP_VALUE_MAX];
	char *configured[] = {argv[0], "--uid", camera_text, "--policy-uid", power_text,
		"--reverse-line", "42", NULL};
	if (argc == 1) {
		unsigned int camera_uid, power_uid;
		char property[PROP_VALUE_MAX];
		FILE *packages;
		int log_fd;
		__system_property_get("persist.sys.bengal.camera_service", property);
		if (strcmp(property, "true")) return 2;
		__system_property_get("persist.sys.bengal.acc_power_policy", property);
		if (strcmp(property, "true")) return 2;
		packages = fopen("/data/system/packages.list", "re");
		if (!packages) return 2;
		int rc = camera_package_uids(packages, &camera_uid, &power_uid);
		fclose(packages);
		if (rc) return 2;
		__system_property_get("persist.sys.bengal.camera_reverse_line", reverse_text);
		if (*reverse_text && strcmp(reverse_text, "-1") && strcmp(reverse_text, "42")) return 2;
		log_fd = open("/data/misc/bengal-camera/bridge.log",
			O_WRONLY | O_CREAT | O_TRUNC | O_CLOEXEC | O_NOFOLLOW, 0600);
		if (log_fd < 0) return 2;
		if (dup2(log_fd, STDOUT_FILENO) < 0 || dup2(log_fd, STDERR_FILENO) < 0) {
			close(log_fd); return 2;
		}
		close(log_fd);
		snprintf(camera_text, sizeof(camera_text), "%u", camera_uid);
		snprintf(power_text, sizeof(power_text), "%u", power_uid);
		argv = configured;
		argc = !strcmp(reverse_text, "42") ? 7 : 5;
	}
	if ((argc != 5 && argc != 7) || strcmp(argv[1], "--uid") || strcmp(argv[3], "--policy-uid")) {
		fprintf(stderr, "usage: %s --uid CAMERA_APP_UID --policy-uid BENGAL_POWER_UID [--reverse-line 42]\n", argv[0]);
		return 2;
	}
	errno = 0; allowed = strtoul(argv[2], &end, 10);
	if (errno || *end || allowed < 10000 || allowed > 19999 || geteuid() != 0) return 2;
	errno = 0; policy_uid = strtoul(argv[4], &end, 10);
	if (errno || *end || policy_uid < 10000 || policy_uid > 19999 || policy_uid == allowed) return 2;
	if (argc == 7) {
		if (strcmp(argv[5], "--reverse-line") || strcmp(argv[6], "42")) return 2;
		reverse_line = 42;
	}
	setvbuf(stdout, NULL, _IONBF, 0);
	sigaction(SIGINT, &sa, NULL); sigaction(SIGTERM, &sa, NULL);
	sigaction(SIGALRM, &sa, NULL);
	server = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (server < 0) { perror("socket"); return 1; }
	memcpy(addr.sun_path + 1, name, sizeof(name) - 1);
	if (bind(server, (struct sockaddr *)&addr,
		 offsetof(struct sockaddr_un, sun_path) + sizeof(name)) || listen(server, 1)) {
		perror("bind/listen"); close(server); return 1;
	}
	printf("BENGAL_CAMERA_PREVIEW_READY uid=%lu; idle until authorized client\n", allowed);
	if (camera_policy_start((unsigned int)policy_uid, reverse_line)) { perror("camera policy"); close(server); return 1; }
	while (!interrupted) {
		struct pollfd p = {.fd = server, .events = POLLIN};
		struct ucred peer;
		socklen_t len = sizeof(peer);
		struct timeval timeout = {.tv_sec = 2};
		uint32_t request = 0;
		int n = poll(&p, 1, 1000), client;
		if (n < 0 && errno == EINTR) continue;
		if (n < 0) break;
		if (!n) continue;
		client = accept4(server, NULL, NULL, SOCK_CLOEXEC);
		if (client < 0) continue;
		if (getsockopt(client, SOL_SOCKET, SO_PEERCRED, &peer, &len) ||
		    len != sizeof(peer) || (unsigned long)peer.uid != allowed) {
			printf("PREVIEW_REJECT peer not allowed\n"); close(client); continue;
		}
		if (setsockopt(client, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) ||
		    setsockopt(client, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout))) {
			close(client); continue;
		}
		if (recv(client, &request, sizeof(request), MSG_WAITALL) == sizeof(request)
		    && (ntohl(request) == 0x42435031 || ntohl(request) == 0x42435231)
		    && camera_policy_begin(ntohl(request) == 0x42435231)) {
			printf("PREVIEW_BEGIN uid=%u pid=%d\n", peer.uid, peer.pid);
			capture(client, ntohl(request) == 0x42435231);
			camera_policy_streaming(false);
		}
		close(client);
		printf("PREVIEW_IDLE descriptors released\n");
	}
	close(server);
	camera_policy_stop();
	return 0;
}

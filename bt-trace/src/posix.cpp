// SPDX-License-Identifier: Apache-2.0
#include "bt_trace.h"

#include <dirent.h>
#include <fcntl.h>
#include <linux/magic.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/utsname.h>
#include <unistd.h>
#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <limits>

namespace bttrace {
namespace {
constexpr const char* kTracefs = "/sys/kernel/tracing";
constexpr const char* kOutput = "/dev/xyauto-bt-trace";

bool failed(IoError& error, const char* operation, int number, const std::string& detail = {}) {
    if (error.operation.empty()) error = {operation, number, detail};
    return false;
}

// Resolve every component through held directory FDs. O_NOFOLLOW on only the
// last component would still follow a substituted parent directory symlink.
int beneath(int root, const std::string& path, int flags, IoError& error, mode_t mode = 0) {
    int dir = fcntl(root, F_DUPFD_CLOEXEC, 3);
    if (dir < 0) { failed(error, "fcntl-dup", errno); return -1; }
    if (path.empty() || path[0] == '/') {
        failed(error, "path-check", 0, "empty-or-absolute"); close(dir); return -1;
    }
    size_t pos = 0;
    for (;;) {
        const auto slash = path.find('/', pos);
        const std::string part = path.substr(pos, slash == std::string::npos ? slash : slash - pos);
        if (part.empty() || part == "." || part == "..") {
            failed(error, "path-check", 0, "invalid-component"); close(dir); return -1;
        }
        int next = openat(dir, part.c_str(), O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK |
                          (slash == std::string::npos ? flags : O_RDONLY | O_DIRECTORY), mode);
        if (next < 0) failed(error, "openat", errno, part);
        close(dir); // never use errno after a descriptor close
        if (next < 0 || slash == std::string::npos) return next;
        dir = next; pos = slash + 1;
    }
}
int absolute_dir(const char* path, IoError& error) {
    int root = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (root < 0) { failed(error, "open-root", errno); return -1; }
    int fd = beneath(root, path + 1, O_RDONLY | O_DIRECTORY, error);
    close(root);
    return fd;
}
bool private_file(int fd, IoError& error) {
    struct stat st{};
    if (fstat(fd, &st)) return failed(error, "fstat", errno);
    return (S_ISREG(st.st_mode) && st.st_uid == 0 && st.st_nlink == 1 && (st.st_mode & 0777) == 0600) ||
           failed(error, "private-file-check", 0, "type-owner-links-mode");
}
bool write_once(int fd, const std::string& value, IoError& error) {
    ssize_t count = ::write(fd, value.data(), value.size());
    if (count < 0) return failed(error, "write", errno);
    return size_t(count) == value.size() || failed(error, "write-short", 0,
            std::to_string(count) + "/" + std::to_string(value.size()));
}
class PosixTrace final : public TraceIo {
    int root_ = -1, instances_ = -1, output_ = -1, lock_ = -1;
    const size_t trace_cap_;
    std::string owned_name_;
    Clock clock_;
    IoError error_;
public:
    explicit PosixTrace(Mode mode = Mode::Legacy) : trace_cap_(trace_export_cap(mode)) {}
    IoError error() const override { return error_; }
    bool dma_d84_target() override {
        error_ = {};
        utsname name{};
        if (uname(&name)) return failed(error_, "uname", errno);
        const long page_size = sysconf(_SC_PAGESIZE);
        return dma_d84_target_matches(name.release, name.machine, page_size) ||
                failed(error_, "target-check", 0, "requires-d84-aarch64-4096;not-image-sha");
    }
    ~PosixTrace() override {
        for (int fd : {lock_, output_, instances_, root_}) if (fd >= 0) close(fd);
        // Destruction only closes FDs. It is NOT advertised as trace cleanup.
    }
    bool open_and_lock() override {
        error_ = {};
        root_ = absolute_dir(kTracefs, error_);
        struct statfs fs{};
        if (root_ < 0) return false;
        if (fstatfs(root_, &fs)) return failed(error_, "fstatfs-tracefs", errno);
        if (fs.f_type != TRACEFS_MAGIC) return failed(error_, "tracefs-magic-check", 0);
        instances_ = beneath(root_, "instances", O_RDONLY | O_DIRECTORY, error_);
        output_ = absolute_dir(kOutput, error_);
        struct stat st{};
        if (instances_ < 0 || output_ < 0) return false;
        if (fstatfs(output_, &fs)) return failed(error_, "fstatfs-output", errno);
        if (fs.f_type != TMPFS_MAGIC) return failed(error_, "tmpfs-magic-check", 0);
        if (fstat(output_, &st)) return failed(error_, "fstat-output", errno);
        if (!S_ISDIR(st.st_mode) || st.st_uid != 0 || (st.st_mode & 0777) != 0700)
            return failed(error_, "output-directory-check", 0, "type-owner-mode");
        lock_ = beneath(output_, "owner.lock", O_RDWR | O_CREAT, error_, 0600);
        return lock_ >= 0 && private_file(lock_, error_) &&
               (flock(lock_, LOCK_EX | LOCK_NB) == 0 || failed(error_, "flock", errno));
    }
    bool create(const std::string& name) override {
        error_ = {};
        if (name.find('/') != std::string::npos) return failed(error_, "name-check", 0);
        if (mkdirat(instances_, name.c_str(), 0700)) return failed(error_, "mkdirat", errno);
        owned_name_ = name;
        return true;
    }
    bool remove(const std::string& name) override {
        error_ = {};
        if (name.find('/') != std::string::npos) return failed(error_, "name-check", 0);
        return unlinkat(instances_, name.c_str(), AT_REMOVEDIR) == 0 || failed(error_, "unlinkat", errno);
    }
    Read read(const std::string& path, size_t cap, uint64_t end) override {
        Read result;
        // Only the explicitly selected DMA owner's exact trace may exceed the
        // generic read limit. Setup, global events and profiles stay bounded
        // by kTraceCap, including when the owner is in DMA mode.
        const bool dma_trace = !owned_name_.empty() && trace_cap_ == kDmaD84TraceCap &&
                path == "instances/" + owned_name_ + "/trace" && cap == trace_cap_;
        if (clock_.now() >= end || !cap || (cap > kTraceCap && !dma_trace)) {
            failed(result.error, "read-budget-check", 0); return result;
        }
        int fd = beneath(root_, path, O_RDONLY, result.error);
        if (fd < 0) return result;
        char chunk[4096];
        while (clock_.now() < end) {
            const size_t want = std::min(sizeof(chunk), cap + 1 - result.text.size());
            ssize_t n = ::read(fd, chunk, want);
            if (n == 0) { result.ok = true; break; }
            if (n < 0) { if (errno == EINTR) continue; failed(result.error, "read", errno); break; }
            result.text.append(chunk, size_t(n));
            if (result.text.size() > cap) {
                result.text.resize(cap); result.truncated = true; result.ok = true; break;
            }
        }
        if (!result.ok && result.error.operation.empty()) failed(result.error, "read-deadline", 0);
        close(fd);
        return result;
    }
    bool write(const std::string& path, const std::string& value) override {
        error_ = {};
        // One definition/control value per syscall, never truncate the global
        // event file, never retry a partial registration whose effect is unknown.
        // trace's write callback is a no-op; only O_TRUNC clears its ring.
        // Apply it exclusively to the newly owned instance's trace, never to
        // kprobe_events or the global trace.
        bool clear = path.compare(0, 10, "instances/") == 0 &&
                     path.size() > 6 && path.compare(path.size() - 6, 6, "/trace") == 0;
        int fd = beneath(root_, path, O_WRONLY | (clear ? O_TRUNC : O_APPEND), error_);
        if (fd < 0) return false;
        bool ok = write_once(fd, value, error_);
        close(fd);
        return ok;
    }
    std::vector<std::string> cpus() override {
        error_ = {};
        std::vector<std::string> names;
        int fd = beneath(root_, "per_cpu", O_RDONLY | O_DIRECTORY, error_);
        if (fd < 0) return names;
        DIR* directory = fdopendir(fd);
        if (!directory) { failed(error_, "fdopendir", errno); close(fd); return names; }
        for (;;) {
            errno = 0;
            dirent* entry = readdir(directory);
            const int number = entry ? 0 : errno;
            if (!entry) {
                if (number) { failed(error_, "readdir", number); names.clear(); }
                break;
            }
            std::string name = entry->d_name;
            if (name.size() > 3 && name.compare(0, 3, "cpu") == 0 &&
                name.find_first_not_of("0123456789", 3) == std::string::npos) names.push_back(name);
            if (names.size() > 256) { failed(error_, "cpu-count-check", 0, "over-256"); names.clear(); break; }
        }
        closedir(directory);
        std::sort(names.begin(), names.end());
        return names;
    }
    bool save(const std::string& name, const std::string& value, uint64_t end) override {
        error_ = {};
        const size_t cap = !owned_name_.empty() && name == owned_name_ + ".trace" ? trace_cap_ : kTraceCap;
        if (value.size() > cap || name.find('/') != std::string::npos || clock_.now() >= end)
            return failed(error_, "save-budget-check", 0);
        int fd = beneath(output_, name, O_WRONLY | O_CREAT | O_EXCL, error_, 0600);
        if (fd < 0) return false;
        bool ok = private_file(fd, error_);
        size_t offset = 0;
        while (ok && offset < value.size() && clock_.now() < end) {
            ssize_t n = ::write(fd, value.data() + offset, std::min<size_t>(4096, value.size() - offset));
            if (n < 0 && errno == EINTR) continue;
            if (n <= 0) { ok = false; break; }
            offset += size_t(n);
        }
        close(fd);
        // Preserve partial evidence under its unique name, never overwrite it.
        return ok && offset == value.size();
    }
};
bool number(const char* text, unsigned& value) {
    if (!text || !*text || strspn(text, "0123456789") != strlen(text)) return false;
    errno = 0;
    char* end = nullptr;
    unsigned long n = strtoul(text, &end, 10);
    if (errno || *end || n > std::numeric_limits<unsigned>::max()) return false;
    value = unsigned(n); return true;
}
} // namespace

int production_main(int argc, char** argv) {
    if (argc != 4) return 64;
    unsigned value;
    if (!number(argv[3], value)) return 64;
    Endpoint endpoint;
    Clock clock;
    if (!strcmp(argv[1], "wait") && !strcmp(argv[2], "--budget-ms") && value == 2000) {
        Packet receipt;
        bool armed = wait_for_owner(endpoint, clock, &receipt);
        if (armed) {
            printf("BTD ARMED release=modprobe nonce=%016llx%016llx D=%llu C=%llu on=%llu readback=%llu mask=%llu\n",
                   (unsigned long long)receipt.nonce[0], (unsigned long long)receipt.nonce[1],
                   (unsigned long long)receipt.deadline, (unsigned long long)receipt.capture,
                   (unsigned long long)receipt.enabled, (unsigned long long)receipt.readback,
                   (unsigned long long)receipt.mask);
            if (receipt.mask == kDmaD84ProbeMask)
                printf("BTD receipt mode=capture-dma-d84 schema=d84-vendor-v3 mask=16383 trace_export_cap_bytes=%zu\n",
                       kDmaD84TraceCap);
        } else puts("BTD NOT-CONFIRMED release=modprobe off=unknown cleanup=unknown");
        return armed ? 0 : 1;
    }
    const bool dma_d84 = !strcmp(argv[1], "capture-dma-d84");
    if ((!strcmp(argv[1], "capture") || dma_d84) && !strcmp(argv[2], "--seconds") && value >= 1 && value <= 120) {
        // This is Android init's inherited socket descriptor, not a path or test
        // injection. Validate its type, listening state, and fixed bound name.
        unsigned fd;
        if (!number(getenv("ANDROID_SOCKET_bengal_bt_trace"), fd) || fd > INT32_MAX) return 1;
        endpoint.listener = int(fd);
        const Mode mode = dma_d84 ? Mode::DmaD84 : Mode::Legacy;
        PosixTrace io(mode);
        Result r = capture(value, endpoint, io, clock, mode);
        fputs(summarize(r).c_str(), stdout);
        return r.armed && r.off && r.disabled && r.exported && r.cleanup && r.statistics && r.status_saved ? 0 : 1;
    }
    return 64;
}
} // namespace bttrace

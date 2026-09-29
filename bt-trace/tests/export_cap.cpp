// SPDX-License-Identifier: Apache-2.0
// Real owner/socket/timer + actual POSIX read/save loops on ordinary files.
// Only root resolution and filesystem/UID metadata are substituted; this is
// not a tracefs, Android DAC/SELinux, or kernel-registration acceptance test.
#define main legacy_desk_main
#include "desk.cpp"
#undef main
#include <sys/statfs.h>
#include <linux/magic.h>

namespace {
std::string fixture_root;
unsigned filesystem_checks = 0;
int fixture_open(const char* path, int flags) {
    return ::open(strcmp(path, "/") == 0 ? fixture_root.c_str() : path, flags);
}
int fixture_fstatfs(int fd, struct statfs* st) {
    int rc = ::fstatfs(fd, st);
    if (!rc) st->f_type = filesystem_checks++ == 0 ? TRACEFS_MAGIC : TMPFS_MAGIC;
    return rc;
}
int fixture_fstat(int fd, struct stat* st) {
    int rc = ::fstat(fd, st);
    if (!rc) st->st_uid = 0;
    return rc;
}
}
#define open(path, flags) fixture_open(path, flags)
#define fstatfs(fd, st) fixture_fstatfs(fd, st)
#define fstat(fd, st) fixture_fstat(fd, st)
#include "../src/posix.cpp"
#undef open
#undef fstatfs
#undef fstat

namespace {
class ExportIo final : public TraceIo {
    Fake state_;
    PosixTrace posix_;
    const Mode mode_;
    const std::string payload_;
    std::string name_;
    uint64_t cleanup_end_ = 0;
public:
    ExportIo(const std::string& dir, Mode mode, size_t bytes, bool delay)
        : state_(dir, delay ? "export-delay" : ""), posix_(mode), mode_(mode),
          payload_(std::string(bytes - 1, 'X') + "\n") {
        fixture_root = dir + "/fs";
        filesystem_checks = 0;
        for (const auto& path : {"", "/sys", "/sys/kernel", "/sys/kernel/tracing",
                                "/sys/kernel/tracing/instances", "/dev", "/dev/xyauto-bt-trace"})
            require(mkdir((fixture_root + path).c_str(), 0700) == 0, "fixture directory");
    }
    bool dma_d84_target() override { return state_.dma_d84_target(); }
    bool open_and_lock() override { return state_.open_and_lock() && posix_.open_and_lock(); }
    bool create(const std::string& name) override {
        name_ = name;
        return state_.create(name) && posix_.create(name);
    }
    bool remove(const std::string& name) override {
        // Ordinary files do not disappear with tracefs instance removal.
        require(unlink((fixture_root + "/sys/kernel/tracing/instances/" + name + "/trace").c_str()) == 0,
                "remove own ordinary trace fixture");
        return state_.remove(name) && posix_.remove(name);
    }
    IoError error() const override { return posix_.error(); }
    Read read(const std::string& path, size_t cap, uint64_t end) override {
        Read semantic = state_.read(path, cap, end); // asserts OFF before trace read
        if (path == "instances/" + name_ + "/trace") {
            cleanup_end_ = end;
            require(cap == (mode_ == Mode::DmaD84 ? 1048576 : 262144), "owner selects exact mode cap");
            for (size_t bad : {size_t(0), size_t(262145), size_t(1048577), SIZE_MAX}) {
                auto rejected = posix_.read(path, bad, end);
                require(!rejected.ok && rejected.error.operation == "read-budget-check",
                        "invalid trace cap refused before filesystem operation");
            }
            auto expired = posix_.read(path, cap, 0);
            require(!expired.ok && expired.error.operation == "read-budget-check", "expired trace read refused");
            return posix_.read(path, cap, end);
        }
        require(cap <= kTraceCap, "unrelated owner read cap unchanged");
        if (path == "kprobe_events" || path == "kprobe_profile")
            require(cap == 262144, "global event/profile cap unchanged");
        else require(cap <= 32768, "status/setup cap unchanged");
        auto refused = posix_.read(path, 1048576, end);
        require(!refused.ok && refused.error.operation == "read-budget-check", "unrelated read cannot use DMA cap");
        return semantic;
    }
    bool write(const std::string& path, const std::string& value) override {
        if (path == "instances/" + name_ + "/trace")
            record(fixture_root + "/sys/kernel/tracing/" + path, payload_);
        return state_.write(path, value);
    }
    std::vector<std::string> cpus() override { return state_.cpus(); }
    bool save(const std::string& name, const std::string& value, uint64_t end) override {
        require(end == cleanup_end_, "trace/status saves retain original cleanup deadline");
        if (name == name_ + ".trace") {
            for (const auto& other : {name_ + ".status", std::string("foreign.trace"), std::string("missing/file")})
                require(!posix_.save(other, std::string(262145, 'X'), end) &&
                        posix_.error().operation == "save-budget-check", "unrelated save cap unchanged");
            require(!posix_.save(name, std::string(1048577, 'X'), end) &&
                    posix_.error().operation == "save-budget-check", "oversize trace save refused before create");
            if (mode_ == Mode::Legacy)
                require(!posix_.save(name, std::string(262145, 'X'), end) &&
                        posix_.error().operation == "save-budget-check", "legacy trace save keeps 256 KiB hard cap");
            require(!posix_.save(name, value, 0) && posix_.error().operation == "save-budget-check",
                    "expired save refused before create");
        }
        // The semantic fixture also models a slow exporter crossing the fixed
        // cleanup deadline. Its journal remains the ordering witness.
        const bool semantic = state_.save(name, value, end);
        const bool saved = posix_.save(name, value, end);
        return semantic && saved;
    }
};

void run_export(Mode mode, size_t bytes, bool delay = false) {
    const std::string label = std::string(mode == Mode::DmaD84 ? "dma" : "legacy") +
            std::to_string(bytes) + (delay ? "delay" : "");
    Run run(label);
    pid_t owner = fork(); require(owner >= 0, "fork export owner");
    if (!owner) {
        try {
            Clock clock;
            ExportIo io(run.dir, mode, bytes, delay);
            record(run.dir + "/result", summarize(capture(1, run.endpoint, io, clock, mode)));
            _exit(0);
        } catch (const std::exception& ex) { record(run.dir + "/error", ex.what()); _exit(91); }
    }
    const auto waiter = run.waiter();
    require(finish(waiter) == 0 && finish(owner) == 0, "actual owner+POSIX processes " + label + " " + file(run.dir + "/error"));
    const auto result = file(run.dir + "/result"), journal = file(run.dir + "/journal");
    require(result.find("armed=1") != std::string::npos && result.find("off=1") != std::string::npos, "armed and OFF");
    require(result.find("cleanup=" + std::to_string(!delay)) != std::string::npos &&
            result.find("exported=" + std::to_string(!delay)) != std::string::npos, "bounded export/cleanup result");
    const size_t cap = mode == Mode::DmaD84 ? 1048576 : 262144;
    require(result.find("truncated=" + std::to_string(bytes > cap)) != std::string::npos, "explicit truncation result");
    require((result.find("trace_export_cap_bytes=1048576") != std::string::npos) == (mode == Mode::DmaD84),
            "DMA cap receipt explicit, legacy receipt unchanged");
    const auto on = journal.find("ON_EFFECT"), off = journal.find("OFF_EFFECT", on);
    require(on < off && off < journal.find("TRACE_READ_AFTER_OFF"), "OFF before POSIX export");
    const std::string output = run.dir + "/fs/dev/xyauto-bt-trace";
    DIR* dir = opendir(output.c_str()); require(dir, "output directory");
    unsigned traces = 0;
    while (auto* entry = readdir(dir)) {
        std::string name = entry->d_name;
        if (name.size() < 6 || name.substr(name.size() - 6) != ".trace") continue;
        const auto trace = file(output + "/" + name);
        require(trace == (std::string(bytes - 1, 'X') + "\n").substr(0, cap), "exact POSIX output bytes");
        ++traces;
    }
    closedir(dir);
    require(traces == unsigned(!delay), "unique trace/no expired output");
    printf("PASS owner+POSIX %s input=%zu cap=%zu output=%zu truncated=%u\n", label.c_str(), bytes, cap,
           delay ? 0 : std::min(bytes, cap), unsigned(bytes > cap));
}
}
int main(int argc, char** argv) {
    try {
        require(argc == 2, "own scratch required"); root = argv[1];
        // An unopened adapter has no usable directory FD: budget-check here
        // proves refusal precedes even fcntl/openat (which would give EBADF).
        for (Mode mode : {Mode::Legacy, Mode::DmaD84}) {
            PosixTrace io(mode);
            for (size_t cap : {size_t(0), size_t(262145), size_t(1048576), size_t(1048577), SIZE_MAX}) {
                auto r = io.read("instances/foreign/trace", cap, Clock().now() + kSecond);
                require(!r.ok && r.error.operation == "read-budget-check", "unowned/invalid read cap before FD access");
            }
            require(!io.save("foreign.trace", std::string(262145, 'X'), Clock().now() + kSecond) &&
                    io.error().operation == "save-budget-check", "unowned oversize save before FD access");
            for (size_t bytes : {size_t(262244), size_t(524388), size_t(1048576), size_t(1048676)}) run_export(mode, bytes);
        }
        run_export(Mode::DmaD84, 262244, true);
        return 0;
    } catch (const std::exception& ex) { fprintf(stderr, "FAIL %s\n", ex.what()); return 1; }
}

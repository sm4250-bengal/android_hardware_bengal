// SPDX-License-Identifier: Apache-2.0
// Internal semantic OS adapter, linked only in the host test executable.
#include "bt_trace.h"
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <sys/stat.h>
#include <unistd.h>
#include <fcntl.h>
#include <dirent.h>
#include <signal.h>
#include <poll.h>
#include <algorithm>
#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <map>
#include <sstream>
#include <stdexcept>

using namespace bttrace;
namespace {
std::string root;
unsigned passed = 0;
void require(bool value, const std::string& what) {
    if (!value) throw std::runtime_error(what);
}
std::string file(const std::string& path) {
    std::ifstream in(path);
    return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}
void record(const std::string& path, const std::string& text) {
    int fd = open(path.c_str(), O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0 || write(fd, text.data(), text.size()) != ssize_t(text.size())) _exit(90);
    close(fd);
}
std::string name_of(const std::string& line) {
    size_t colon = line.find(':'), end = line.find(' ');
    return line.substr(colon + 1, end - colon - 1);
}
std::string clean(std::string s) {
    while (!s.empty() && (s.back() == '\n' || s.back() == '\r')) s.pop_back();
    return s;
}

// Narrow kernel-registration model for the deployed built-in + vendor symbol
// set. This consumes the production definition, not a second definition list.
// trace_kprobe validate_probe_symbol rejects ambiguous unqualified names;
// traceprobe_parse_btf_arg cannot resolve fields in a BTF-stripped module.
std::string vendor_registration_error(const std::string& def, bool loaded = true) {
    const auto begin = def.find(' ') + 1;
    const auto target = def.substr(begin, def.find(' ', begin) - begin);
    if (target == "qcom_geni_serial_start_tx_dma" || target == "qcom_geni_serial_tx_empty")
        return "NON_UNIQ_SYMBOL";
    if (target.rfind("bengal_bt_uart:", 0) == 0) {
        const auto symbol = target.substr(15);
        if (symbol != "qcom_geni_serial_start_tx_dma" && symbol != "qcom_geni_serial_tx_empty" &&
            symbol != "qcom_geni_serial_flush_buffer_dma") return "BAD_PROBE_ADDR";
        if (def.find("->") != std::string::npos) return "NO_BTF_ENTRY";
        if (!loaded && def.rfind("p:", 0) == 0 && def.find("$arg") != std::string::npos)
            return "NOFENTRY_ARGS";
    }
    return {};
}

class Fake final : public TraceIo {
    const std::string dir_, fault_;
    Clock clock_;
    std::map<std::string, std::string> controls_;
    std::vector<std::string> global_{"p:kprobes/foreign existing_symbol x=$arg1:x64"};
    std::string base_;
    unsigned registered_ = 0, off_writes_ = 0;
    bool on_ = false, altered_ = false;
    bool diagnostic_fired_ = false;
    IoError error_;
    bool diagnostic(const std::string& operation, const std::string& path) {
        const std::string target = "diag-" + operation + ":";
        if (diagnostic_fired_ || fault_.compare(0, target.size(), target) != 0) return false;
        const auto suffix = fault_.substr(target.size());
        if (path.size() < suffix.size() || path.compare(path.size() - suffix.size(), suffix.size(), suffix) != 0) return false;
        diagnostic_fired_ = true;
        return true;
    }
    void event(const std::string& message) {
        record(dir_ + "/journal", std::to_string(clock_.now()) + " " + message + "\n");
    }
public:
    IoError error() const override { return error_; }
    bool dma_d84_target() override { event("TARGET_D84"); return fault_ != "wrong-target"; }
    Fake(std::string dir, std::string fault) : dir_(std::move(dir)), fault_(std::move(fault)) {}
    ~Fake() override {
        event("FINAL_ON=" + std::to_string(on_));
        for (const auto& def : global_) event("RETAINED " + def);
    }
    bool open_and_lock() override { event("OPEN"); return fault_ != "missing-tracefs" && fault_ != "lock-busy"; }
    bool create(const std::string& name) override {
        base_ = "instances/" + name + "/";
        if (fault_ == "instance-collision") { event("FOREIGN_INSTANCE_PRESERVED_ON"); return false; }
        if (fault_ == "event-collision") global_.push_back(definitions(name)[0]);
        on_ = true;
        controls_[base_ + "tracing_on"] = "1\n";
        controls_[base_ + "buffer_size_kb"] = "1408\n";
        controls_[base_ + "buffer_subbuf_size_kb"] = "4\n";
        controls_[base_ + "buffer_total_size_kb"] = "2048\n";
        controls_[base_ + "current_tracer"] = "nop\n";
        controls_[base_ + "trace_clock"] = "[local] global mono\n";
        controls_[base_ + "trace"] = "fresh garbage must clear";
        event("CREATE_ON"); return true;
    }
    bool remove(const std::string&) override {
        require(!on_, "fake refuses remove while ON");
        event("REMOVE_INSTANCE"); return true;
    }
    Read read(const std::string& path, size_t cap, uint64_t end) override {
        event("READ " + path);
        if ((on_ && diagnostic("read-on", path)) ||
            (path == "kprobe_events" && registered_ && diagnostic("read", "registered")))
            return {false, false, "", {"fixture-read", EACCES, "first-error"}};
        if (diagnostic("read", path)) return {false, false, "partial\n", {"fixture-read", EACCES, "first-error"}};
        if (diagnostic("mismatch", path)) return {true, false, "wrong\n"};
        if (diagnostic("truncated", path)) return {true, true, std::string(300, 'x')};
        for (const auto& item : std::vector<std::pair<std::string, std::string>>{
                 {"buffer_size_kb", "buffer-value:"}, {"buffer_subbuf_size_kb", "subbuffer-value:"}}) {
            if (path == base_ + item.first && fault_.compare(0, item.second.size(), item.second) == 0) {
                const auto value = fault_.substr(item.second.size());
                return {true, value.size() > cap, value.substr(0, cap)};
            }
        }
        if (clock_.now() >= end) return {};
        if (path == base_ + "buffer_subbuf_size_kb" && fault_ == "late-subbuffer") usleep(2100000);
        if (path == base_ + "tracing_on" && on_ && fault_ == "late-readback") usleep(2100000);
        if (path == "kprobe_events" && registered_ >= 2 && fault_ == "registration-readback-fail") return {};
        std::string value;
        if (path == "kprobe_events") {
            if (registered_ == 2 && fault_ == "append-collision" && !altered_) {
                // Linux accepts multiple probes under a single event name.
                // Same field layout, distinct target. A different field layout
                // would be rejected rather than exercising append semantics.
                std::string extra = global_.back();
                size_t symbol = extra.find(' ') + 1;
                extra.replace(symbol, extra.find(' ', symbol) - symbol, "foreign_receive_buf");
                global_.push_back(extra);
                altered_ = true;
            }
            for (const auto& def : global_) value += def + "\n";
        } else if (path == "kprobe_profile") {
            for (const auto& def : global_) value += name_of(def) + " 10 0\n";
        } else if (path.find("/stats") != std::string::npos) {
            value = "entries: 10\noverrun: 0\ncommit overrun: 0\ndropped events: 0\n";
            if (fault_ == "statistics-fail") return {};
        } else if (path == base_ + "trace") {
            require(!on_, "export read while ON"); event("TRACE_READ_AFTER_OFF");
            if (fault_ == "trace-read-fail") return {};
            value = fault_ == "trace-large" ? std::string(kTraceCap + 100, 'X') : "captured test trace\n";
        } else {
            auto found = controls_.find(path);
            if (found == controls_.end()) return {};
            value = found->second;
            if (path == base_ + "tracing_on" && !on_ && fault_ == "off-readback-fail") value = "1\n";
            if (path == base_ + "tracing_on" && off_writes_ == 1 && fault_ == "initial-readback-fail") return {};
        }
        bool truncated = value.size() > cap;
        if (truncated) value.resize(cap);
        return {true, truncated, value};
    }
    bool write(const std::string& path, const std::string& raw) override {
        std::string value = clean(raw);
        event("WRITE " + path + "=" + value);
        // A later cleanup call deliberately replaces the adapter's error and
        // libc errno. The frozen Result must retain the first setup error.
        error_ = {"fixture-cleanup", EBADF, "later-error"}; errno = EBADF;
        if ((value == "1" && diagnostic("write-on", path)) || diagnostic("write", path)) {
            error_ = {"fixture-write", EACCES, "first-error"}; return false;
        }
        if (path == "kprobe_events") {
            if (value.compare(0, 2, "-:") == 0) {
                const auto name = value.substr(2);
                global_.erase(std::remove_if(global_.begin(), global_.end(), [&](const auto& def) {
                    return name_of(def) == name;
                }), global_.end());
                event("DELETE " + name); return true;
            }
            if (fault_ == "vendor-symbols" || fault_ == "vendor-pending") {
                const auto failure = vendor_registration_error(value, fault_ != "vendor-pending");
                if (!failure.empty()) {
                    error_ = {"fixture-write", EINVAL, failure};
                    event("REGISTRATION_REJECT " + failure);
                    return false;
                }
            }
            ++registered_;
            // A successful write appends, never replaces; partial failure can
            // leave a side effect and must NOT become an owned-delete ledger.
            if ((fault_ == "registration-fail" && registered_ == 2) ||
                (fault_ == "dma-registration-fail" && registered_ == 10)) return false;
            global_.push_back(value);
            controls_[base_ + "events/" + name_of(value) + "/enable"] = "0\n";
            controls_[base_ + "events/" + name_of(value) + "/filter"] = "none\n";
            if ((fault_ == "partial-registration" && registered_ == 2) ||
                (fault_ == "dma-partial-registration" && registered_ == 10)) return false;
            return true;
        }
        if (!controls_.count(path)) return false;
        if (path == base_ + "tracing_on") {
            if (value == "0") {
                ++off_writes_;
                if (fault_ == "off-fail" || (fault_ == "stop-off-fail" && off_writes_ > 1)) return false;
                on_ = false; event("OFF_EFFECT");
            } else {
                if (fault_ == "late-on") usleep(2100000);
                on_ = true; event("ON_EFFECT");
            }
        }
        if (path == base_ + "buffer_size_kb" && fault_ == "late-setup") usleep(2100000);
        if (path.find("/enable") != std::string::npos && value == "1" && fault_ == "enable-fail") return false;
        if (path == base_ + "buffer_size_kb") {
            // Installed ACK d84cfa44: round bytes up to 4080-byte payload
            // pages (4K ARM64, 16-byte header), then report payload KiB.
            const auto bytes = std::stoul(value) * 1024;
            const auto pages = std::max(2UL, (bytes + 4079) / 4080);
            const auto reported = pages * 4080 / 1024;
            controls_[path] = std::to_string(reported) + "\n";
            controls_[base_ + "buffer_total_size_kb"] = std::to_string(reported * 8) + "\n";
            return true;
        }
        controls_[path] = path == base_ + "trace_clock" ? "local global [mono]\n" : raw;
        return true;
    }
    std::vector<std::string> cpus() override {
        if (diagnostic("list", "per_cpu")) { error_ = {"fixture-readdir", EIO, "first-error"}; return {}; }
        return {"cpu0", "cpu1", "cpu2", "cpu3", "cpu4", "cpu5", "cpu6", "cpu7"};
    }
    bool save(const std::string& name, const std::string& value, uint64_t end) override {
        require(!on_ || name.find(".status") != std::string::npos, "save trace while ON");
        event("SAVE " + name + " bytes=" + std::to_string(value.size()));
        if (name.find(".trace") != std::string::npos) {
            if (fault_ == "export-delay") usleep(1100000);
            if (fault_ == "export-full") return false;
            if (fault_ == "export-short") { record(dir_ + "/partial.trace", value.substr(0, 3)); return false; }
        }
        if (clock_.now() >= end) return false;
        record(dir_ + "/" + name, value); return true;
    }
};

struct Run {
    std::string dir;
    Endpoint endpoint;
    explicit Run(const std::string& label) {
        dir = root + "/" + label;
        require(mkdir(dir.c_str(), 0700) == 0, "fresh test directory " + dir);
        endpoint.socket = dir + "/socket";
        require(endpoint.socket.size() < sizeof(sockaddr_un::sun_path), "unix socket path too long");
        endpoint.peer.clear(); endpoint.uid = getuid();
        endpoint.listener = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        sockaddr_un address{}; address.sun_family = AF_UNIX;
        strcpy(address.sun_path, endpoint.socket.c_str());
        require(bind(endpoint.listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0 &&
                listen(endpoint.listener, 8) == 0, "init listening socket fixture");
    }
    ~Run() { close(endpoint.listener); }
    pid_t owner(const std::string& fault = "", unsigned delay_ms = 0, unsigned seconds = 1, Mode mode = Mode::Legacy) {
        pid_t pid = fork(); require(pid >= 0, "fork owner");
        if (!pid) {
            usleep(delay_ms * 1000);
            try {
                Clock clock;
                { Fake fake(dir, fault); Result r = capture(seconds, endpoint, fake, clock, mode);
                  record(dir + "/result", summarize(r)); }
                _exit(0);
            } catch (const std::exception& ex) { record(dir + "/error", ex.what()); _exit(91); }
        }
        return pid;
    }
    pid_t waiter() {
        pid_t pid = fork(); require(pid >= 0, "fork waiter");
        if (!pid) {
            Clock clock; Packet receipt;
            bool armed = wait_for_owner(endpoint, clock, &receipt);
            record(dir + "/waiter", std::string(armed ? "ARMED" : "NOT-CONFIRMED") +
                   " release=modprobe at=" + std::to_string(clock.now()) + "\n");
            // This is the observable unconditional next init-command marker.
            record(dir + "/modprobe-marker", "released\n");
            _exit(armed ? 0 : 1);
        }
        return pid;
    }
    int connect_client() {
        int fd = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
        sockaddr_un addr{}; addr.sun_family = AF_UNIX; strcpy(addr.sun_path, endpoint.socket.c_str());
        require(connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) == 0, "connect raw client");
        return fd;
    }
};
int finish(pid_t pid, unsigned max_ms = 5000) {
    Clock clock; uint64_t end = clock.now() + max_ms * kMs;
    int status;
    do {
        if (waitpid(pid, &status, WNOHANG) == pid) return WIFEXITED(status) ? WEXITSTATUS(status) : 128 + WTERMSIG(status);
        usleep(10000);
    } while (clock.now() < end);
    kill(pid, SIGKILL); waitpid(pid, &status, 0);
    throw std::runtime_error("test child exceeded absolute harness bound");
}
Packet request(unsigned lease_ms = 2000) {
    Clock clock; Packet p;
    p.entered = clock.now(); p.deadline = p.entered + lease_ms * kMs;
    require(random_nonce(p.nonce), "random nonce"); return p;
}
void sent(int fd, const Packet& p) { require(send(fd, &p, sizeof(p), MSG_NOSIGNAL) == sizeof(p), "send packet"); }
void test(const std::string& label, const std::string& fault, bool armed, const std::vector<std::string>& expected) {
    Run run(label);
    pid_t owner = run.owner(fault), waiter = run.waiter();
    require(finish(waiter) == (armed ? 0 : 1), label + " waiter result");
    if (armed) require(kill(owner, 0) == 0, label + " owner survives waiter exit");
    require(finish(owner) == 0, label + " owner process");
    auto result = file(run.dir + "/result"), journal = file(run.dir + "/journal");
    for (const auto& needle : expected) require(result.find(needle) != std::string::npos, label + " result lacks " + needle + ": " + result);
    require(file(run.dir + "/modprobe-marker") == "released\n", label + " unconditional release");
    require(journal.find("RETAINED p:kprobes/foreign existing_symbol x=$arg1:x64") != std::string::npos,
            label + " foreign event preserved");
    for (const char* global : {"tracing_on", "trace", "current_tracer", "buffer_size_kb", "buffer_subbuf_size_kb", "trace_clock"}) {
        require(journal.find(std::string("WRITE ") + global + "=") == std::string::npos, label + " no global control write");
    }
    if (journal.find("CREATE_ON") != std::string::npos) {
        size_t first = journal.find("WRITE ", journal.find("CREATE_ON"));
        require(journal.find("/tracing_on=0", first) < journal.find('\n', first), label + " first operation is OFF");
    }
    if (fault == "append-collision") {
        require(journal.find("RETAINED p:kprobes/btd_") != std::string::npos &&
                journal.find("foreign_receive_buf") != std::string::npos, label + " ambiguous event preserved");
    }
    if (fault == "partial-registration")
        require(journal.find("RETAINED p:kprobes/btd_") != std::string::npos, label + " failed append never deleted");
    if (fault.compare(0, 13, "buffer-value:") == 0 || fault.compare(0, 16, "subbuffer-value:") == 0 || fault == "late-subbuffer") {
        require(journal.find("ON_EFFECT") == std::string::npos &&
                journal.find("WRITE kprobe_events=") == std::string::npos,
                label + " rejected geometry cannot register or arm");
    }
    if (fault.compare(0, 5, "diag-") == 0) {
        DIR* dir = opendir(run.dir.c_str()); require(dir, "status fixture directory");
        std::string status;
        while (dirent* entry = readdir(dir)) {
            std::string name = entry->d_name;
            if (name.size() > 7 && name.substr(name.size() - 7) == ".status") status = file(run.dir + "/" + name);
        }
        closedir(dir);
        auto first = result.find(" setup_stage="), last = result.find(" kill_safe=");
        require(first != std::string::npos && last > first &&
                status.find(result.substr(first, last - first)) != std::string::npos,
                label + " saved status retains exact terminal diagnostic after cleanup");
    }
    if (armed && result.find("off=1") != std::string::npos && fault != "export-delay") {
        auto on = journal.find("ON_EFFECT");
        auto off = journal.find("OFF_EFFECT", on);
        auto export_at = journal.find("TRACE_READ_AFTER_OFF");
        if (export_at != std::string::npos) require(on < off && off < export_at, label + " autonomous OFF before export");
    }
    printf("PASS %s\n", label.c_str()); ++passed;
}
unsigned fd_count() {
    DIR* dir = opendir("/proc/self/fd"); require(dir, "fd inventory");
    unsigned n = 0; while (readdir(dir)) ++n; closedir(dir); return n;
}
void packet_tests() {
    int pair[2]; require(socketpair(AF_UNIX, SOCK_SEQPACKET, 0, pair) == 0, "packet pair");
    Endpoint endpoint; endpoint.peer.clear(); endpoint.uid = getuid();
    require(validate_socket(pair[0], false, endpoint), "positive peer identity");
    endpoint.uid += 1;
    require(!validate_socket(pair[0], false, endpoint), "reject wrong peer UID");
    endpoint.uid = getuid(); endpoint.peer = "u:r:wrong_domain:s0";
    require(!validate_socket(pair[0], false, endpoint), "reject wrong/unavailable peer security context");
    unsigned before = fd_count();
    int null = open("/dev/null", O_RDONLY | O_CLOEXEC); require(null >= 0, "null fixture");
    for (unsigned i = 0; i < 100; ++i) {
        Packet p = request();
        alignas(cmsghdr) char control[CMSG_SPACE(2 * sizeof(int))]{};
        iovec iov{&p, sizeof(p)}; msghdr msg{};
        msg.msg_iov = &iov; msg.msg_iovlen = 1; msg.msg_control = control; msg.msg_controllen = sizeof(control);
        auto* c = CMSG_FIRSTHDR(&msg); c->cmsg_level = SOL_SOCKET; c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(2 * sizeof(int)); int rights[] = {null, null}; memcpy(CMSG_DATA(c), rights, sizeof(rights));
        require(sendmsg(pair[0], &msg, 0) == sizeof(p), "send rights");
        require(!receive_packet(pair[1], p), "reject ancillary");
    }
    close(null); require(fd_count() == before, "no received descriptor leak");
    Packet p;
    char extra[sizeof(Packet) + 1]{};
    require(send(pair[0], extra, sizeof(extra), 0) == sizeof(extra) && !receive_packet(pair[1], p), "reject truncated long packet");
    require(send(pair[0], extra, 1, 0) == 1 && !receive_packet(pair[1], p), "reject short packet");
    close(pair[0]); close(pair[1]);
    for (int type : {SOCK_STREAM, SOCK_DGRAM}) {
        require(socketpair(AF_UNIX, type, 0, pair) == 0, "wrong socket fixture");
        Endpoint endpoint; endpoint.peer.clear(); endpoint.uid = getuid();
        require(!validate_socket(pair[0], false, endpoint), "reject non-seqpacket");
        close(pair[0]); close(pair[1]);
    }
    printf("PASS packet-shape-socket-type-and-100-rights-no-leak\n"); ++passed;
}
void lease_tests() {
    for (unsigned mode = 0; mode < 8; ++mode) {
        Run run("lease" + std::to_string(mode));
        pid_t owner = run.owner();
        int fd = run.connect_client(); Packet p = request();
        switch (mode) {
            case 0: p.version = 2; break;
            case 1: p.deadline = p.entered - 1; break;
            case 2: p.entered += 10 * kSecond; p.deadline = p.entered + kLease; break;
            case 3: p.entered = UINT64_MAX - kSecond; p.deadline = 1; break;
            case 4: p.deadline += kMs; break;
            case 5: p.nonce[0] = p.nonce[1] = 0; break;
            case 6: p.capture = p.deadline; break;
            case 7: p.entered -= 3 * kSecond; p.deadline -= 3 * kSecond; break;
        }
        sent(fd, p); close(fd);
        // A later good request is admitted only once within the original owner
        // window; the bad request itself creates no trace resource.
        pid_t waiter = run.waiter();
        require(finish(waiter) == 0 && finish(owner) == 0, "invalid lease rejected before good lease");
        require(file(run.dir + "/result").find("armed=1") != std::string::npos, "good lease positive control");
    }
    printf("PASS eight-invalid-leases-with-real-good-client-controls\n"); ++passed;
}
void reply_tests() {
    // Waiter validates hostile/malformed status through the same native wait
    // implementation using a separate real server process.
    for (unsigned mode = 0; mode < 7; ++mode) {
        Run run("reply" + std::to_string(mode));
        pid_t server = fork(); require(server >= 0, "reply server fork");
        if (!server) {
            pollfd listen{run.endpoint.listener, POLLIN, 0};
            if (poll(&listen, 1, 2000) != 1) _exit(92);
            int fd = accept4(run.endpoint.listener, nullptr, nullptr, SOCK_CLOEXEC);
            if (fd < 0) _exit(93);
            pollfd input{fd, POLLIN, 0}; poll(&input, 1, 2000);
            Packet p; if (!receive_packet(fd, p)) _exit(94);
            p.kind = 2; p.capture = p.entered + kSecond; p.enabled = p.readback = Clock().now(); p.mask = kProbeMask;
            switch (mode) {
                case 0: ++p.nonce[0]; break;
                case 1: ++p.version; break;
                case 2: ++p.deadline; break;
                case 3: p.readback = p.deadline + 1; break;
                case 4: p.mask = 1; break;
                case 5: p.readback += 100 * kMs; break;
                case 6: p.capture = p.readback; break;
            }
            send(fd, &p, sizeof(p), MSG_NOSIGNAL); close(fd); _exit(0);
        }
        auto waiter = run.waiter();
        require(finish(waiter) == 1 && finish(server) == 0, "waiter rejects malformed status " + std::to_string(mode));
    }
    printf("PASS seven-wrong-status-packets-through-process-waiter\n"); ++passed;
}
void timing_tests() {
    { Run run("no-client"); uint64_t start = Clock().now(); auto owner = run.owner();
      require(finish(owner) == 0 && Clock().now() - start < 3 * kSecond, "no-client finite exit");
      require(file(run.dir + "/journal").find("OPEN") == std::string::npos, "no client no trace resources"); }
    { Run run("delayed-owner"); const uint64_t entered = Clock().now(); auto waiter = run.waiter(); auto owner = run.owner("", 2200);
      require(finish(waiter) == 1 && Clock().now() - entered < 2200 * kMs, "waiter releases within 2s plus host scheduling margin");
      require(finish(owner) == 0, "delayed owner rejects expired queued lease");
      require(file(run.dir + "/journal").find("OPEN") == std::string::npos, "no delayed arm"); }
    { Run run("disconnect"); auto owner = run.owner(); int fd = run.connect_client(); sent(fd, request()); close(fd);
      require(finish(owner) == 0 && file(run.dir + "/result").find("armed=0") != std::string::npos, "EOF aborts admission"); }
    { Run run("no-extension"); auto owner = run.owner(); int fd = run.connect_client(); auto p = request(); sent(fd, p);
      pollfd watch{fd, POLLIN, 0}; require(poll(&watch, 1, 2000) == 1, "initial status");
      Packet reply; require(receive_packet(fd, reply) && reply.kind == 2, "initial ARMED");
      auto duplicate = request(); duplicate.deadline += kSecond;
      for (unsigned i = 0; i < 10000; ++i) if (send(fd, &duplicate, sizeof(duplicate), MSG_DONTWAIT | MSG_NOSIGNAL) < 0) break;
      require(finish(owner) == 0 && Clock().now() < p.entered + 1800 * kMs, "duplicate flood cannot extend timer"); close(fd); }
    { Run run("kill-limit"); auto owner = run.owner("", 0, 2); auto waiter = run.waiter();
      require(finish(waiter) == 0, "killed-owner initially armed"); kill(owner, SIGKILL);
      require(finish(owner) == 137, "owner SIGKILL observed");
      const auto journal = file(run.dir + "/journal");
      auto on = journal.rfind("ON_EFFECT"); require(on != std::string::npos && journal.find("OFF_EFFECT", on) == std::string::npos,
              "SIGKILL leaves ON fake state: explicit unsupported guarantee"); }
    printf("PASS real-no-client-delayed-EOF-duplicate-timer-and-SIGKILL-limitation\n"); ++passed;
}
void diagnostic_tests() {
    const std::vector<std::pair<std::string, std::string>> writes = {
        {"tracing_on", "initial-off-write"}, {"buffer_size_kb", "buffer-write"},
        {"trace_clock", "clock-write"}, {"/trace", "trace-clear"},
        {"kprobe_events", "registration-write"}, {"/enable", "event-enable-write"}
    };
    unsigned n = 0;
    for (const auto& item : writes) {
        test("dw" + std::to_string(n++), "diag-write:" + item.first, false,
             {"setup_stage=" + item.second, "setup_check=write-failed", "setup_op=fixture-write",
              "setup_errno=" + std::to_string(EACCES), "off=1"});
    }
    const std::vector<std::pair<std::string, std::string>> reads = {
        {"tracing_on", "initial-off-read"}, {"buffer_size_kb", "buffer-read"},
        {"buffer_subbuf_size_kb", "subbuffer-read"},
        {"current_tracer", "tracer-read"}, {"trace_clock", "clock-read"},
        {"buffer_total_size_kb", "buffer-total-read"}, {"/enable", "event-enable-read"}
    };
    n = 0;
    for (const auto& item : reads) {
        test("dr" + std::to_string(n++), "diag-read:" + item.first, false,
             {"setup_stage=" + item.second, "setup_check=read-failed", "setup_op=fixture-read",
              "setup_errno=" + std::to_string(EACCES), "setup_observed_hex=7061727469616c0a", "cleanup=1"});
    }
    n = 0;
    for (const auto& item : reads) {
        if (item.first == "buffer_total_size_kb") continue; // existing contract checks readability only
        test("dm" + std::to_string(n++), "diag-mismatch:" + item.first, false,
             {"setup_stage=" + item.second, "setup_errno=0", "setup_observed_hex=77726f6e670a", "cleanup=1"});
    }
    test("dcpu", "diag-list:per_cpu", false, {"setup_stage=cpu-list", "setup_op=fixture-readdir",
         "setup_errno=" + std::to_string(EIO), "cleanup=1"});
    test("dtrunc", "diag-truncated:current_tracer", false, {"setup_stage=tracer-read", "setup_check=truncated",
         "setup_read_truncated=1", "setup_observed_bytes=300", "setup_observed_capped=1", "cleanup=1"});
    test("dreg", "diag-read:registered", false, {"setup_stage=registration-read", "setup_check=exact-definition",
         "setup_op=fixture-read", "setup_errno=" + std::to_string(EACCES), "cleanup=1"});
    test("donw", "diag-write-on:tracing_on", false, {"setup_stage=on-write", "setup_check=write-failed",
         "setup_op=fixture-write", "setup_errno=" + std::to_string(EACCES), "cleanup=1"});
    test("donr", "diag-read-on:tracing_on", false, {"setup_stage=on-read", "setup_check=read-failed",
         "setup_op=fixture-read", "setup_errno=" + std::to_string(EACCES), "cleanup=1"});
    test("dgood", "", true, {"setup_stage=none", "armed=1", "off=1", "cleanup=1", "status_saved=1"});
}
void rounded_buffer_tests() {
    test("rounded", "", true, {"armed=1", "off=1", "cleanup=1", "status_saved=1"});
    DIR* dir = opendir((root + "/rounded").c_str()); require(dir, "rounded status directory");
    std::string status;
    while (dirent* entry = readdir(dir)) {
        const std::string name = entry->d_name;
        if (name.size() > 7 && name.substr(name.size() - 7) == ".status")
            status = file(root + "/rounded/" + name);
    }
    closedir(dir);
    require(status.find("buffer_requested_per_cpu_kb=256 buffer_per_cpu_kb=258 buffer_subbuf_kb=4 cpu_count=8 buffer_total_kb=2064") != std::string::npos,
            "status distinguishes requested and ACK-reported sizes");
    printf("PASS rounded buffer status reports request256 / observed258\n");
    unsigned n = 0;
    for (const auto& value : std::vector<std::string>{"X\n", "0\n", "255\n", "256\n", "257\n", "259\n", "260\n",
             "1048576\n", "", "0258\n", "+258\n", "258x\n", "258\n4\n", "258 (expanded: 1408)\n", std::string("258\0\n", 5)}) {
        test("bad-buffer" + std::to_string(n++), "buffer-value:" + value, false,
             {"setup_stage=buffer-read", "setup_check=readback-mismatch", "armed=0", "off=1", "cleanup=1"});
    }
    n = 0;
    for (const auto& value : std::vector<std::string>{"X\n", "0\n", "2\n", "8\n", "16\n", "", "+4\n", "4x\n", "4\n8\n"}) {
        test("bad-subbuffer" + std::to_string(n++), "subbuffer-value:" + value, false,
             {"setup_stage=subbuffer-read", "setup_check=readback-mismatch", "armed=0", "off=1", "cleanup=1"});
    }
    for (const auto& item : std::vector<std::pair<std::string, std::string>>{
             {"buffer_size_kb", "buffer-read"}, {"buffer_subbuf_size_kb", "subbuffer-read"}}) {
        test("trunc-" + item.first, "diag-truncated:" + item.first, false,
             {"setup_stage=" + item.second, "setup_check=truncated", "armed=0", "off=1", "cleanup=1"});
        test("unread-" + item.first, "diag-read:" + item.first, false,
             {"setup_stage=" + item.second, "setup_check=read-failed", "armed=0", "off=1", "cleanup=1"});
    }
    test("late-subbuffer", "late-subbuffer", false,
         {"setup_stage=subbuffer-read", "setup_check=deadline-or-peer", "armed=0", "off=1", "cleanup=1"});
    test("cap", "trace-large", true, {"armed=1", "off=1", "exported=1", "truncated=1", "cleanup=1"});
    dir = opendir((root + "/cap").c_str()); require(dir, "cap trace directory");
    std::string trace;
    while (dirent* entry = readdir(dir)) {
        const std::string name = entry->d_name;
        if (name.size() > 6 && name.substr(name.size() - 6) == ".trace")
            trace = file(root + "/cap/" + name);
    }
    closedir(dir);
    require(trace.size() == 262144, "stopped trace export remains capped at 256 KiB, not buffer capacity");
    printf("PASS %u rounded-buffer process groups; strict sizes, read failures, deadline and unchanged export cap\n", passed);
}
} // namespace

int main(int argc, char** argv) {
    try {
        require(argc == 2 || (argc == 3 && (std::string(argv[2]) == "replies" || std::string(argv[2]) == "diagnostics" || std::string(argv[2]) == "rounding")),
                "host test needs an isolated scratch directory"); root = argv[1];
        if (argc == 3) {
            if (std::string(argv[2]) == "diagnostics") diagnostic_tests();
            else if (std::string(argv[2]) == "rounding") rounded_buffer_tests();
            else reply_tests();
            return 0;
        }
        packet_tests();
        test("success", "", true, {"armed=1", "off=1", "exported=1", "statistics=1", "cleanup=1", "status_saved=1"});
        test("missing", "missing-tracefs", false, {"made=0", "armed=0"});
        test("locked", "lock-busy", false, {"made=0", "armed=0"});
        test("collision", "instance-collision", false, {"made=0", "armed=0"});
        test("event-collision", "event-collision", false, {"armed=0", "off=1", "cleanup=1"});
        test("initial-off", "off-fail", false, {"off=0", "exported=0", "cleanup=0"});
        test("initial-read", "initial-readback-fail", false, {"armed=0", "off=1", "cleanup=1"});
        test("register-fail", "registration-fail", false, {"armed=0", "off=1", "cleanup=0"});
        test("partial-register", "partial-registration", false, {"armed=0", "off=1", "cleanup=0"});
        test("register-read", "registration-readback-fail", false, {"armed=0", "off=1", "cleanup=0"});
        test("append", "append-collision", false, {"armed=0", "off=1", "cleanup=0"});
        test("enable", "enable-fail", false, {"armed=0", "off=1"});
        test("late-setup", "late-setup", false, {"armed=0", "off=1"});
        test("late-on", "late-on", false, {"armed=0", "off=1"});
        test("late-read", "late-readback", false, {"armed=0", "off=1"});
        test("stop-off", "stop-off-fail", true, {"armed=1", "off=0", "exported=0", "cleanup=0"});
        test("stop-read", "off-readback-fail", false, {"off=0", "exported=0", "cleanup=0"});
        test("large", "trace-large", true, {"off=1", "exported=1", "truncated=1", "cleanup=1"});
        test("read-fail", "trace-read-fail", true, {"off=1", "exported=0", "cleanup=1"});
        test("full", "export-full", true, {"off=1", "exported=0", "cleanup=1"});
        test("short", "export-short", true, {"off=1", "exported=0", "cleanup=1"});
        test("export-delay", "export-delay", true, {"off=1", "exported=0", "cleanup=0"});
        test("stats-fail", "statistics-fail", true, {"off=1", "statistics=0", "cleanup=1"});
        lease_tests(); reply_tests(); timing_tests();
        printf("PASS %u bounded semantic/process groups; no real tracefs operation\n", passed);
        return 0;
    } catch (const std::exception& ex) { fprintf(stderr, "FAIL %s\n", ex.what()); return 1; }
}

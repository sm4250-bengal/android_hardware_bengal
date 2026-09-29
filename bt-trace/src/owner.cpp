// SPDX-License-Identifier: Apache-2.0
#include "bt_trace.h"

#include <sys/random.h>
#include <sys/socket.h>
#include <sys/timerfd.h>
#include <sys/un.h>
#include <unistd.h>
#include <fcntl.h>
#include <poll.h>
#include <algorithm>
#include <cerrno>
#include <climits>
#include <cstring>
#include <sstream>
#include <time.h>

namespace bttrace {
namespace {
struct Fd {
    int n;
    explicit Fd(int fd = -1) : n(fd) {}
    ~Fd() { if (n >= 0) close(n); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
};
bool nonblock(int fd) {
    int flags = fcntl(fd, F_GETFL);
    return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}
bool ready(int fd, short events, uint64_t end, const Clock& clock) {
    while (clock.now() < end) {
        uint64_t now = clock.now();
        if (now >= end) break;
        pollfd p{fd, events, 0};
        int rc = poll(&p, 1, int(std::min<uint64_t>((end - now + kMs - 1) / kMs, 50)));
        if (rc > 0) return (p.revents & events) != 0;
        if (rc < 0 && errno != EINTR) break;
    }
    return false;
}
bool gone(int fd) {
    pollfd p{fd, POLLIN | POLLRDHUP, 0};
    if (poll(&p, 1, 0) < 0) return true;
    // Any additional client packet aborts pre-ON; never a second lease.
    return p.revents & (POLLIN | POLLHUP | POLLRDHUP | POLLERR | POLLNVAL);
}
bool header(const Packet& p, uint32_t kind) {
    return p.magic == 0x42544431 && p.version == 1 && p.kind == kind && !p.reserved &&
           (p.nonce[0] || p.nonce[1]);
}
bool lease(const Packet& p, uint64_t now, uint64_t latest) {
    return header(p, 1) && p.entered && p.entered <= now && now < p.deadline &&
           p.deadline > p.entered && p.deadline - p.entered <= kLease &&
           p.deadline <= latest && !p.capture && !p.enabled && !p.readback && !p.mask;
}
bool send_packet(int fd, const Packet& p) {
    return send(fd, &p, sizeof(p), MSG_DONTWAIT | MSG_NOSIGNAL) == sizeof(p);
}
std::string trim(const std::string& value) {
    size_t first = value.find_first_not_of(" \t\r\n");
    if (first == std::string::npos) return {};
    return value.substr(first, value.find_last_not_of(" \t\r\n") - first + 1);
}
std::string normalized(const std::string& line) {
    std::istringstream in(line);
    std::string word, out;
    while (in >> word) { if (!out.empty()) out += ' '; out += word; }
    return out;
}
std::string event_name(const std::string& def) {
    auto colon = def.find(':');
    return def.substr(colon + 1, def.find(' ') - colon - 1);
}
bool exact_definition(const Read& read, const std::string& definition) {
    if (!read.ok || read.truncated) return false;
    std::istringstream in(read.text);
    std::string line;
    size_t matches = 0;
    const std::string wanted = event_name(definition);
    while (std::getline(in, line)) {
        line = normalized(line);
        if (event_name(line) == wanted) {
            if (line != normalized(definition)) return false;
            ++matches;
        }
    }
    return matches == 1;
}
std::string prefix(uint64_t (&nonce)[2]) {
    char buf[40];
    snprintf(buf, sizeof(buf), "btd_%016llx%016llx",
             (unsigned long long)nonce[0], (unsigned long long)nonce[1]);
    return buf;
}
// Encode at most 128 input bytes: arbitrary readback cannot inject status lines
// or expand an already bounded status beyond its existing export budget.
std::string hex(const std::string& text) {
    constexpr char digits[] = "0123456789abcdef";
    std::string out;
    for (size_t i = 0; i < std::min<size_t>(text.size(), 128); ++i) {
        unsigned char c = text[i]; out += digits[c >> 4]; out += digits[c & 15];
    }
    return out.empty() ? "-" : out;
}
bool setup_failed(Result& result, const char* stage, const char* check,
                  const std::string& path = {}, const IoError& error = {},
                  const Read* read = nullptr, const std::string& expected = {}) {
    auto& f = result.setup_failure;
    if (!f.stage.empty()) return false; // cleanup/secondary failures never replace it
    f.stage = stage; f.check = check; f.path = path; f.error = error; f.expected = expected;
    if (read) {
        f.read_ok = read->ok; f.read_truncated = read->truncated;
        f.observed_bytes = read->text.size(); f.observed_capped = read->text.size() > 128;
        f.observed = hex(read->text);
    }
    return false;
}
} // namespace

uint64_t Clock::now() const {
    timespec ts{};
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) _exit(70);
    return uint64_t(ts.tv_sec) * kSecond + uint64_t(ts.tv_nsec);
}
bool random_nonce(uint64_t (&nonce)[2], IoError* error) {
    const ssize_t count = getrandom(nonce, sizeof(nonce), GRND_NONBLOCK);
    const int number = count < 0 ? errno : 0;
    const bool ok = count == sizeof(nonce) && (nonce[0] || nonce[1]);
    if (!ok && error) *error = {"getrandom", number,
            count < 0 ? "syscall-failed" : "short-or-zero-nonce"};
    return ok;
}
bool validate_socket(int fd, bool listening, const Endpoint& endpoint) {
    int type = 0, accept = 0;
    socklen_t size = sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &size) || type != SOCK_SEQPACKET)
        return false;
    size = sizeof(accept);
    if (getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &accept, &size) || bool(accept) != listening)
        return false;
    if (listening) {
        sockaddr_un addr{};
        size = sizeof(addr);
        if (getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &size) ||
            addr.sun_family != AF_UNIX || size <= offsetof(sockaddr_un, sun_path) ||
            strnlen(addr.sun_path, sizeof(addr.sun_path)) == sizeof(addr.sun_path) ||
            endpoint.socket != addr.sun_path) return false;
    } else {
        ucred cred{};
        size = sizeof(cred);
        if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &cred, &size) || size != sizeof(cred) ||
            cred.uid != endpoint.uid) return false;
        if (!endpoint.peer.empty()) {
            char context[256]{};
            size = sizeof(context);
            if (getsockopt(fd, SOL_SOCKET, SO_PEERSEC, context, &size) ||
                size > sizeof(context)) return false;
            if (size && !context[size - 1]) --size;
            if (endpoint.peer != std::string(context, size)) return false;
        }
    }
    return nonblock(fd);
}
bool receive_packet(int fd, Packet& packet) {
    // Linux closes excess SCM_RIGHTS on MSG_CTRUNC; close every delivered FD.
    alignas(cmsghdr) char control[CMSG_SPACE(256 * sizeof(int))]{};
    iovec iov{&packet, sizeof(packet)};
    msghdr msg{};
    msg.msg_iov = &iov; msg.msg_iovlen = 1;
    msg.msg_control = control; msg.msg_controllen = sizeof(control);
    ssize_t count = recvmsg(fd, &msg, MSG_DONTWAIT | MSG_CMSG_CLOEXEC);
    bool ancillary = false;
    if (count >= 0) {
        for (cmsghdr* c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
            ancillary = true;
            if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS &&
                c->cmsg_len >= CMSG_LEN(0)) {
                size_t num = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
                const int* rights = reinterpret_cast<const int*>(CMSG_DATA(c));
                for (size_t i = 0; i < num; ++i) close(rights[i]);
            }
        }
    }
    return count == sizeof(packet) && !(msg.msg_flags & (MSG_TRUNC | MSG_CTRUNC)) && !ancillary;
}

std::vector<std::string> definitions(const std::string& name) {
    // Explicit r128 avoids kernel-selected MAXACTIVE readback normalization.
    // $argN at return is SAVED ENTRY DATA on GKI92378ced, not live return regs.
    const std::string g = "kprobes/" + name;
    return {
        "r128:" + g + "_tx serdev_device_write_buf serdev=$arg1:x64 ctrl=serdev->ctrl:x64 requested=$arg3:u64 accepted=$retval:s32",
        "p:" + g + "_rx ttyport_receive_buf port=$arg1:x64 ctrl=port->client_data:x64 cp=$arg2:x64 count=$arg4:u64",
        "p:" + g + "_wait tty_wait_until_sent tty=$arg1:x64 port=tty->port:x64 state=tty->driver_data:x64 timeout=$arg2:s64",
        "r128:" + g + "_fifo uart_chars_in_buffer tty=$arg1:x64 state=tty->driver_data:x64 count=$retval:u32",
        "p:" + g + "_dma qcom_geni_serial_start_tx_dma uport=$arg1:x64 state=uport->state:x64 mapbase=uport->mapbase:x64 line=uport->line:u32",
        "r128:" + g + "_empty qcom_geni_serial_tx_empty uport=$arg1:x64 state=uport->state:x64 mapbase=uport->mapbase:x64 empty=$retval:u32",
        "p:" + g + "_close serdev_device_close serdev=$arg1:x64 ctrl=serdev->ctrl:x64"
    };
}

bool dma_d84_target_matches(const std::string& release, const std::string& machine, long page_size) {
    return release == kDmaD84Release && machine == "aarch64" && page_size == 4096;
}

std::vector<std::string> dma_d84_definitions(const std::string& name) {
    auto defs = definitions(name); // Preserve the legacy seven definitions verbatim.
    // Exact original/autocts vendor DWARF witnesses: uport=0, se=544,
    // tx_dma_addr=664 (u64), tx_remaining=720 (u32). These are RAM, not MMIO.
    // Match helper se to the UART event's uport + 544; do NOT subtract from
    // arbitrary non-UART helper callers and dereference an assumed container.
    // Root must verify Image 4da78c72... and the explicit compiled UART
    // profile in tests/dma-d84-source.py before deployment/capture.
    // The module deliberately has no BTF and its local names duplicate the
    // built-in driver. Qualification alone is insufficient: use witnessed
    // numeric RAM fetches instead of BTF field expressions for all four sites.
    const std::string fields = " dma_addr=+664($arg1):x64 remaining=+720($arg1):u32";
    const std::string uart = " uport=$arg1:x64 state=+208($arg1):x64 mapbase=+328($arg1):x64 line=+316($arg1):u32";
    // Before module load, the kernel cannot classify its entry as FENTRY and
    // rejects $arg1 there. Arm64 entry x0 is the same uport; return probes
    // MUST keep saved entry $arg1, not x0 which now holds the return value.
    const std::string entry_uart = " uport=%x0:x64 state=+208(%x0):x64 mapbase=+328(%x0):x64 line=+316(%x0):u32";
    const std::string entry_fields = " dma_addr=+664(%x0):x64 remaining=+720(%x0):u32";
    const std::string g = "kprobes/" + name;
    const std::string module = "bengal_bt_uart:";
    defs[4] = "p:" + g + "_dma " + module + "qcom_geni_serial_start_tx_dma" + entry_uart + entry_fields;
    defs[5] = "r128:" + g + "_empty " + module + "qcom_geni_serial_tx_empty" + uart + " empty=$retval:u32";
    defs.push_back("p:" + g + "_flush_in " + module + "qcom_geni_serial_flush_buffer_dma" + entry_uart + entry_fields);
    // $arg1 at return is saved ENTRY data; the pointed-to RAM is read at return.
    defs.push_back("r128:" + g + "_flush_out " + module + "qcom_geni_serial_flush_buffer_dma" + uart + fields);
    defs.push_back("p:" + g + "_prep_in geni_se_tx_dma_prep se=$arg1:x64 len=$arg3:u64 iova_ptr=$arg4:x64");
    defs.push_back("r128:" + g + "_prep_out geni_se_tx_dma_prep se=$arg1:x64 len=$arg3:u64 iova_ptr=$arg4:x64 dma_addr=+0($arg4):x64 ret=$retval:s32");
    // d84 unprep entry is BTI, before PACIASP; x30 is the unsigned caller LR.
    // symstr preserves caller+offset even when KASLR changes absolute pointers.
    // ISR-side unprep is a completion-path witness, NOT physical DMA proof.
    defs.push_back("p:" + g + "_unprep geni_se_tx_dma_unprep se=$arg1:x64 dma_addr=$arg2:x64 len=$arg3:u64 caller=%x30:symstr");
    // d84 serial_core can enqueue a byte but suppress start_tx before the
    // vendor callback. Observe its real stop flags, not tty.hw_stopped.
    // Built-in BTF supplies these fields. Omit an explicit type on tx_en:
    // the trace parser must extract the one-bit field, not its whole word.
    defs.push_back("p:" + g + "_gate_in __uart_start state=$arg1:x64 uport=state->uart_port:x64"
                  " mapbase=state->uart_port->mapbase:x64 flags=state->uart_port->flags:x64"
                  " tty=state->port.tty:x64 sw_stop=state->port.tty->flow.stopped:u8"
                  " hw_stop=state->uart_port->hw_stopped:u8"
                  " port_dev=state->uart_port->port_dev:x64"
                  " tx_en=state->uart_port->port_dev->tx_enabled"
                  " pm_status=state->uart_port->port_dev->dev.power.runtime_status:s32");
    // Exact compiled __uart_start+0x4c is the first instruction after
    // __pm_runtime_resume. x20=uport, x19=serial_port_device (dev at zero),
    // w0=signed return. An entry/return pair on the void __uart_start would
    // lose that result. All numeric reads below are witnessed ordinary RAM.
    // dev.power=272; status=236, error=244, disable_depth=224 bits0..2.
    // trace_kprobe.c prints symbol offsets with %u, so use canonical decimal
    // 76 (0x4c). Exact readback and selective cleanup must match that spelling.
    defs.push_back("p:" + g + "_gate_pm __uart_start+76 uport=%x20:x64"
                  " mapbase=+328(%x20):x64 port_dev=%x19:x64 pm_ret=%x0:s32"
                  " pm_status=+508(%x19):s32 pm_error=+516(%x19):s32"
                  " pm_disabled=+496(%x19):b3@0/32 phys_disabled=+496(+344(%x20)):b3@0/32"
                  " tx_en=+936(%x19):b1@0/32");
    return defs;
}

std::string summarize(const Result& r) {
    std::ostringstream out;
    out << "BTD terminal=" << r.reason << " owner=" << r.name
        << " admitted=" << r.admitted << " armed=" << r.armed << " reply=" << r.reply
        << " made=" << r.made << " off=" << r.off << " disabled=" << r.disabled
        << " exported=" << r.exported << " truncated=" << r.truncated
        << " statistics=" << r.statistics << " cleanup=" << r.cleanup
        << " registration_uncertain=" << r.registration_uncertain
        << " T=" << r.entered << " D=" << r.deadline << " C=" << r.capture_end
        << " on=" << r.enabled << " readback=" << r.readback << " mask=" << (r.armed ? (r.mode == Mode::DmaD84 ? kDmaD84ProbeMask : kProbeMask) : 0)
        << " nonce0=" << r.nonce[0] << " nonce1=" << r.nonce[1]
        << " stopped_ns=" << r.stopped << " status_saved=" << r.status_saved;
    if (r.mode == Mode::DmaD84)
        out << " mode=capture-dma-d84 schema=d84-vendor-v3 target_release=" << kDmaD84Release
            << " target_identity=uname+arm64+4k-not-image-sha uart_se_offset=544"
            << " trace_export_cap_bytes=" << trace_export_cap(r.mode)
            << " fifo_excluded_comm=xyauto-mcu-read fifo_filter_applied=" << r.fifo_filter_applied
            << " gate_mapbase=0x4a8c000 gate_filters_applied=" << r.gate_filters_applied;
    const auto& f = r.setup_failure;
    out << " setup_stage=" << (f.stage.empty() ? "none" : f.stage);
    if (!f.stage.empty()) {
        out << " setup_check=" << f.check << " setup_path_hex=" << hex(f.path)
            << " setup_op=" << (f.error.operation.empty() ? "none" : f.error.operation)
            << " setup_errno=" << f.error.number << " setup_detail_hex=" << hex(f.error.detail)
            << " setup_expected_hex=" << hex(f.expected)
            << " setup_read_ok=" << f.read_ok << " setup_read_truncated=" << f.read_truncated
            << " setup_observed_bytes=" << f.observed_bytes << " setup_observed_capped=" << f.observed_capped
            << " setup_observed_hex=" << (f.observed.empty() ? "-" : f.observed);
    }
    out
        << " kill_safe=0 physical_delivery=unproven"
        << " absence_requires=covered_interval+matched_uart+no_drops+no_kretprobe_misses\n";
    return out.str();
}

Result capture(unsigned seconds, const Endpoint& endpoint, TraceIo& io, const Clock& clock, Mode mode) {
    Result result;
    result.mode = mode;
    const uint64_t start = clock.now(), latest = start + kNoClient;
    if (seconds < 1 || seconds > 120 || (mode != Mode::Legacy && mode != Mode::DmaD84) ||
        !validate_socket(endpoint.listener, true, endpoint)) {
        result.reason = "invalid-owner-configuration"; return result;
    }
    Packet request;
    Fd client;
    for (unsigned attempts = 0; attempts < 32 && clock.now() < latest; ++attempts) {
        if (!ready(endpoint.listener, POLLIN, latest, clock)) break;
        Fd candidate(accept4(endpoint.listener, nullptr, nullptr, SOCK_NONBLOCK | SOCK_CLOEXEC));
        if (candidate.n < 0) continue;
        if (!validate_socket(candidate.n, false, endpoint) ||
            !ready(candidate.n, POLLIN, latest, clock) ||
            !receive_packet(candidate.n, request) || !lease(request, clock.now(), latest)) continue;
        client.n = candidate.n; candidate.n = -1; break;
    }
    if (client.n < 0) return result;
    result.admitted = true;
    const uint64_t end = request.entered + uint64_t(seconds) * kSecond;
    result.entered = request.entered; result.deadline = request.deadline; result.capture_end = end;
    result.nonce[0] = request.nonce[0]; result.nonce[1] = request.nonce[1];
    const uint64_t admission = std::min(end, request.deadline);
    auto time_ok = [&] { return clock.now() < admission && !gone(client.n); };
    std::vector<std::string> ledger;
    std::string base, stats;
    Fd timer(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC));
    const int timer_error = timer.n < 0 ? errno : 0;
    auto timely = [&](const char* stage, const std::string& path = {}, const Read* read = nullptr) {
        return time_ok() || setup_failed(result, stage, "deadline-or-peer", path,
                                        read ? read->error : IoError{}, read);
    };
    auto readable = [&](const char* stage, const std::string& path, const Read& read) {
        return (read.ok && !read.truncated) || setup_failed(result, stage,
                read.ok ? "truncated" : "read-failed", path, read.error, &read);
    };
    auto matched = [&](const char* stage, const std::string& path, const Read& read, const std::string& want) {
        return readable(stage, path, read) && (trim(read.text) == want ||
                setup_failed(result, stage, "readback-mismatch", path, {}, &read, want));
    };
    auto wrote = [&](const char* stage, const std::string& path, const std::string& value) {
        return io.write(path, value) || setup_failed(result, stage, "write-failed", path, io.error());
    };
    auto checked_read = [&](const char* stage, const std::string& path, const std::string& want) {
        if (!timely(stage)) return false;
        Read read = io.read(path, kStatusCap, admission);
        return timely(stage, path, &read) && matched(stage, path, read, want);
    };
    auto checked_write = [&](const char* stage, const std::string& path, const std::string& value) {
        return timely(stage) && wrote(stage, path, value) && timely(stage);
    };
    auto setup = [&]() -> bool {
        result.reason = "setup-failed";
        uint64_t nonce[2];
        if (!timely("begin")) return false;
        if (timer.n < 0) return setup_failed(result, "timer-create", "syscall", {}, {"timerfd_create", timer_error, {}});
        if (mode == Mode::DmaD84 && !io.dma_d84_target())
            return setup_failed(result, "target-d84", "unsupported-target", {}, io.error());
        IoError nonce_error;
        if (!random_nonce(nonce, &nonce_error)) return setup_failed(result, "nonce", "random-nonce-failed", {}, nonce_error);
        if (!io.open_and_lock()) return setup_failed(result, "open-lock", "io-failed", {}, io.error());
        if (!timely("open-lock")) return false;
        result.name = prefix(nonce);
        base = "instances/" + result.name + "/";
        Read all = io.read("kprobe_events", kTraceCap, admission);
        if (!timely("initial-events-read", "kprobe_events", &all) || !readable("initial-events-read", "kprobe_events", all)) return false;
        if (all.text.find(result.name) != std::string::npos)
            return setup_failed(result, "initial-events-read", "name-collision", "kprobe_events", {}, &all, result.name);
        if (!timely("instance-create")) return false;
        if (!io.create(result.name)) return setup_failed(result, "instance-create", "io-failed", result.name, io.error());
        result.made = true;
        // The fresh kernel instance starts ON. OFF is the FIRST operation on it,
        // even if creation itself crossed the admission deadline.
        if (!wrote("initial-off-write", base + "tracing_on", "0\n")) return false;
        Read off = io.read(base + "tracing_on", 32, admission);
        if (!timely("initial-off-read", base + "tracing_on", &off) || !matched("initial-off-read", base + "tracing_on", off, "0")) return false;
        // ACK d84cfa44, 4K ARM64: each subbuffer has 4080 payload bytes
        // after its 16-byte header. ceil(256*1024/4080)=65 pages; the kernel
        // reports (65*4080)>>10 = 258 KiB, not the requested 256 KiB.
        // Reject other geometry and noncanonical/unequal sizes, not >=256.
        if (!checked_read("subbuffer-read", base + "buffer_subbuf_size_kb", "4") ||
            !checked_write("buffer-write", base + "buffer_size_kb", "256\n") ||
            !checked_read("buffer-read", base + "buffer_size_kb", "258") ||
            !checked_read("tracer-read", base + "current_tracer", "nop") ||
            !checked_write("clock-write", base + "trace_clock", "mono\n") ||
            !checked_write("trace-clear", base + "trace", "\n")) return false;
        Read clocks = io.read(base + "trace_clock", 1024, admission);
        if (!timely("clock-read", base + "trace_clock", &clocks) || !readable("clock-read", base + "trace_clock", clocks)) return false;
        std::istringstream clock_names(clocks.text);
        std::string clock_name;
        unsigned selected = 0;
        while (clock_names >> clock_name) {
            if (clock_name.front() == '[') {
                if (clock_name != "[mono]") return setup_failed(result, "clock-read", "readback-mismatch",
                        base + "trace_clock", {}, &clocks, "one-[mono]");
                ++selected;
            }
        }
        if (selected != 1) return setup_failed(result, "clock-read", "selection-count",
                base + "trace_clock", {}, &clocks, "one-[mono]");
        Read total = io.read(base + "buffer_total_size_kb", 64, admission);
        if (!timely("buffer-total-read", base + "buffer_total_size_kb", &total) || !readable("buffer-total-read", base + "buffer_total_size_kb", total)) return false;
        auto cpus = io.cpus();
        const IoError cpu_error = io.error();
        if (!time_ok()) return setup_failed(result, "cpu-list", "deadline-or-peer", "per_cpu", cpu_error);
        if (!total.ok || total.truncated) return false;
        if (cpus.empty() || cpus.size() > 256)
            return setup_failed(result, "cpu-list", "cpu-count", "per_cpu", cpu_error, nullptr, "1..256");
        stats = "trace_clock=mono buffer_requested_per_cpu_kb=256 buffer_per_cpu_kb=258 buffer_subbuf_kb=4 cpu_count=" + std::to_string(cpus.size()) +
                " buffer_total_kb=" + trim(total.text) + "\n";
        for (const auto& def : mode == Mode::DmaD84 ? dma_d84_definitions(result.name) : definitions(result.name)) {
            if (!timely("registration-precheck")) return false;
            all = io.read("kprobe_events", kTraceCap, admission);
            // Recheck each name immediately before append; an existing event can
            // accept extra probes. Never put a failed append in the ledger.
            if (!readable("registration-precheck", "kprobe_events", all)) return false;
            if (all.text.find(event_name(def)) != std::string::npos)
                return setup_failed(result, "registration-precheck", "name-collision", "kprobe_events", {}, &all, event_name(def));
            if (!timely("registration-precheck")) return false;
            if (!wrote("registration-write", "kprobe_events", def + "\n")) {
                result.registration_uncertain = true; return false;
            }
            ledger.push_back(def); // SUCCESSFUL write, before readback or clock.
            if (!timely("registration-read")) return false;
            Read registered = io.read("kprobe_events", kTraceCap, admission);
            if (!exact_definition(registered, def)) return setup_failed(result, "registration-read",
                    "exact-definition", "kprobe_events", registered.error, &registered, def);
            if (!timely("registration-read")) return false;
        }
        for (const auto& def : ledger) {
            const auto event = base + "events/" + event_name(def);
            if (mode == Mode::DmaD84 && event_name(def) == "kprobes/" + result.name + "_fifo") {
                // D3-0143: unrelated MCU TIOCOUTQ polling used 363 KiB in
                // 23 s of the then-512-KiB export. Keep BT's zero/nonzero drain
                // predicates; exclude only HUD's named MCU-reader task.
                // Linux comm is 15 visible bytes (xyauto-mcu-reader truncates).
                // d84 trace_events.c/filter.c support generic comm predicates.
                // Instance-local filter only, checked before enable/ON.
                const std::string filter = "comm != \"xyauto-mcu-read\"";
                if (!checked_write("fifo-filter-write", event + "/filter", filter + "\n") ||
                    !checked_read("fifo-filter-read", event + "/filter", filter)) return false;
                result.fifo_filter_applied = true;
            }
            if (mode == Mode::DmaD84 &&
                (event_name(def) == "kprobes/" + result.name + "_gate_in" ||
                 event_name(def) == "kprobes/" + result.name + "_gate_pm")) {
                // Generic serial-core sites also serve the MCU. Filter only
                // these new events by measured SE3 identity, before ON.
                const std::string filter = "mapbase == 0x4a8c000";
                if (!checked_write("gate-filter-write", event + "/filter", filter + "\n") ||
                    !checked_read("gate-filter-read", event + "/filter", filter)) return false;
                ++result.gate_filters_applied;
            }
            const auto path = event + "/enable";
            if (!checked_write("event-enable-write", path, "1\n") || !checked_read("event-enable-read", path, "1")) return false;
        }
        itimerspec spec{};
        spec.it_value.tv_sec = end / kSecond; spec.it_value.tv_nsec = end % kSecond;
        if (!timely("timer-arm")) return false;
        if (timerfd_settime(timer.n, TFD_TIMER_ABSTIME, &spec, nullptr))
            return setup_failed(result, "timer-arm", "syscall", {}, {"timerfd_settime", errno, {}});
        if (!timely("timer-arm")) return false;
        result.reason = "enable-failed-or-late";
        if (!wrote("on-write", base + "tracing_on", "1\n")) return false;
        const uint64_t enabled = clock.now();
        result.enabled = enabled;
        if (enabled >= admission) return setup_failed(result, "on-write", "deadline");
        Read on = io.read(base + "tracing_on", 32, admission);
        const uint64_t readback = clock.now();
        result.readback = readback;
        if (!matched("on-read", base + "tracing_on", on, "1")) return false;
        if (readback >= admission || gone(client.n)) return setup_failed(result, "on-read", "deadline-or-peer");
        Packet reply = request;
        reply.kind = 2; reply.capture = end; reply.enabled = enabled;
        reply.readback = readback; reply.mask = mode == Mode::DmaD84 ? kDmaD84ProbeMask : kProbeMask;
        if (clock.now() >= admission) return setup_failed(result, "reply", "deadline");
        result.armed = true;
        result.reply = send_packet(client.n, reply); // Never wait for socket space.
        result.reason = "capture-complete";
        return true;
    };
    if (setup()) {
        // No client polling or CONTROL parser while ON. A duplicate/flood/lost
        // reply cannot change C or starve the one real autonomous timer.
        while (clock.now() < end) {
            if (!ready(timer.n, POLLIN, end, clock)) {
                if (clock.now() < end) result.reason = "timer-wait-failed";
                break;
            }
            uint64_t expirations;
            if (read(timer.n, &expirations, sizeof(expirations)) == sizeof(expirations)) break;
            if (errno != EINTR && errno != EAGAIN) { result.reason = "timer-read-failed"; break; }
        }
    }
    if (!result.made) return result;
    // A single terminal cleanup. OFF is independent of the admission lease and
    // happens before event disable, trace read/export, or any status publication.
    const uint64_t cleanup_end = clock.now() + kSecond;
    bool off_write = io.write(base + "tracing_on", "0\n");
    Read off = io.read(base + "tracing_on", 32, cleanup_end);
    result.off = off_write && off.ok && !off.truncated && trim(off.text) == "0";
    result.stopped = clock.now();
    result.disabled = true;
    for (const auto& def : ledger) {
        const std::string path = base + "events/" + event_name(def) + "/enable";
        if (clock.now() >= cleanup_end) { result.disabled = false; break; }
        bool wrote = io.write(path, "0\n");
        Read value = io.read(path, 32, cleanup_end);
        if (!wrote || !value.ok || value.truncated || trim(value.text) != "0") result.disabled = false;
    }
    if (result.off && result.disabled && clock.now() < cleanup_end) {
        Read trace = io.read(base + "trace", trace_export_cap(mode), cleanup_end);
        result.truncated = trace.truncated;
        if (trace.ok) result.exported = io.save(result.name + ".trace", trace.text, cleanup_end);
        bool stats_ok = true;
        for (const auto& cpu : io.cpus()) {
            if (clock.now() >= cleanup_end || stats.size() >= kStatusCap / 2) { stats_ok = false; break; }
            Read s = io.read(base + "per_cpu/" + cpu + "/stats", 1024, cleanup_end);
            if (!s.ok || s.truncated) stats_ok = false;
            stats += cpu + "\n" + s.text;
        }
        Read profile = io.read("kprobe_profile", kTraceCap, cleanup_end);
        if (!profile.ok || profile.truncated) stats_ok = false;
        std::istringstream lines(profile.text);
        std::string line;
        unsigned found = 0;
        while (std::getline(lines, line)) {
            if (line.find(result.name) != std::string::npos && stats.size() + line.size() < kStatusCap / 2) {
                stats += line + "\n"; ++found;
            }
        }
        result.statistics = stats_ok && found == ledger.size();
    }
    result.cleanup = result.off && result.disabled && !result.registration_uncertain;
    for (const auto& def : ledger) {
        if (clock.now() >= cleanup_end || !result.off || !result.disabled ||
            !exact_definition(io.read("kprobe_events", kTraceCap, cleanup_end), def) || clock.now() >= cleanup_end) {
            result.cleanup = false; continue;
        }
        if (!io.write("kprobe_events", "-:" + event_name(def) + "\n")) {
            result.cleanup = false; continue;
        }
        Read after = io.read("kprobe_events", kTraceCap, cleanup_end);
        if (!after.ok || after.truncated || after.text.find(event_name(def)) != std::string::npos)
            result.cleanup = false;
    }
    if (result.cleanup && clock.now() < cleanup_end) result.cleanup = io.remove(result.name);
    else result.cleanup = false;
    // Leave ambiguous or not-confirmed-OFF resources in place. No blind delete.
    std::string status = summarize(result) + stats;
    for (const auto& definition : ledger) status += "successful_definition " + definition + "\n";
    if (status.size() <= kStatusCap && clock.now() < cleanup_end)
        result.status_saved = io.save(result.name + ".status", status, cleanup_end);
    return result;
}

bool wait_for_owner(const Endpoint& endpoint, const Clock& clock, Packet* receipt) {
    Packet request;
    request.entered = clock.now();
    request.deadline = request.entered + kLease;
    if (!random_nonce(request.nonce) || endpoint.socket.size() >= sizeof(sockaddr_un::sun_path)) return false;
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    memcpy(addr.sun_path, endpoint.socket.c_str(), endpoint.socket.size() + 1);
    Fd fd(socket(AF_UNIX, SOCK_SEQPACKET | SOCK_NONBLOCK | SOCK_CLOEXEC, 0));
    if (fd.n < 0) return false;
    while (clock.now() < request.deadline) {
        if (connect(fd.n, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) == 0 || errno == EISCONN) break;
        if (errno != ENOENT && errno != ECONNREFUSED && errno != EAGAIN && errno != EINPROGRESS) return false;
        // Resource-free bounded wait for init/owner; never restart the lease.
        poll(nullptr, 0, 5);
    }
    if (clock.now() >= request.deadline || !validate_socket(fd.n, false, endpoint) ||
        !send_packet(fd.n, request) || !ready(fd.n, POLLIN, request.deadline, clock)) return false;
    Packet reply;
    if (!receive_packet(fd.n, reply) || clock.now() >= request.deadline || !header(reply, 2) ||
        reply.nonce[0] != request.nonce[0] || reply.nonce[1] != request.nonce[1] ||
        reply.entered != request.entered || reply.deadline != request.deadline ||
        (reply.mask != kProbeMask && reply.mask != kDmaD84ProbeMask) || reply.enabled < reply.entered || reply.readback < reply.enabled ||
        reply.readback >= reply.deadline || reply.readback > clock.now() || reply.capture <= reply.readback ||
        reply.capture < reply.entered + kSecond || reply.capture > reply.entered + 120 * kSecond)
        return false;
    if (receipt) *receipt = reply;
    return true;
}
} // namespace bttrace

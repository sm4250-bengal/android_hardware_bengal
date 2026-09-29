// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace bttrace {
constexpr uint64_t kMs = 1000000, kSecond = 1000000000;
constexpr uint64_t kLease = 2000 * kMs, kNoClient = 2500 * kMs;
constexpr uint64_t kProbeMask = 0x7f;
constexpr uint64_t kDmaD84ProbeMask = 0x3fff;
enum class Mode { Legacy, DmaD84 };
constexpr const char* kDmaD84Release = "6.18.32-4k-gd84cfa4479d9";
constexpr size_t kTraceCap = 256 * 1024, kStatusCap = 32 * 1024;
constexpr size_t kDmaD84TraceCap = 1024 * 1024;
constexpr size_t trace_export_cap(Mode mode) {
    return mode == Mode::DmaD84 ? kDmaD84TraceCap : kTraceCap;
}
constexpr const char* kSocket = "/dev/socket/bengal_bt_trace";
constexpr const char* kPeer = "u:r:vendor_bengal_bttrace:s0";

// Local native-endian, fixed-size protocol. No descriptors or resources travel.
struct Packet {
    uint32_t magic = 0x42544431, version = 1, kind = 1, reserved = 0;
    uint64_t nonce[2] = {}, entered = 0, deadline = 0, capture = 0;
    uint64_t enabled = 0, readback = 0, mask = 0;
};
static_assert(sizeof(Packet) == 80);

struct Clock {
    virtual ~Clock() = default;
    virtual uint64_t now() const;
};
// Diagnostic only. number is a captured syscall errno, or zero when no syscall
// failed (policy refusal, short I/O, readback mismatch, deadline/peer loss).
struct IoError {
    std::string operation;
    int number = 0;
    std::string detail;
};
struct Read {
    bool ok = false, truncated = false;
    std::string text;
    IoError error{};
};
// Internal adapter only: neither the production CLI nor its environment selects
// a fake root, definition, delay, clock, UID, or security context.
struct TraceIo {
    virtual ~TraceIo() = default;
    // Only called for explicit DMA mode, before opening/writing tracefs.
    // This admission check is not an Image SHA identity proof.
    virtual bool dma_d84_target() { return false; }
    // Last bool/CPU operation's diagnostic; read carries its own snapshot.
    virtual IoError error() const { return {}; }
    virtual bool open_and_lock() = 0;
    virtual bool create(const std::string& name) = 0;
    virtual bool remove(const std::string& name) = 0;
    virtual Read read(const std::string& path, size_t cap, uint64_t deadline) = 0;
    virtual bool write(const std::string& path, const std::string& value) = 0;
    virtual std::vector<std::string> cpus() = 0;
    virtual bool save(const std::string& name, const std::string& value,
                      uint64_t deadline) = 0;
};
struct Endpoint {
    int listener = -1; // init-owned listening socket, never bound by owner
    std::string socket = kSocket, peer = kPeer;
    uint32_t uid = 0;
};
struct SetupFailure {
    std::string stage, check, path, expected, observed;
    size_t observed_bytes = 0;
    bool read_ok = false, read_truncated = false, observed_capped = false;
    IoError error;
};
struct Result {
    Mode mode = Mode::Legacy;
    bool admitted = false, armed = false, reply = false, made = false;
    bool off = false, disabled = false, exported = false, cleanup = false;
    bool truncated = false, statistics = false, status_saved = false;
    bool registration_uncertain = false;
    bool fifo_filter_applied = false;
    unsigned gate_filters_applied = 0;
    uint64_t entered = 0, deadline = 0, capture_end = 0, enabled = 0, readback = 0, stopped = 0;
    uint64_t nonce[2] = {};
    std::string reason = "not-admitted", name;
    SetupFailure setup_failure;
};

std::vector<std::string> definitions(const std::string& name);
std::vector<std::string> dma_d84_definitions(const std::string& name);
bool dma_d84_target_matches(const std::string& release, const std::string& machine, long page_size);
std::string summarize(const Result& result);
bool random_nonce(uint64_t (&nonce)[2], IoError* error = nullptr);
bool receive_packet(int fd, Packet& packet); // closes/rejects all received rights
bool validate_socket(int fd, bool listener, const Endpoint& endpoint);
Result capture(unsigned seconds, const Endpoint&, TraceIo&, const Clock&, Mode = Mode::Legacy);
bool wait_for_owner(const Endpoint&, const Clock&, Packet* receipt = nullptr);
int production_main(int argc, char** argv);
} // namespace bttrace

// SPDX-License-Identifier: Apache-2.0
#pragma once
#include <array>
#include <chrono>
#include <condition_variable>
#include <cstdint>
#include <map>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

namespace bengal::power {
struct Vote { int original = 0; int target = 0; };
using Votes = std::array<Vote, 2>;
enum class LoadResult { Absent, Valid, RetryableIo, Rejected };
struct Diagnostic {
    const char* stage;
    int policy;
    const char* node;
    int requested;
    int observed;
    int error;
    int64_t elapsedMs;
    const char* result;
};
class Io {
  public:
    virtual ~Io() = default;
    virtual bool read(int policy, const std::string& node, int* value) = 0;
    virtual bool frequencies(int policy, std::vector<int>* values) = 0;
    // Success means the kernel accepted the request, NOT effective convergence.
    virtual bool write(int policy, int value) = 0;
    // Malformed/unsafe journals and lock contention are terminal, not retryable.
    virtual LoadResult load(Votes* votes) = 0;
    virtual bool save(const Votes& votes) = 0;
    virtual bool clear() = 0;
    virtual void diagnostic(const Diagnostic&) {}
};
class SysfsIo final : public Io {
  public:
    explicit SysfsIo(std::string cpuRoot = "/sys/devices/system/cpu/cpufreq",
                     std::string stateRoot = "/dev/bengal-power")
        : cpuRoot_(std::move(cpuRoot)), stateRoot_(std::move(stateRoot)) {}
    ~SysfsIo() override;
    bool read(int, const std::string&, int*) override;
    bool frequencies(int, std::vector<int>*) override;
    bool write(int, int) override;
    LoadResult load(Votes*) override;
    bool save(const Votes&) override;
    bool clear() override;
    void diagnostic(const Diagnostic&) override;
  private:
    int lock_ = -1;
    std::string cpuRoot_, stateRoot_;
    // Keys come only from fixed internal stage/policy/result values, never caller input.
    std::map<std::string, std::chrono::steady_clock::time_point> diagnostics_;
    std::string path(int policy, const std::string& node) const;
};
bool recover(Io& io);
class Boost {
  public:
    explicit Boost(Io& io);
    ~Boost();
    bool supported();
    bool interaction(int durationMs);
  private:
    using Clock = std::chrono::steady_clock;
    void run();
    void startup();
    void initialize();
    bool restore();
    Io& io_;
    std::mutex mutex_;
    std::condition_variable cv_;
    Votes votes_{};
    bool supported_ = false;
    bool active_ = false;
    bool recoveryPending_ = false;
    bool stop_ = false;
    Clock::time_point deadline_{};
    std::thread worker_;
};
}  // namespace bengal::power

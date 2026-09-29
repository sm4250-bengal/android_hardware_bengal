// SPDX-License-Identifier: Apache-2.0
#include "Boost.h"
#include <atomic>
#include <cassert>
#include <cerrno>
#include <iostream>
#include <mutex>
#include <thread>

using namespace std::chrono_literals;
using Clock = std::chrono::steady_clock;
using bengal::power::Boost;
using bengal::power::LoadResult;
using bengal::power::Votes;

// Models ACK's accepted QoS request separately from deferred effective policy->min.
// A later original store supersedes a still-pending target; reads run due work.
struct Async final : bengal::power::Io {
    std::mutex mutex;
    int effective[2]{300000, 300000}, requested[2]{300000, 300000};
    Clock::time_point due[2]{};
    std::atomic<int> targetDelay[2]{{20}, {20}}, restoreDelay[2]{{20}, {20}};
    std::atomic<int> originals[2]{{0}, {0}}, targets[2]{{0}, {0}};
    std::atomic<int> maximum[2]{{1804800}, {2016000}};
    std::atomic<bool> journal{false}, neverTarget{false}, neverRestore{false};
    std::atomic<bool> failSecondTarget{false};
    std::atomic<bool> failOriginal{false}, failReadsAfterTarget{false}, reduceCapAfterTarget{false};
    std::atomic<int64_t> firstOriginalUs{0};
    std::vector<bengal::power::Diagnostic> diagnostics;
    Votes saved{};
    void publishLocked(int i) {
        if (Clock::now() >= due[i]) effective[i] = requested[i];
    }
    int floor(int i) {
        std::lock_guard lock(mutex); publishLocked(i); return effective[i];
    }
    int request(int i) { std::lock_guard lock(mutex); return requested[i]; }
    void foreign(int i, int value) {
        std::lock_guard lock(mutex); effective[i] = requested[i] = value;
        due[i] = Clock::time_point{};
    }
    bool read(int p, const std::string& node, int* out) override {
        if (failReadsAfterTarget && targets[0] > 0) { errno = EIO; return false; }
        if (node == "scaling_max_freq" && reduceCapAfterTarget && targets[0] > 0) {
            *out = 614400; return true;
        }
        *out = node == "scaling_max_freq" ? maximum[p / 4].load() : floor(p / 4);
        return true;
    }
    bool frequencies(int p, std::vector<int>* out) override {
        *out = p == 0 ? std::vector<int>{300000, 614400, 864000, 1804800}
                     : std::vector<int>{300000, 652800, 902400, 2016000};
        return true;
    }
    bool write(int p, int value) override {
        assert(journal);
        int i = p / 4;
        if (value == 300000 && failOriginal) { errno = EACCES; return false; }
        if (i == 1 && value > 300000 && failSecondTarget) { errno = EIO; return false; }
        std::lock_guard lock(mutex);
        publishLocked(i);
        requested[i] = value;
        if (value == 300000) {
            ++originals[i];
            if (!firstOriginalUs) firstOriginalUs = std::chrono::duration_cast<
                    std::chrono::microseconds>(Clock::now().time_since_epoch()).count();
        } else ++targets[i];
        const bool held = value == 300000 ? neverRestore.load() : neverTarget.load();
        due[i] = held ? Clock::time_point::max() : Clock::now() +
                std::chrono::milliseconds(value == 300000 ? restoreDelay[i].load() : targetDelay[i].load());
        return true;
    }
    LoadResult load(Votes* out) override {
        std::lock_guard lock(mutex); *out = saved;
        return journal ? LoadResult::Valid : LoadResult::Absent;
    }
    bool save(const Votes& votes) override {
        std::lock_guard lock(mutex); saved = votes; journal = true; return true;
    }
    bool clear() override { journal = false; return true; }
    void diagnostic(const bengal::power::Diagnostic& d) override {
        std::lock_guard lock(mutex); diagnostics.push_back(d);
    }
    bool told(const std::string& stage, const std::string& result, int error = 0) {
        std::lock_guard lock(mutex);
        for (const auto& d : diagnostics)
            if (d.stage == stage && d.result == result && d.error == error) return true;
        return false;
    }
};

template<class Predicate> void eventually(Predicate predicate) {
    for (int i = 0; i < 150; ++i) {
        if (predicate()) return;
        std::this_thread::sleep_for(10ms);
    }
    assert(predicate());
}
int main() {
    {
        Async io;
        Boost boost(io);
        assert(boost.supported());
        const auto begin = Clock::now();
        const bool admitted = boost.interaction(250);
        std::this_thread::sleep_for(50ms);
        std::cout << "delayed first store: admitted=" << admitted << " supported=" << boost.supported()
                  << " floor0=" << io.floor(0) << " floor4=" << io.floor(1)
                  << " journal=" << io.journal << std::endl;
        assert(admitted && boost.supported() && io.journal);
        assert(io.floor(0) == 864000 && io.floor(1) == 902400);
        eventually([&] { return !io.journal; });
        const auto restoreUs = io.firstOriginalUs - std::chrono::duration_cast<
                std::chrono::microseconds>(begin.time_since_epoch()).count();
        assert(restoreUs >= 240000 && restoreUs < 300000);
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
        assert(io.told("target_converge", "observed") && io.told("restore_converge", "observed"));
        std::cout << "PASS delayed target/restore, admission-based expiry; restore_us=" << restoreUs << '\n';
    }
    {
        Async io; io.neverTarget = true;
        Boost boost(io);
        assert(!boost.interaction(250));
        assert(!boost.supported() && !io.journal);
        assert(io.originals[0] == 1 && io.originals[1] == 1);
        assert(io.request(0) == 300000 && io.request(1) == 300000);
        assert(io.told("target_converge", "timeout"));
        io.neverTarget = false;
        std::this_thread::sleep_for(120ms);
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
    }
    std::cout << "PASS never-converging target explicitly superseded despite original readback\n";
    {
        Async io; io.targetDelay[0] = 150; io.targetDelay[1] = 150; io.failOriginal = true;
        Boost boost(io);
        assert(!boost.interaction(250));
        assert(!boost.supported() && io.journal && io.originals[0] == 0);
        std::this_thread::sleep_for(70ms);
        assert(io.floor(0) == 864000 && io.floor(1) == 902400 && io.journal);
        assert(io.told("restore_store", "write_failed", EACCES));
        io.failOriginal = false;
        eventually([&] { return !io.journal; });
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
    }
    std::cout << "PASS failed compensation retains journal across later queued target publication\n";
    {
        Async io; io.failReadsAfterTarget = true;
        Boost boost(io);
        assert(!boost.interaction(250));
        assert(!boost.supported() && io.journal);
        assert(io.told("target_converge", "read_failed", EIO));
        io.failReadsAfterTarget = false;
        eventually([&] { return !io.journal; });
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
    }
    std::cout << "PASS post-submit read failure retains journal until observed compensation\n";
    {
        Async io; io.reduceCapAfterTarget = true;
        Boost boost(io);
        assert(!boost.interaction(250));
        assert(!io.journal && !boost.supported());
        assert(io.told("target_converge", "cap_reduced"));
        assert(io.request(0) == 300000 && io.request(1) == 300000);
    }
    std::cout << "PASS cap reduced during convergence cancels both submitted targets\n";
    {
        Async io; io.failSecondTarget = true;
        Boost boost(io);
        assert(!boost.interaction(250));
        assert(!boost.supported() && !io.journal);
        assert(io.targets[0] == 1 && io.targets[1] == 0);
        assert(io.originals[0] == 1 && io.originals[1] == 1);
        assert(io.request(0) == 300000 && io.request(1) == 300000);
        assert(io.told("target_store", "write_failed", EIO));
        std::this_thread::sleep_for(50ms);
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
    }
    std::cout << "PASS partial second store failure cancels queued first; errno diagnostic\n";
    {
        Async io; io.targetDelay[0] = 5; io.targetDelay[1] = 5; io.neverRestore = true;
        Boost boost(io);
        assert(boost.interaction(40));
        std::this_thread::sleep_for(180ms);
        assert(io.journal && !boost.supported());
        assert(io.floor(0) == 864000 && io.floor(1) == 902400);
        for (int i = 0; i < 20; ++i) assert(!boost.interaction(100) && !boost.interaction(-1));
        assert(io.originals[0] == 1 && io.originals[1] == 1); // no accelerated retries
        assert(io.told("restore_converge", "timeout") && io.told("journal", "retained"));
        io.neverRestore = false;
        eventually([&] { return !io.journal; });
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
        assert(!boost.supported()); // no automatic new hint admission after a failed active vote
    }
    std::cout << "PASS unresolved restore retains journal, blocks hints, bounded autonomous retry\n";
    {
        Async io;
        io.save({{{300000, 864000}, {300000, 902400}}});
        io.write(0, 864000); io.write(4, 902400);
        Boost boost(io);
        assert(boost.supported() && !io.journal);
        assert(io.originals[0] == 1 && io.originals[1] == 1);
        std::this_thread::sleep_for(60ms);
        assert(io.floor(0) == 300000 && io.floor(1) == 300000);
    }
    std::cout << "PASS startup journal compensates queued but not-yet-visible targets\n";
    {
        Async io; io.targetDelay[0] = 5; io.targetDelay[1] = 5;
        Boost boost(io); assert(boost.interaction(40));
        io.foreign(0, 614400);
        std::this_thread::sleep_for(180ms);
        assert(io.journal && !boost.supported() && io.originals[0] == 0);
        assert(io.floor(0) == 614400 && io.floor(1) == 300000);
        assert(io.told("restore_check", "foreign") && io.told("journal", "retained"));
        io.foreign(0, 300000); // explicit external reconciliation, not HAL overwrite
        eventually([&] { return !io.journal; });
        assert(!boost.supported());
    }
    std::cout << "PASS foreign effective floor preserves both value and recovery journal\n";
    {
        Async io; io.targetDelay[0] = 70; io.targetDelay[1] = 130;
        Boost boost(io);
        const auto begin = Clock::now();
        assert(!boost.interaction(250));
        assert(Clock::now() - begin < 180ms);
        assert(!io.journal && !boost.supported());
        assert(io.request(0) == 300000 && io.request(1) == 300000);
    }
    std::cout << "PASS two policies share100ms convergence budget, not sequential100ms each\n";
    {
        Async io; io.targetDelay[0] = 50; io.targetDelay[1] = 50;
        Boost boost(io);
        const auto begin = Clock::now();
        assert(!boost.interaction(25));
        assert(Clock::now() - begin < 80ms);
        assert(!io.journal && io.request(0) == 300000 && io.request(1) == 300000);
    }
    std::cout << "PASS short hint expiry bounds convergence and cancels pending targets\n";
}

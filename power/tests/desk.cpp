// SPDX-License-Identifier: Apache-2.0
#include "Boost.h"
#include "ThermalSensors.h"
#include <atomic>
#include <cassert>
#include <cerrno>
#include <cstdio>
#include <cmath>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <thread>
#include <csignal>
#include <sys/wait.h>
#include <unistd.h>

using namespace std::chrono_literals;
using bengal::power::Boost;
using bengal::power::Votes;
using bengal::power::LoadResult;
struct Fake final : bengal::power::Io {
    std::atomic<int> floor[2] = {300000, 300000};
    int maximum[2] = {1804800, 2016000};
    Votes saved{};
    std::atomic<bool> journal{false};
    std::atomic<bool> failRead{false}, failWrite{false}, failRestore{false}, failSave{false};
    std::atomic<bool> failLoad{false}, failClear{false};
    std::atomic<int> loads{0};
    bool malformed = false;
    int failPolicy = -1;
    bool read(int p, const std::string& node, int* out) override {
        if (failRead) return false;
        *out = node == "scaling_max_freq" ? maximum[p / 4] : floor[p / 4].load(); return true;
    }
    bool frequencies(int p, std::vector<int>* out) override {
        *out = p == 0 ? std::vector<int>{300000, 614400, 864000, 1017600, 1804800}
                      : std::vector<int>{300000, 652800, 902400, 1056000, 2016000};
        return true;
    }
    bool write(int p, int value) override {
        if (failWrite || (p == failPolicy && value > 300000) ||
            (failRestore && value == 300000)) return false;
        assert(journal); // Durable process-lifetime undo record must precede ANY write.
        floor[p / 4] = value; return true;
    }
    LoadResult load(Votes* out) override {
        ++loads;
        if (failLoad) return LoadResult::RetryableIo;
        *out = saved;
        return malformed ? LoadResult::Rejected : (journal ? LoadResult::Valid : LoadResult::Absent);
    }
    bool save(const Votes& value) override {
        if (failSave) return false;
        saved = value; journal = true; return true;
    }
    bool clear() override { if (failClear) return false; journal = false; return true; }
};
template<class Predicate> void eventually(Predicate predicate) {
    for (int i = 0; i < 150; ++i) {
        if (predicate()) return;
        std::this_thread::sleep_for(10ms);
    }
    assert(predicate());
}
void put(const std::filesystem::path& path, const std::string& value) {
    std::filesystem::create_directories(path.parent_path());
    std::ofstream file(path); file << value; assert(file.good());
}
int main(int argc, char** argv) {
    assert(argc == 2);
    {
        Fake io; io.saved = {{{300000, 864000}, {300000, 902400}}};
        io.journal = true; io.floor[0] = 864000; io.floor[1] = 902400;
        io.failRead = true;
        Boost boost(io);
        assert(!boost.supported() && io.journal);
        io.failRead = false;
        eventually([&] { return boost.supported() && !io.journal; });
        assert(io.floor[0] == 300000 && io.floor[1] == 300000);
    }
    std::cout << "PASS startup recovery retries transient read failure without daemon restart\n";
    for (int fault = 0; fault < 3; ++fault) {
        Fake io; io.saved = {{{300000, 864000}, {300000, 902400}}};
        io.journal = true; io.floor[0] = 864000; io.floor[1] = 652800; // foreign second floor
        auto& failing = fault == 0 ? io.failLoad : (fault == 1 ? io.failRestore : io.failClear);
        failing = true;
        Boost boost(io);
        assert(!boost.supported() && io.journal);
        assert(!boost.interaction(100) && !boost.interaction(-1));
        std::this_thread::sleep_for(250ms);
        assert(io.journal && !boost.supported());
        assert(io.loads >= 2 && io.loads <= 5); // retry, not a busy loop
        failing = false;
        std::this_thread::sleep_for(150ms);
        assert(!boost.supported() && io.journal && io.floor[1] == 652800);
        io.floor[1] = 300000; // explicit external ownership reconciliation
        eventually([&] { return boost.supported() && !io.journal; });
        assert(io.floor[0] == 300000 && io.floor[1] == 300000);
        assert(boost.interaction(30));
        eventually([&] { return !io.journal; });
        assert(io.floor[0] == 300000 && io.floor[1] == 300000);
    }
    std::cout << "PASS startup retries; foreign floor AND journal retained until external reconciliation\n";
    {
        Fake io; io.malformed = true; io.journal = true;
        io.saved = {{{300000, 864000}, {300000, 902400}}};
        io.floor[0] = 864000; io.floor[1] = 902400;
        Boost boost(io);
        std::this_thread::sleep_for(250ms);
        assert(!boost.supported() && io.loads == 1 && io.journal);
        assert(io.floor[0] == 864000 && io.floor[1] == 902400);
    }
    std::cout << "PASS rejected journal terminal, preserved, no retry or process exit\n";
    {
        Fake io; Boost boost(io);
        assert(boost.supported()); assert(boost.interaction(0));
        assert(io.floor[0] == 864000 && io.floor[1] == 902400);
        eventually([&] { return io.floor[0] == 300000 && !io.journal; });
        assert(io.floor[1] == 300000);
    }
    std::cout << "PASS default250ms autonomous expiry and exact actual OPPs\n";
    {
        Fake io; Boost boost(io);
        assert(boost.interaction(80)); std::this_thread::sleep_for(40ms);
        assert(boost.interaction(200)); std::this_thread::sleep_for(100ms);
        assert(io.floor[0] == 864000);
        eventually([&] { return !io.journal; });
        assert(boost.interaction(100000));
        eventually([&] { return !io.journal; });
        assert(boost.interaction(900)); assert(boost.interaction(-1));
        assert(io.floor[0] == 300000 && !io.journal);
        assert(boost.interaction(40)); std::this_thread::sleep_for(100ms);
        assert(!io.journal); // a cancelled long hint must not extend the next short hint
    }
    std::cout << "PASS extension clamp1000ms and explicit cancellation\n";
    {
        Fake io;
        { Boost boost(io); assert(boost.interaction(1000)); }
        assert(io.floor[0] == 300000 && !io.journal);
    }
    std::cout << "PASS destructor restores active request\n";
    {
        Fake io; Boost boost(io); io.failSave = true;
        assert(!boost.interaction(100)); assert(!boost.supported());
        assert(io.floor[0] == 300000 && !io.journal);
    }
    {
        Fake io; Boost boost(io); io.failWrite = true;
        assert(!boost.interaction(100)); assert(!boost.supported());
        assert(io.floor[0] == 300000);
    }
    {
        Fake io; Boost boost(io); io.failRead = true;
        assert(!boost.interaction(100)); assert(!boost.supported());
        assert(io.floor[0] == 300000 && !io.journal);
    }
    std::cout << "PASS journal/read/write failure do not advertise success\n";
    {
        Fake io; Boost boost(io); io.failPolicy = 4;
        assert(!boost.interaction(100)); assert(!boost.supported());
        assert(io.floor[0] == 300000 && io.floor[1] == 300000 && !io.journal);
    }
    std::cout << "PASS partial second-policy failure rolls first policy back\n";
    {
        Fake io; Boost boost(io); assert(boost.interaction(50));
        io.failRestore = true; std::this_thread::sleep_for(100ms);
        assert(io.journal && !boost.supported());
        io.failRestore = false; eventually([&] { return !io.journal; });
        assert(io.floor[0] == 300000);
    }
    std::cout << "PASS failed restore retains journal and retries\n";
    {
        Fake io; Boost boost(io); assert(boost.interaction(100));
        io.floor[0] = 614400;
        assert(!boost.interaction(100)); assert(!boost.supported());
        assert(io.floor[0] == 614400 && io.floor[1] == 300000 && io.journal);
        assert(!boost.interaction(-1));
        io.floor[0] = 300000;
        eventually([&] { return !io.journal; });
    }
    {
        Fake io; io.saved = {{{300000, 864000}, {300000, 902400}}};
        io.journal = true; io.floor[0] = 864000; io.floor[1] = 652800;
        assert(!bengal::power::recover(io));
        assert(io.floor[0] == 300000 && io.floor[1] == 652800 && io.journal);
        io.floor[1] = 300000;
        assert(bengal::power::recover(io) && !io.journal);
    }
    {
        Fake io; io.malformed = true; Boost boost(io);
        assert(!boost.supported() && !boost.interaction(100));
    }
    std::cout << "PASS foreign floor preserved, recovery selective, malformed journal refused\n";
    {
        Fake io; Boost boost(io); io.maximum[0] = 614400;
        assert(!boost.interaction(100)); assert(io.floor[0] == 300000);
    }
    std::cout << "PASS reduced thermal cap refuses stale higher floor\n";
    {
        const auto cpuRoot = std::filesystem::path(argv[1]) / "cpufreq";
        const auto state = std::filesystem::path(argv[1]) / "state";
        std::filesystem::create_directories(state);
        for (int policy : {0, 4}) {
            const auto p = cpuRoot / ("policy" + std::to_string(policy));
            put(p / "scaling_min_freq", "300000");
            put(p / "scaling_max_freq", policy == 0 ? "1804800" : "2016000");
            put(p / "scaling_available_frequencies", policy == 0 ?
                "300000 614400 864000 1017600 1804800\n" : "300000 652800 902400 1056000 2016000\n");
        }
        {
            bengal::power::SysfsIo io(cpuRoot.string(), state.string()); Boost boost(io);
            assert(boost.supported() && boost.interaction(40));
            bengal::power::SysfsIo rival(cpuRoot.string(), state.string());
            Votes votes; assert(rival.load(&votes) == LoadResult::Rejected);
            eventually([&] { return !std::filesystem::exists(state / "votes"); });
            int floor; assert(io.read(0, "scaling_min_freq", &floor) && floor == 300000);
        }
        {
            put(state / "votes", "malformed\n");
            bengal::power::SysfsIo io(cpuRoot.string(), state.string()); Boost boost(io);
            assert(!boost.supported());
            assert(std::filesystem::exists(state / "votes"));
        }
        std::cout << "PASS native file backend expiry, lock exclusion and malformed journal preservation\n";
        {
            const auto unavailableState = std::filesystem::path(argv[1]) / "initially-absent-state";
            bengal::power::SysfsIo io(cpuRoot.string(), unavailableState.string());
            Votes votes;
            assert(io.load(&votes) == LoadResult::RetryableIo);
            std::filesystem::create_directories(unavailableState);
            assert(io.load(&votes) == LoadResult::Absent);
        }
        {
            bengal::power::SysfsIo io(cpuRoot.string(), state.string());
            int value;
            errno = EACCES;
            put(cpuRoot / "policy0/scaling_min_freq", "broken\n");
            assert(!io.read(0, "scaling_min_freq", &value) && errno == EBADMSG);
            put(cpuRoot / "policy0/scaling_min_freq", "300000");
            assert(!io.write(8, 300000) && errno == ENOENT);
            // Capture only this host stderr seam; Android uses libc syslog/logcat.
            const auto capture = std::filesystem::path(argv[1]) / "diagnostic-rate.txt";
            FILE* file = std::fopen(capture.c_str(), "w+"); assert(file);
            const int saved = dup(STDERR_FILENO); assert(saved >= 0);
            assert(dup2(fileno(file), STDERR_FILENO) >= 0);
            errno = EBUSY;
            for (int i = 0; i < 20; ++i)
                io.diagnostic({"rate_test", 0, "scaling_min_freq", 300000, 864000, EIO, 100, "timeout"});
            assert(errno == EBUSY);
            io.diagnostic({"rate_test", 0, "scaling_min_freq", 300000, 300000, 0, 101, "observed"});
            std::fflush(stderr);
            assert(dup2(saved, STDERR_FILENO) >= 0); close(saved); std::fclose(file);
            std::ifstream input(capture); std::string line; int lines = 0;
            while (std::getline(input, line)) {
                assert(line.find("BENGAL_POWER stage=rate_test") != std::string::npos);
                ++lines;
            }
            assert(lines == 2); // repeated same result suppressed; transition retained
        }
        std::cout << "PASS native parse/store errno and bounded diagnostic repetition\n";
        {
            put(state / "votes", "BENGAL_POWER_V1\n300000 864000\n300000 902400\n");
            // Simulate a temporarily unavailable sysfs attribute at startup.
            assert(std::filesystem::remove(cpuRoot / "policy0/scaling_min_freq"));
            bengal::power::SysfsIo io(cpuRoot.string(), state.string()); Boost boost(io);
            assert(!boost.supported() && std::filesystem::exists(state / "votes"));
            put(cpuRoot / "policy0/scaling_min_freq", "864000");
            eventually([&] { return boost.supported(); });
            int floor;
            assert(io.read(0, "scaling_min_freq", &floor) && floor == 300000);
            assert(!std::filesystem::exists(state / "votes"));
        }
        std::cout << "PASS native retryable load classification and live startup sysfs recovery\n";
        const auto crashState = std::filesystem::path(argv[1]) / "crash-state";
        std::filesystem::create_directories(crashState);
        const pid_t child = fork(); assert(child >= 0);
        if (child == 0) {
            bengal::power::SysfsIo io(cpuRoot.string(), crashState.string()); Boost boost(io);
            if (!boost.interaction(1000)) _exit(2);
            raise(SIGKILL); _exit(3);
        }
        int status; assert(waitpid(child, &status, 0) == child);
        assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
        bengal::power::SysfsIo recovery(cpuRoot.string(), crashState.string());
        int floor;
        assert(recovery.read(0, "scaling_min_freq", &floor) && floor == 864000);
        assert(std::filesystem::exists(crashState / "votes"));
        assert(bengal::power::recover(recovery));
        assert(recovery.read(0, "scaling_min_freq", &floor) && floor == 300000);
        std::cout << "PASS actual SIGKILL leaves floor; fresh native owner recovers journal (not kill-safe)\n";
    }
    using namespace bengal::thermal;
    const auto root = std::filesystem::path(argv[1]) / "thermal";
    const auto cpu = root / "thermal_zone7";
    put(cpu / "type", "cpu4-thermal\n"); put(cpu / "temp", "46000\n");
    put(cpu / "trip_point_0_type", "passive\n"); put(cpu / "trip_point_0_temp", "90000\n");
    put(cpu / "trip_point_1_type", "critical\n"); put(cpu / "trip_point_1_temp", "110000\n");
    put(root / "thermal_zone0/type", "pm6125-thermal\n");
    put(root / "thermal_zone0/temp", "37000\n");
    Sensors sensors(root.string()); assert(sensors.initialized()); assert(sensors.inventory().size() == 2);
    const auto& sensor = sensors.inventory()[1];
    assert(sensor.type == Type::Cpu && sensors.inventory()[0].type == Type::Unknown);
    assert(sensors.read(sensor).temperature == 46.0f && sensors.read(sensor).severity == 0);
    put(cpu / "temp", "95000\n"); assert(sensors.read(sensor).severity == 1);
    put(cpu / "temp", "110000\n"); assert(sensors.read(sensor).severity == 6);
    assert(Sensors::severity("hot") == 3 && Sensors::severity("active") == 0);
    put(cpu / "temp", "-2000\n"); assert(sensors.read(sensor).temperature == -2.0f);
    for (const char* invalid : {"oops", "42000garbage", "999999999999999999", "-999999", ""}) {
        put(cpu / "temp", invalid); assert(std::isnan(sensors.read(sensor).temperature));
        assert(sensors.inventory().size() == 2);
    }
    put(cpu / "type", "battery\n"); assert(std::isnan(sensors.read(sensor).temperature));
    assert(Sensors::classify("battery") == Type::Unknown);
    assert(Sensors::classify("display-thermal") == Type::Soc);
    assert(Sensors::classify("gpu-thermal") == Type::Gpu);
    assert(!Sensors((root / "missing").string()).initialized());
    std::cout << "PASS thermal actual names, stable inventory, units, trips, malformed/missing/renamed NaN\n";
}

// SPDX-License-Identifier: Apache-2.0
#include "Boost.h"
#include <algorithm>
#include <cerrno>

namespace bengal::power {
namespace {
using Clock = std::chrono::steady_clock;
using namespace std::chrono_literals;
constexpr int policies[] = {0, 4};
constexpr auto settleBudget = 100ms;
constexpr auto pollPeriod = 2ms;

void tell(Io& io, const char* stage, int policy, const char* node, int requested,
          int observed, int error, Clock::time_point start, const char* result) {
    io.diagnostic({stage, policy, node, requested, observed, error,
                   std::chrono::duration_cast<std::chrono::milliseconds>(Clock::now() - start).count(),
                   result});
}

// A single deadline covers BOTH policies. Poll effective state, never reissue a target.
bool converge(Io& io, const Votes& votes, const std::array<bool, 2>& pending,
              bool restoring, Clock::time_point start, Clock::time_point limit) {
    const char* stage = restoring ? "restore_converge" : "target_converge";
    std::array<int, 2> observed{-1, -1};
    for (;;) {
        bool complete = true;
        for (size_t i = 0; i < votes.size(); ++i) {
            if (!pending[i]) continue;
            const int wanted = restoring ? votes[i].original : votes[i].target;
            if (Clock::now() >= limit) {
                tell(io, stage, policies[i], "scaling_min_freq", wanted, observed[i],
                     0, start, "timeout");
                return false;
            }
            errno = 0;
            if (!io.read(policies[i], "scaling_min_freq", &observed[i])) {
                const int error = errno;
                tell(io, stage, policies[i], "scaling_min_freq", wanted, -1, error, start, "read_failed");
                return false;
            }
            if (observed[i] != votes[i].original && observed[i] != votes[i].target) {
                tell(io, stage, policies[i], "scaling_min_freq", wanted, observed[i], 0, start, "foreign");
                return false;
            }
            if (!restoring) {
                int maximum = -1;
                errno = 0;
                if (!io.read(policies[i], "scaling_max_freq", &maximum)) {
                    const int error = errno;
                    tell(io, stage, policies[i], "scaling_max_freq", wanted, -1, error, start, "read_failed");
                    return false;
                }
                if (maximum < wanted) {
                    tell(io, stage, policies[i], "scaling_max_freq", wanted, maximum, 0, start, "cap_reduced");
                    return false;
                }
            }
            complete = complete && observed[i] == wanted;
        }
        if (Clock::now() >= limit) {
            tell(io, stage, -1, "scaling_min_freq", -1, -1, 0, start, "timeout");
            return false;
        }
        if (complete) {
            for (size_t i = 0; i < votes.size(); ++i) if (pending[i])
                tell(io, stage, policies[i], "scaling_min_freq",
                     restoring ? votes[i].original : votes[i].target, observed[i], 0, start, "observed");
            return true;
        }
        std::this_thread::sleep_until(std::min(limit, Clock::now() + pollPeriod));
    }
}

bool restoreVotes(Io& io, const Votes& votes) {
    const auto start = Clock::now();
    const auto limit = start + settleBudget;
    bool success = true;
    std::array<bool, 2> submitted{};
    for (size_t i = 0; i < votes.size(); ++i) {
        int current = -1;
        errno = 0;
        if (!io.read(policies[i], "scaling_min_freq", &current)) {
            const int error = errno;
            tell(io, "restore_check", policies[i], "scaling_min_freq", votes[i].original,
                 -1, error, start, "read_failed");
            success = false; continue;
        }
        // Effective foreign state cannot prove our hidden request is gone. Preserve
        // the foreign value AND the journal; block hints until ownership is resolved.
        if (current != votes[i].target && current != votes[i].original) {
            tell(io, "restore_check", policies[i], "scaling_min_freq", votes[i].original,
                 current, 0, start, "foreign");
            success = false; continue;
        }
        if (Clock::now() >= limit) { success = false; break; }
        // Even current==original may hide a queued target. An explicit original
        // store supersedes it under cpufreq's policy rwsem. V1 journals predate
        // all stores, so conservatively compensate BOTH rows, including untouched ones.
        errno = 0;
        submitted[i] = io.write(policies[i], votes[i].original);
        const int error = submitted[i] ? 0 : errno;
        tell(io, "restore_store", policies[i], "scaling_min_freq", votes[i].original,
             current, error, start, submitted[i] ? "accepted" : "write_failed");
        success = submitted[i] && success;
    }
    const bool settled = converge(io, votes, submitted, true, start, limit);
    if (!success || !settled) {
        tell(io, "journal", -1, "votes", -1, -1, 0, start, "retained");
        return false;
    }
    errno = 0;
    const bool cleared = io.clear();
    const int error = cleared ? 0 : errno;
    tell(io, "journal", -1, "votes", -1, -1, error, start, cleared ? "cleared" : "clear_failed");
    return cleared;
}
}  // namespace
bool recover(Io& io) {
    const auto start = Clock::now();
    Votes votes;
    errno = 0;
    const auto result = io.load(&votes);
    const int error = errno;
    tell(io, "recovery_load", -1, "votes", -1, -1,
         result == LoadResult::Absent || result == LoadResult::Valid ? 0 : error, start,
         result == LoadResult::Absent ? "absent" : result == LoadResult::Valid ? "valid" :
         result == LoadResult::RetryableIo ? "retryable" : "rejected");
    return result == LoadResult::Absent ||
           (result == LoadResult::Valid && restoreVotes(io, votes));
}

Boost::Boost(Io& io) : io_(io) {
    startup();
    worker_ = std::thread(&Boost::run, this);
}
void Boost::startup() {
    const auto start = Clock::now();
    Votes pending;
    errno = 0;
    const auto result = io_.load(&pending);
    const int error = errno;
    tell(io_, "startup_load", -1, "votes", -1, -1,
         result == LoadResult::Absent || result == LoadResult::Valid ? 0 : error, start,
         result == LoadResult::Absent ? "absent" : result == LoadResult::Valid ? "valid" :
         result == LoadResult::RetryableIo ? "retryable" : "rejected");
    recoveryPending_ = result == LoadResult::RetryableIo ||
                       (result == LoadResult::Valid && !restoreVotes(io_, pending));
    if (recoveryPending_ || result == LoadResult::Rejected) return;
    initialize();
}
void Boost::initialize() {
    const auto start = Clock::now();
    supported_ = true;
    for (size_t i = 0; supported_ && i < votes_.size(); ++i) {
        int maximum = 0;
        std::vector<int> values;
        for (const auto* node : {"scaling_min_freq", "scaling_max_freq"}) {
            int* value = std::string(node) == "scaling_min_freq" ? &votes_[i].original : &maximum;
            errno = 0;
            if (!io_.read(policies[i], node, value)) {
                const int error = errno;
                tell(io_, "initialize", policies[i], node, -1, -1, error, start, "read_failed");
                supported_ = false; break;
            }
        }
        if (!supported_) break;
        errno = 0;
        if (!io_.frequencies(policies[i], &values) || values.empty()) {
            const int error = errno;
            tell(io_, "initialize", policies[i], "scaling_available_frequencies", -1, -1,
                 error, start, "read_failed");
            supported_ = false; break;
        }
        std::sort(values.begin(), values.end());
        int target = votes_[i].original;
        for (int f : values) if (f > target && f <= maximum / 2) target = f;
        votes_[i].target = target;
        tell(io_, "initialize", policies[i], "scaling_min_freq", target, votes_[i].original,
             0, start, "baseline");
    }
    supported_ = supported_ && std::any_of(votes_.begin(), votes_.end(), [](const Vote& v) {
        return v.target > v.original;
    });
    tell(io_, "support", -1, "INTERACTION", -1, -1, 0, start, supported_ ? "enabled" : "disabled");
}
Boost::~Boost() {
    { std::lock_guard lock(mutex_); stop_ = true; cv_.notify_all(); }
    worker_.join();
}
bool Boost::supported() { std::lock_guard lock(mutex_); return supported_; }
bool Boost::restore() {
    const bool success = restoreVotes(io_, votes_);
    active_ = !success;
    deadline_ = success ? Clock::time_point{} : Clock::now() + settleBudget;
    if (!success) {
        supported_ = false;
        tell(io_, "support", -1, "INTERACTION", -1, -1, 0, Clock::now(), "disabled_pending_restore");
    }
    return success;
}
bool Boost::interaction(int durationMs) {
    std::lock_guard lock(mutex_);
    if (recoveryPending_ || (active_ && !supported_)) return false;
    if (durationMs < 0) { const bool ok = !active_ || restore(); cv_.notify_all(); return ok; }
    if (!supported_) return false;
    const auto admission = Clock::now();
    const int ms = durationMs == 0 ? 250 : std::min(durationMs, 1000);
    const auto expires = admission + std::chrono::milliseconds(ms);
    const auto limit = std::min(expires, admission + settleBudget);
    // Clear an expired previous request before admitting a fresh one.
    if (active_ && admission >= deadline_ && !restore()) return false;
    for (size_t i = 0; i < votes_.size(); ++i) {
        int current = -1, maximum = -1;
        errno = 0;
        const bool read = io_.read(policies[i], "scaling_min_freq", &current);
        const int error = read ? 0 : errno;
        const int expected = active_ ? votes_[i].target : votes_[i].original;
        if (!read || current != expected) {
            tell(io_, "hint_check", policies[i], "scaling_min_freq", expected, current, error,
                 admission, read ? "foreign" : "read_failed");
            supported_ = false;
            if (active_) restore();
            cv_.notify_all(); return false;
        }
        errno = 0;
        const bool capRead = io_.read(policies[i], "scaling_max_freq", &maximum);
        const int capError = capRead ? 0 : errno;
        if (!capRead || maximum < votes_[i].target) {
            tell(io_, "hint_check", policies[i], "scaling_max_freq", votes_[i].target, maximum,
                 capError, admission, capRead ? "cap_reduced" : "read_failed");
            supported_ = false;
            if (active_) restore();
            cv_.notify_all(); return false;
        }
    }
    if (Clock::now() >= limit) return false; // no new write after admission budget
    if (!active_) {
        errno = 0;
        if (!io_.save(votes_)) {
            const int error = errno;
            tell(io_, "journal", -1, "votes", -1, -1, error, admission, "save_failed");
            supported_ = false; return false;
        }
        active_ = true;
        deadline_ = expires; // Convergence never adds time to the requested lease.
        bool submitted = true;
        for (size_t i = 0; i < votes_.size(); ++i) {
            if (Clock::now() >= limit) { submitted = false; break; }
            errno = 0;
            const bool accepted = io_.write(policies[i], votes_[i].target);
            const int error = accepted ? 0 : errno;
            tell(io_, "target_store", policies[i], "scaling_min_freq", votes_[i].target,
                 votes_[i].original, error, admission, accepted ? "accepted" : "write_failed");
            if (!accepted) { submitted = false; break; }
        }
        if (!submitted || !converge(io_, votes_, {true, true}, false, admission, limit)) {
            supported_ = false;
            tell(io_, "support", -1, "INTERACTION", -1, -1, 0, admission, "disabled_after_target");
            restore(); cv_.notify_all(); return false;
        }
    }
    deadline_ = std::max(deadline_, expires);
    cv_.notify_all();
    return true;
}
void Boost::run() {
    std::unique_lock lock(mutex_);
    while (!stop_) {
        if (recoveryPending_) {
            cv_.wait_for(lock, settleBudget, [this] { return stop_; });
            if (!stop_) startup();
            continue;
        }
        if (!active_) { cv_.wait(lock); continue; }
        if (Clock::now() < deadline_) { cv_.wait_until(lock, deadline_); continue; }
        restore(); // failure schedules the next bounded attempt >=100ms later
    }
    if (active_) restore();
    if (recoveryPending_) recover(io_); // one bounded attempt, never loop during teardown
}
}  // namespace bengal::power

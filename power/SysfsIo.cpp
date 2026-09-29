// SPDX-License-Identifier: Apache-2.0
#include "Boost.h"
#include <cerrno>
#include <cstdio>
#include <fcntl.h>
#include <fstream>
#include <sstream>
#include <sys/file.h>
#include <sys/stat.h>
#include <unistd.h>
#ifdef __ANDROID__
#include <syslog.h>
#endif

namespace bengal::power {
namespace {
bool boundedText(const std::string& path, std::string* result) {
    const int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0) return false;
    char bytes[4096];
    const ssize_t size = ::read(fd, bytes, sizeof(bytes));
    const int readError = errno;
    close(fd);
    if (size < 0) { errno = readError; return false; }
    if (size == 0 || size == sizeof(bytes)) { errno = EBADMSG; return false; }
    result->assign(bytes, static_cast<size_t>(size));
    return true;
}
bool writeText(const char* name, const std::string& value, bool create) {
    const int fd = open(name, O_WRONLY | O_CLOEXEC | O_NOFOLLOW |
                       (create ? O_CREAT | O_TRUNC : 0), 0600);
    if (fd < 0) return false;
    const auto written = ::write(fd, value.data(), value.size());
    const bool ok = written == static_cast<ssize_t>(value.size());
    const int writeError = written < 0 ? errno : EIO;
    close(fd);
    if (!ok) errno = writeError; // close must not replace the failing store's errno.
    return ok;
}
}  // namespace
std::string SysfsIo::path(int policy, const std::string& node) const {
    return cpuRoot_ + "/policy" + std::to_string(policy) + "/" + node;
}
SysfsIo::~SysfsIo() { if (lock_ >= 0) close(lock_); }
bool SysfsIo::read(int policy, const std::string& node, int* value) {
    std::string data;
    if (!boundedText(path(policy, node), &data)) return false;
    std::istringstream stream(data);
    if (!(stream >> *value) || *value <= 0) { errno = EBADMSG; return false; }
    stream >> std::ws;
    if (!stream.eof()) { errno = EBADMSG; return false; }
    return true;
}
bool SysfsIo::frequencies(int policy, std::vector<int>* values) {
    std::string data;
    if (!boundedText(path(policy, "scaling_available_frequencies"), &data)) return false;
    std::istringstream stream(data);
    values->clear();
    int value;
    while (stream >> value) {
        if (value <= 0) { errno = EBADMSG; return false; }
        values->push_back(value);
    }
    if (!stream.eof() || values->empty()) { errno = EBADMSG; return false; }
    return true;
}
bool SysfsIo::write(int policy, int value) {
    return writeText(path(policy, "scaling_min_freq").c_str(), std::to_string(value), false);
}
LoadResult SysfsIo::load(Votes* votes) {
    const auto journal = stateRoot_ + "/votes";
    if (lock_ < 0) {
        lock_ = open((stateRoot_ + "/lock").c_str(), O_RDWR | O_CREAT | O_CLOEXEC | O_NOFOLLOW, 0600);
        if (lock_ < 0) return errno == ELOOP ? LoadResult::Rejected : LoadResult::RetryableIo;
        if (flock(lock_, LOCK_EX | LOCK_NB) != 0) {
            const int error = errno;
            close(lock_); lock_ = -1;
            errno = error;
            return error == EWOULDBLOCK ? LoadResult::Rejected : LoadResult::RetryableIo;
        }
    }
    struct stat st;
    if (lstat(journal.c_str(), &st) != 0)
        return errno == ENOENT ? LoadResult::Absent : LoadResult::RetryableIo;
    if (!S_ISREG(st.st_mode)) { errno = EBADMSG; return LoadResult::Rejected; }
    std::string data;
    if (!boundedText(journal, &data))
        return errno == EBADMSG || errno == ELOOP ? LoadResult::Rejected : LoadResult::RetryableIo;
    std::istringstream stream(data);
    std::string version;
    if (!(stream >> version) || version != "BENGAL_POWER_V1") {
        errno = EBADMSG; return LoadResult::Rejected;
    }
    for (auto& vote : *votes) {
        if (!(stream >> vote.original >> vote.target) || vote.original <= 0 ||
            vote.target < vote.original || vote.target > 3000000) {
            errno = EBADMSG; return LoadResult::Rejected;
        }
    }
    stream >> std::ws;
    if (!stream.eof()) { errno = EBADMSG; return LoadResult::Rejected; }
    return LoadResult::Valid;
}
bool SysfsIo::save(const Votes& votes) {
    const auto temporary = stateRoot_ + "/votes.new";
    const auto journal = stateRoot_ + "/votes";
    std::string data = "BENGAL_POWER_V1\n";
    for (const auto& vote : votes)
        data += std::to_string(vote.original) + " " + std::to_string(vote.target) + "\n";
    // Private tmpfs journal: survives service death, intentionally not a reboot.
    return writeText(temporary.c_str(), data, true) && rename(temporary.c_str(), journal.c_str()) == 0;
}
bool SysfsIo::clear() { return unlink((stateRoot_ + "/votes").c_str()) == 0 || errno == ENOENT; }
void SysfsIo::diagnostic(const Diagnostic& d) {
    const int savedErrno = errno;
    const auto now = std::chrono::steady_clock::now();
    const std::string key = std::string(d.stage) + ":" + std::to_string(d.policy) + ":" + d.result;
    const auto prior = diagnostics_.find(key);
    if (prior != diagnostics_.end() && now - prior->second < std::chrono::seconds(1)) {
        errno = savedErrno;
        return;
    }
    diagnostics_[key] = now;
    char message[512];
    std::snprintf(message, sizeof(message),
                  "BENGAL_POWER stage=%s policy=%d node=%s requested=%d observed=%d "
                  "errno=%d elapsed_ms=%lld result=%s", d.stage, d.policy, d.node,
                  d.requested, d.observed, d.error, static_cast<long long>(d.elapsedMs), d.result);
#ifdef __ANDROID__
    // Bionic's public syslog routes to logcat through async_safe_write_log.
    // No init stderr/kmsg dependency or additional shared-library requirement.
    syslog(LOG_INFO, "%s", message);
#else
    std::fprintf(stderr, "%s\n", message);
#endif
    errno = savedErrno;
}
}  // namespace bengal::power

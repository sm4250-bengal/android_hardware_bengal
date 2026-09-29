/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <cstdint>
#include <optional>
#include <sstream>
#include <string>

namespace bengal::gnss {
// Per-session scalar metadata only. Caller serializes access; no coordinates,
// satellite identities, protocol bytes, wall-clock samples or extra LOC owner.
class SignalDiagnostics {
public:
    void startAck() { started_ = true; }
    void sv(bool present, unsigned count) {
        ++svEvents_;
        listPresent_ = present;
        lastCount_ = count;
        if (!present) ++absent_;
        else if (!count) ++empty_;
        else ++nonempty_;
    }
    void android(unsigned nativeCount, std::optional<unsigned> convertedCount) {
        if (!convertedCount) {
            ++suppressed_;
            omittedEntries_ += nativeCount;
        } else {
            ++converted_;
            convertedEntries_ += *convertedCount;
            omittedEntries_ += nativeCount - *convertedCount;
        }
    }
    void engine(uint32_t raw) {
        ++engineEvents_;
        if (engineKnown_ && raw != engineRaw_) ++engineChanges_;
        engineKnown_ = true;
        engineRaw_ = raw;
    }
    // First snapshot, then at most one per 30 monotonic seconds, plus exactly
    // one final snapshot. Event-driven: silence produces no periodic heartbeat.
    std::optional<std::string> summary(uint64_t nowMs, bool final = false) {
        if (finished_ || (!final && logged_ &&
            (nowMs < lastLogMs_ || nowMs - lastLogMs_ < 30000))) return std::nullopt;
        logged_ = true;
        finished_ = final;
        lastLogMs_ = nowMs;
        std::ostringstream out;
        out << "reception summary final=" << final << " start-ack=" << started_
            << " engine-events=" << engineEvents_ << " engine-changes=" << engineChanges_
            << " engine-raw=";
        if (engineKnown_) out << engineRaw_; else out << "unknown";
        out << " sv-indications=" << svEvents_ << " list-absent=" << absent_
            << " list-empty=" << empty_ << " list-nonempty=" << nonempty_
            << " latest-list-count=";
        if (listPresent_) out << lastCount_; else out << "unknown";
        out << " android-converted-reports=" << converted_
            << " android-converted-entries=" << convertedEntries_
            << " android-suppressed-reports=" << suppressed_
            << " android-omitted-entries=" << omittedEntries_;
        return out.str();
    }
private:
    bool started_ = false, engineKnown_ = false, listPresent_ = false;
    bool logged_ = false, finished_ = false;
    uint32_t engineRaw_ = 0;
    unsigned lastCount_ = 0;
    uint64_t lastLogMs_ = 0, svEvents_ = 0, absent_ = 0, empty_ = 0, nonempty_ = 0;
    uint64_t converted_ = 0, convertedEntries_ = 0, suppressed_ = 0, omittedEntries_ = 0;
    uint64_t engineEvents_ = 0, engineChanges_ = 0;
};
}

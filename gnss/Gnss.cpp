/* SPDX-License-Identifier: Apache-2.0 */
#define LOG_TAG "BengalGnss"
#include "Gnss.h"
#include "Accuracy.h"
#include "Satellite.h"
#include "TimeAiding.h"
#include "SignalDiagnostics.h"
#include <log/log.h>
#include <chrono>
#include <algorithm>
#include <cmath>
#include <cstdio>
#include <cstring>
#include <sys/file.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/system_properties.h>

namespace bengal::gnss {
struct Gnss::State {
    std::mutex mutex;
    std::condition_variable changed;
    std::atomic<bool> cancel{true};
    bool quit = false, started = false, finished = true, failed = false, unclean = false;
    uint64_t generation = 0, registration = 0;
    std::shared_ptr<g::IGnssCallback> callback;
    struct Event { uint64_t generation; int kind; std::optional<g::GnssLocation> location; };
    std::deque<Event> events;
    SatelliteStream<std::vector<g::IGnssCallback::GnssSvInfo>> svStream;
    SignalDiagnostics diagnostics;
    TimeAiding timeAiding;
};
static void receptionSummary(SignalDiagnostics& diagnostics, bool final = false) {
    const auto now = std::chrono::duration_cast<std::chrono::milliseconds>(
        std::chrono::steady_clock::now().time_since_epoch()).count();
    if (auto summary = diagnostics.summary(now, final)) ALOGI("%s", summary->c_str());
}
struct Gnss::Cookie { std::weak_ptr<State> state; uint64_t registration; };
static bool publish(const char* state) {
    if (__system_property_set("vendor.bengal.gnss.state", state) == 0) return true;
    ALOGE("cannot publish GNSS cleanup state=%s", state);
    return false;
}

// Convert only proven native fields; accuracy conversion is separately tested.
static g::GnssLocation location(const loc_fix& f) {
    g::GnssLocation out{};
    out.gnssLocationFlags = g::GnssLocation::HAS_LAT_LONG;
    out.latitudeDegrees = f.latitude;
    out.longitudeDegrees = f.longitude;
    out.timestampMillis = static_cast<int64_t>(f.utc_ms);
    if (f.fields & LOC_FIX_ALTITUDE) { out.altitudeMeters = f.altitude_m; out.gnssLocationFlags |= g::GnssLocation::HAS_ALTITUDE; }
    if (f.fields & LOC_FIX_SPEED) { out.speedMetersPerSec = f.speed_mps; out.gnssLocationFlags |= g::GnssLocation::HAS_SPEED; }
    if (f.fields & LOC_FIX_BEARING) { out.bearingDegrees = f.bearing_deg; out.gnssLocationFlags |= g::GnssLocation::HAS_BEARING; }
    if (auto accuracy = horizontalAccuracy68(f)) {
        out.horizontalAccuracyMeters = *accuracy;
        out.gnssLocationFlags |= g::GnssLocation::HAS_HORIZONTAL_ACCURACY;
    }
    if (auto accuracy = verticalAccuracy68(f)) {
        out.verticalAccuracyMeters = *accuracy;
        out.gnssLocationFlags |= g::GnssLocation::HAS_VERTICAL_ACCURACY;
    }
    // Speed/bearing uncertainty confidence has no established wire contract.
    // Don't mark it as Android68%, and don't pretend AP receive time is a
    // modem-synchronized clock. Framework Utils.cpp supplies receive time
    // when elapsedRealtime.flags is zero.
    return out;
}

Gnss::Gnss() : state_(std::make_shared<State>()), death_(AIBinder_DeathRecipient_new(died)) {
    char prior[PROP_VALUE_MAX]{};
    __system_property_get("vendor.bengal.gnss.state", prior);
    // Only boot initialization establishes idle after a crashed service.
    state_->unclean = std::strcmp(prior, "idle") != 0;
    if (state_->unclean) publish("failed");
    AIBinder_DeathRecipient_setOnUnlinked(death_, unlinked);
    // Exactly one dispatcher per service, not per session. A blocked remote
    // Binder callback must never block the transport worker's Stop/cleanup.
    // Shared state owns its lifetime; no detached thread retains `this`.
    std::thread(dispatch, state_).detach();
}
Gnss::~Gnss() {
    close();
    AIBinder_DeathRecipient_delete(death_);
    std::lock_guard guard(state_->mutex);
    state_->quit = true;
    state_->changed.notify_all();
}
void Gnss::died(void* pointer) {
    auto* cookie = static_cast<Cookie*>(pointer);
    if (auto s = cookie->state.lock()) {
        std::lock_guard guard(s->mutex);
        if (s->registration != cookie->registration) return;
        s->cancel.store(true);
        s->callback.reset();
        s->timeAiding.clear();
        ++s->generation;
        s->events.clear();
        s->svStream.clear();
        s->changed.notify_all();
        ALOGW("framework died; native session cancellation requested");
    }
}
void Gnss::unlinked(void* p) { delete static_cast<Cookie*>(p); }
void Gnss::unlinkLocked() {
    if (linked_) AIBinder_unlinkToDeath(linked_->asBinder().get(), death_, cookie_);
    linked_.reset();
    cookie_ = nullptr;
}
void Gnss::stopLocked() {
    bool hadSession;
    {
        std::lock_guard guard(state_->mutex);
        hadSession = state_->started && !state_->finished;
        if (!state_->finished && !publish("stopping")) state_->unclean = true;
        bool metadata = std::any_of(state_->events.begin(), state_->events.end(),
                                    [](const auto& e) { return e.kind == 0; });
        state_->cancel.store(true);
        ++state_->generation;
        state_->events.clear();
        state_->svStream.clear();
        if (metadata) state_->events.push_back({state_->generation, 0, std::nullopt});
    }
    if (worker_.joinable()) worker_.join();
    std::lock_guard guard(state_->mutex);
    // SESSION_END describes this client stream, not physical ENGINE_OFF.
    state_->started = false;
    if (!publish(state_->unclean ? "failed" : "idle")) state_->unclean = true;
    if (hadSession) state_->events.push_back({state_->generation, 2, std::nullopt});
    state_->changed.notify_all();
}
Status Gnss::setCallback(const std::shared_ptr<g::IGnssCallback>& cb) {
    if (!cb) return Status::fromExceptionCode(EX_ILLEGAL_ARGUMENT);
    std::lock_guard serial(lifecycle_);
    stopLocked();
    unlinkLocked();
    uint64_t registration;
    {
        std::lock_guard guard(state_->mutex);
        state_->callback = cb;
        state_->svStream.stop();
        state_->timeAiding.clear();
        registration = ++state_->registration;
        ++state_->generation;
        state_->events.clear();
        state_->svStream.clear();
    }
    auto* cookie = new Cookie{state_, registration};
    if (AIBinder_linkToDeath(cb->asBinder().get(), death_, cookie) != STATUS_OK) {
        std::lock_guard guard(state_->mutex);
        state_->callback.reset();
        // onUnlinked owns cookie even if linking fails.
        return Status::fromServiceSpecificError(ERROR_GENERIC);
    }
    linked_ = cb; cookie_ = cookie;
    std::lock_guard guard(state_->mutex);
    state_->events.push_back({state_->generation, 0, std::nullopt});
    state_->changed.notify_all();
    return Status::ok();
}
Status Gnss::close() {
    std::lock_guard serial(lifecycle_);
    {
        std::lock_guard guard(state_->mutex);
        state_->callback.reset();
        state_->timeAiding.clear();
        state_->svStream.stop();
        ++state_->registration;
    }
    stopLocked();
    unlinkLocked();
    std::lock_guard guard(state_->mutex);
    return state_->unclean ? Status::fromServiceSpecificError(ERROR_GENERIC) : Status::ok();
}
Status Gnss::stop() {
    std::lock_guard serial(lifecycle_);
    stopLocked();
    std::lock_guard guard(state_->mutex);
    return state_->unclean ? Status::fromServiceSpecificError(ERROR_GENERIC) : Status::ok();
}
Status Gnss::startSvStatus() {
    std::lock_guard guard(state_->mutex);
    if (!state_->callback) return Status::fromServiceSpecificError(ERROR_GENERIC);
    state_->svStream.start();
    return Status::ok();
}
Status Gnss::stopSvStatus() {
    std::lock_guard guard(state_->mutex);
    state_->svStream.stop();
    return Status::ok();
}
Status Gnss::injectTime(int64_t utc, int64_t reference, int uncertainty) {
    std::lock_guard guard(state_->mutex);
    if (!state_->callback || state_->unclean || state_->quit)
        return Status::fromServiceSpecificError(ERROR_GENERIC);
    // Binder only replaces a bounded sample; it never sends QMI or waits for
    // modem completion. A same-registration pre-start sample remains useful.
    if (!state_->timeAiding.offer(state_->registration, {utc, reference, uncertainty}, loc_boottime_ms()))
        return Status::fromServiceSpecificError(ERROR_INVALID_ARGUMENT);
    ALOGI("UTC aiding queued (modem acceptance pending)");
    return Status::ok();
}
Status Gnss::setPositionMode(const PositionModeOptions& o) {
    if (o.mode != GnssPositionMode::STANDALONE || o.recurrence != GnssPositionRecurrence::RECURRENCE_PERIODIC || o.lowPowerMode)
        return unsupported();
    if (o.minIntervalMs < 0 || o.preferredAccuracyMeters < 0 || o.preferredTimeMs < 0)
        return Status::fromServiceSpecificError(ERROR_INVALID_ARGUMENT);
    std::lock_guard serial(lifecycle_);
    const unsigned interval = std::max(1000, o.minIntervalMs);
    if (interval == interval_) return Status::ok();
    bool restart;
    {
        std::lock_guard guard(state_->mutex);
        restart = !state_->finished && !state_->cancel.load();
    }
    if (restart) stopLocked();
    interval_ = interval;
    return restart ? startLocked() : Status::ok();
}
Status Gnss::start() {
    std::lock_guard serial(lifecycle_);
    return startLocked();
}
Status Gnss::startLocked() {
    {
        std::lock_guard guard(state_->mutex);
        if (!state_->callback || state_->unclean) return Status::fromServiceSpecificError(ERROR_GENERIC);
        if (!state_->finished && !state_->cancel.load()) return Status::ok();
    }
    stopLocked();
    {
        std::lock_guard guard(state_->mutex);
        if (state_->unclean) return Status::fromServiceSpecificError(ERROR_GENERIC);
        state_->cancel.store(false);
        state_->started = false; state_->finished = false; state_->failed = false;
        state_->diagnostics = SignalDiagnostics{};
        if (!publish("starting")) {
            state_->cancel.store(true); state_->finished = true; state_->unclean = true;
            return Status::fromServiceSpecificError(ERROR_GENERIC);
        }
    }
    const auto s = state_;
    const unsigned interval = interval_;
    worker_ = std::thread([s, interval] {
        // No raw coordinates in logcat/disk. Protocol logs go to /dev/null;
        // the separate manual diagnostic is the explicit private capture path.
        FILE* log = fopen("/dev/null", "we");
        int owner = open("/data/vendor/gnss/bengal-owner.lock", O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW, 0600);
        loc_io io{};
        int result = 1;
        if (log && owner >= 0 && flock(owner, LOCK_EX | LOCK_NB) == 0 && !s->cancel.load() && loc_qrtr_open(&io) == 0) {
            loc_observer o{};
            o.ctx = s.get(); o.interval_ms = interval;
            o.cancelled = [](void* p) { return static_cast<State*>(p)->cancel.load() ? 1 : 0; };
            o.started = [](void* p) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                state->started = true;
                state->diagnostics.startAck();
                receptionSummary(state->diagnostics);
                if (!publish("running")) { state->cancel.store(true); state->unclean = true; }
                state->events.push_back({state->generation, 1, std::nullopt});
                state->changed.notify_all();
            };
            o.request_time = [](void* p) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                if (state->cancel.load() || !state->callback) return;
                // One queued request plus at most one in-flight Binder call.
                if (std::none_of(state->events.begin(), state->events.end(),
                                 [](const auto& event) { return event.kind == 5; }))
                    state->events.push_back({state->generation, 5, std::nullopt});
                state->changed.notify_all();
            };
            o.take_time = [](void* p, loc_time_sample* sample) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                return !state->cancel.load() && state->callback &&
                    state->timeAiding.take(state->registration, *sample) ? 1 : 0;
            };
            o.time_result = [](void*, int result) {
                if (result == 0) ALOGI("UTC aiding accepted: QMI response and LOC completion confirmed (no fix claim)");
                else if (result == 3) ALOGW("UTC aiding completion timed out; UTC disabled for this client, standalone session continues");
                else ALOGW("UTC aiding %s; standalone session continues", result == 1 ? "refused" : "expired or invalid");
            };
            o.fix = [](void* p, const loc_fix* fix) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                if (state->cancel.load()) return;
                // Bound the queue even if the framework stops servicing Binder.
                for (auto it = state->events.begin(); it != state->events.end();) {
                    if (it->location) it = state->events.erase(it); else ++it;
                }
                state->events.push_back({state->generation, 3, location(*fix)});
                state->changed.notify_all();
            };
            o.sv_metadata = [](void* p, int present, unsigned count) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                if (state->cancel.load()) return;
                state->diagnostics.sv(present != 0, count);
                // A present list's summary follows Android conversion below.
                if (!present) receptionSummary(state->diagnostics);
            };
            o.engine_state = [](void* p, uint32_t raw) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                if (state->cancel.load()) return;
                state->diagnostics.engine(raw);
                receptionSummary(state->diagnostics);
            };
            o.satellites = [](void* p, const loc_sv_report* report) {
                auto* state = static_cast<State*>(p);
                std::lock_guard guard(state->mutex);
                if (state->cancel.load()) return;
                auto converted = satellites(*report);
                if (!converted) {
                    // Preserve unknown reception, rather than fake an empty report.
                    state->svStream.clear();
                    state->diagnostics.android(report->count, std::nullopt);
                    receptionSummary(state->diagnostics);
                    return;
                }
                state->diagnostics.android(report->count, converted->size());
                receptionSummary(state->diagnostics);
                std::vector<g::IGnssCallback::GnssSvInfo> values;
                for (const auto& sv : *converted) {
                    g::IGnssCallback::GnssSvInfo value{};
                    value.svid=sv.id;
                    switch (sv.system) {
                        case SvSystem::GPS: value.constellation=g::GnssConstellationType::GPS; break;
                        case SvSystem::SBAS: value.constellation=g::GnssConstellationType::SBAS; break;
                        case SvSystem::GLONASS: value.constellation=g::GnssConstellationType::GLONASS; break;
                        case SvSystem::QZSS: value.constellation=g::GnssConstellationType::QZSS; break;
                        case SvSystem::BEIDOU: value.constellation=g::GnssConstellationType::BEIDOU; break;
                        case SvSystem::GALILEO: value.constellation=g::GnssConstellationType::GALILEO; break;
                        case SvSystem::IRNSS: value.constellation=g::GnssConstellationType::IRNSS; break;
                    }
                    value.cN0Dbhz=sv.cn0; value.basebandCN0DbHz=sv.baseband;
                    value.elevationDegrees=sv.elevation; value.azimuthDegrees=sv.azimuth;
                    if (sv.ephemeris) value.svFlag |= static_cast<int>(g::IGnssCallback::GnssSvFlags::HAS_EPHEMERIS_DATA);
                    if (sv.almanac) value.svFlag |= static_cast<int>(g::IGnssCallback::GnssSvFlags::HAS_ALMANAC_DATA);
                    // TRACK is not USED_IN_FIX; carrier/signal/clock absent.
                    values.push_back(value);
                }
                state->svStream.offer(std::move(values));
                state->changed.notify_all();
            };
            result = loc_follow(&io, log, &o);
        }
        if (log) fclose(log);
        if (owner >= 0) ::close(owner);
        std::lock_guard guard(s->mutex);
        s->finished = true; s->failed = result != 0;
        s->unclean = s->unclean || result == 2;
        if (!publish(s->unclean ? "failed" : "idle")) s->unclean = true;
        if (!s->cancel.load()) {
            s->events.clear();
            s->svStream.clear();
            s->events.push_back({s->generation, 2, std::nullopt});
        }
        s->changed.notify_all();
        receptionSummary(s->diagnostics, true);
        ALOGI("native session finished result=%d (0=confirmed cleanup)", result);
    });
    std::unique_lock guard(state_->mutex);
    bool done = state_->changed.wait_for(guard, std::chrono::seconds(13), [&] {
        return state_->started || state_->finished || !state_->callback;
    });
    if (done && state_->started && !state_->cancel.load()) return Status::ok();
    guard.unlock();
    stopLocked();
    return Status::fromServiceSpecificError(ERROR_GENERIC);
}

void Gnss::dispatch(std::shared_ptr<State> s) {
    for (;;) {
        State::Event event;
        std::shared_ptr<g::IGnssCallback> cb;
        std::optional<std::vector<g::IGnssCallback::GnssSvInfo>> sv;
        {
            std::unique_lock guard(s->mutex);
            s->changed.wait(guard, [&] { return s->quit || !s->events.empty() || s->svStream.pending(); });
            if (s->quit) return;
            if (!s->events.empty()) {
                event = std::move(s->events.front()); s->events.pop_front();
            } else {
                sv = s->svStream.take();
                event = {s->generation, 4, std::nullopt};
                if (s->cancel.load()) continue;
            }
            if (event.generation != s->generation || !s->callback) continue;
            cb = s->callback;
        }
        // No mutex is held across a remote call, and transport cleanup runs on
        // another thread. One already in-flight callback may finish after stop.
        Status result = Status::ok();
        if (event.kind == 0) {
            result = cb->gnssSetCapabilitiesCb(g::IGnssCallback::CAPABILITY_SCHEDULING);
            if (result.isOk()) {
                g::IGnssCallback::GnssSystemInfo info;
                // OEM device3 gnssSetSystemInfoCb reports2018; do not infer
                // the receiver revision from the SoC marketing launch year.
                info.yearOfHw = 2018; info.name = "Bengal native LOC GNSS 0.1 (standalone)";
                result = cb->gnssSetSystemInfoCb(info);
            }
            if (result.isOk()) result = cb->gnssSetSignalTypeCapabilitiesCb({});
        } else if (sv) result = cb->gnssSvStatusCb(*sv);
        else if (event.location) result = cb->gnssLocationCb(*event.location);
        else if (event.kind == 5) result = cb->gnssRequestTimeCb();
        else result = cb->gnssStatusCb(event.kind == 1 ? g::IGnssCallback::GnssStatusValue::SESSION_BEGIN : g::IGnssCallback::GnssStatusValue::SESSION_END);
        if (!result.isOk()) {
            std::lock_guard guard(s->mutex);
            if (event.generation == s->generation) {
                s->cancel.store(true); s->events.clear(); s->svStream.clear(); s->timeAiding.clear();
                ALOGW("callback failed; native session cancellation requested");
            }
        }
    }
}
} // namespace bengal::gnss

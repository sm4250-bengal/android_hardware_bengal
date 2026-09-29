// SPDX-License-Identifier: Apache-2.0
#include "Boost.h"
#include <aidl/android/hardware/power/BnPower.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <cstring>

namespace p = aidl::android::hardware::power;
using ndk::ScopedAStatus;
static ScopedAStatus unsupported() {
    return ScopedAStatus::fromExceptionCodeWithMessage(EX_UNSUPPORTED_OPERATION,
                                                     "Not implemented by Bengal Power HAL");
}
class Power final : public p::BnPower {
  public:
    Power() : boost_(io_) {}
    ScopedAStatus setMode(p::Mode, bool) override { return unsupported(); }
    ScopedAStatus isModeSupported(p::Mode, bool* out) override {
        *out = false; return ScopedAStatus::ok();
    }
    ScopedAStatus isBoostSupported(p::Boost type, bool* out) override {
        *out = type == p::Boost::INTERACTION && boost_.supported();
        return ScopedAStatus::ok();
    }
    ScopedAStatus setBoost(p::Boost type, int32_t ms) override {
        if (type != p::Boost::INTERACTION) return unsupported();
        if (!boost_.interaction(ms))
            return ScopedAStatus::fromExceptionCodeWithMessage(EX_ILLEGAL_STATE,
                                                              "CPU floor request failed");
        return ScopedAStatus::ok();
    }
    ScopedAStatus createHintSession(int32_t, int32_t, const std::vector<int32_t>&, int64_t,
            std::shared_ptr<p::IPowerHintSession>* out) override {
        *out = nullptr; return unsupported();
    }
    ScopedAStatus createHintSessionWithConfig(int32_t, int32_t, const std::vector<int32_t>&,
            int64_t, p::SessionTag, p::SessionConfig*,
            std::shared_ptr<p::IPowerHintSession>* out) override {
        *out = nullptr; return unsupported();
    }
    ScopedAStatus getHintSessionPreferredRate(int64_t* out) override {
        *out = 0; return unsupported();
    }
    ScopedAStatus getSessionChannel(int32_t, int32_t, p::ChannelConfig*) override {
        return unsupported();
    }
    ScopedAStatus closeSessionChannel(int32_t, int32_t) override { return unsupported(); }
    ScopedAStatus getSupportInfo(p::SupportInfo* out) override {
        *out = p::SupportInfo{};
        out->usesSessions = false;
        out->boosts = boost_.supported() ? int64_t{1} << static_cast<int>(p::Boost::INTERACTION) : 0;
        out->modes = out->sessionHints = out->sessionModes = out->sessionTags = 0;
        out->compositionData.isSupported = false;
        out->headroom.isCpuSupported = out->headroom.isGpuSupported = false;
        return ScopedAStatus::ok();
    }
    ScopedAStatus getCpuHeadroom(const p::CpuHeadroomParams&, p::CpuHeadroomResult*) override {
        return unsupported();
    }
    ScopedAStatus getGpuHeadroom(const p::GpuHeadroomParams&, p::GpuHeadroomResult*) override {
        return unsupported();
    }
    ScopedAStatus sendCompositionData(const std::vector<p::CompositionData>&) override {
        return unsupported();
    }
    ScopedAStatus sendCompositionUpdate(const p::CompositionUpdate&) override { return unsupported(); }
  private:
    bengal::power::SysfsIo io_;
    bengal::power::Boost boost_;
};
int main(int argc, char** argv) {
    if (argc == 2 && std::strcmp(argv[1], "--restore") == 0) {
        bengal::power::SysfsIo io;
        return bengal::power::recover(io) ? 0 : 1;
    }
    if (argc != 1) return 64;
    ABinderProcess_setThreadPoolMaxThreadCount(2);
    auto service = ndk::SharedRefBase::make<Power>();
    if (AServiceManager_addService(service->asBinder().get(),
                                  "android.hardware.power.IPower/default") != STATUS_OK) return 1;
    ABinderProcess_joinThreadPool();
    return 1;
}

/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include <aidl/android/hardware/gnss/BnGnss.h>
#include <android/binder_ibinder.h>
#include <atomic>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <optional>
#include <thread>
#include "loc.h"

namespace bengal::gnss {
namespace g = aidl::android::hardware::gnss;
using Status = ndk::ScopedAStatus;
Status unsupported();
std::shared_ptr<g::IGnssConfiguration> configuration();
std::shared_ptr<g::IGnssMeasurementInterface> measurements();
std::shared_ptr<g::IGnssPowerIndication> power();
std::shared_ptr<g::IGnssDebug> debug();
std::shared_ptr<g::IAGnss> agnss();
std::shared_ptr<g::IAGnssRil> ril();
std::shared_ptr<g::IGnssAntennaInfo> antenna();
std::shared_ptr<g::visibility_control::IGnssVisibilityControl> visibility();
std::shared_ptr<g::gnss_assistance::IGnssAssistanceInterface> assistance();

class Gnss final : public g::BnGnss {
public:
    Gnss();
    ~Gnss() override;
    Status setCallback(const std::shared_ptr<g::IGnssCallback>&) override;
    Status close() override;
    Status start() override;
    Status stop() override;
    Status setPositionMode(const PositionModeOptions&) override;
    Status injectTime(int64_t, int64_t, int) override;
    Status injectLocation(const g::GnssLocation&) override { return unsupported(); }
    Status injectBestLocation(const g::GnssLocation&) override { return unsupported(); }
    Status deleteAidingData(GnssAidingData) override { return unsupported(); }
    Status startSvStatus() override;
    Status stopSvStatus() override;
    Status startNmea() override { return unsupported(); }
    Status stopNmea() override { return Status::ok(); }

    Status getExtensionGnssConfiguration(std::shared_ptr<g::IGnssConfiguration>* p) override { *p = configuration(); return Status::ok(); }
    Status getExtensionGnssMeasurement(std::shared_ptr<g::IGnssMeasurementInterface>* p) override { *p = measurements(); return Status::ok(); }
    Status getExtensionGnssPowerIndication(std::shared_ptr<g::IGnssPowerIndication>* p) override { *p = power(); return Status::ok(); }
    Status getExtensionGnssDebug(std::shared_ptr<g::IGnssDebug>* p) override { *p = debug(); return Status::ok(); }
    Status getExtensionAGnss(std::shared_ptr<g::IAGnss>* p) override { *p = agnss(); return Status::ok(); }
    Status getExtensionAGnssRil(std::shared_ptr<g::IAGnssRil>* p) override { *p = ril(); return Status::ok(); }
    Status getExtensionGnssAntennaInfo(std::shared_ptr<g::IGnssAntennaInfo>* p) override { *p = antenna(); return Status::ok(); }
    Status getExtensionGnssVisibilityControl(std::shared_ptr<g::visibility_control::IGnssVisibilityControl>* p) override { *p = visibility(); return Status::ok(); }
    Status getExtensionGnssAssistanceInterface(std::shared_ptr<g::gnss_assistance::IGnssAssistanceInterface>* p) override { *p = assistance(); return Status::ok(); }
    // Android17's framework wraps successful extension returns without checking
    // the Binder pointer. Success + null crashes system_server (D3-0146 PSDS).
    // Report absence as unsupported; never advertise an unusable interface.
    Status getExtensionPsds(std::shared_ptr<g::IGnssPsds>* p) override { p->reset(); return unsupported(); }
    Status getExtensionGnssBatching(std::shared_ptr<g::IGnssBatching>* p) override { p->reset(); return unsupported(); }
    Status getExtensionGnssGeofence(std::shared_ptr<g::IGnssGeofence>* p) override { p->reset(); return unsupported(); }
    Status getExtensionGnssNavigationMessage(std::shared_ptr<g::IGnssNavigationMessageInterface>* p) override { p->reset(); return unsupported(); }
    Status getExtensionMeasurementCorrections(std::shared_ptr<g::measurement_corrections::IMeasurementCorrectionsInterface>* p) override { p->reset(); return unsupported(); }

private:
    struct State;
    struct Cookie;
    std::shared_ptr<State> state_;
    std::mutex lifecycle_;
    std::thread worker_;
    AIBinder_DeathRecipient* death_;
    Cookie* cookie_ = nullptr; // Freed only by onUnlinked, never dereferenced here.
    std::shared_ptr<g::IGnssCallback> linked_;
    unsigned interval_ = 1000;
    Status startLocked();
    void stopLocked();
    void unlinkLocked();
    static void died(void*);
    static void unlinked(void*);
    static void dispatch(std::shared_ptr<State>);
};
} // namespace bengal::gnss

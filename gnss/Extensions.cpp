/* SPDX-License-Identifier: Apache-2.0 */
#include "Gnss.h"
#include <aidl/android/hardware/gnss/BnAGnss.h>
#include <aidl/android/hardware/gnss/BnAGnssRil.h>
#include <aidl/android/hardware/gnss/BnGnssConfiguration.h>
#include <aidl/android/hardware/gnss/BnGnssMeasurementInterface.h>
#include <aidl/android/hardware/gnss/BnGnssPowerIndication.h>
#include <aidl/android/hardware/gnss/BnGnssDebug.h>
#include <aidl/android/hardware/gnss/BnGnssAntennaInfo.h>
#include <aidl/android/hardware/gnss/visibility_control/BnGnssVisibilityControl.h>
#include <aidl/android/hardware/gnss/gnss_assistance/BnGnssAssistanceInterface.h>

namespace bengal::gnss {
Status unsupported() { return Status::fromExceptionCode(EX_UNSUPPORTED_OPERATION); }
// Non-null required interfaces. Unsupported operations are explicit, never
// successful no-ops that claim assistance, measurements, blocklists or power
// accounting which this receiver path does not implement.
struct Configuration final : g::BnGnssConfiguration {
    Status setSuplVersion(int) override { return unsupported(); }
    Status setSuplMode(int) override { return unsupported(); }
    Status setLppProfile(int) override { return unsupported(); }
    Status setGlonassPositioningProtocol(int) override { return unsupported(); }
    Status setEmergencySuplPdn(bool) override { return unsupported(); }
    Status setEsExtensionSec(int) override { return unsupported(); }
    Status setBlocklist(const std::vector<g::BlocklistedSource>& v) override { return v.empty() ? Status::ok() : unsupported(); }
};
struct Measurements final : g::BnGnssMeasurementInterface {
    Status setCallback(const std::shared_ptr<g::IGnssMeasurementCallback>&, bool, bool) override { return unsupported(); }
    Status setCallbackWithOptions(const std::shared_ptr<g::IGnssMeasurementCallback>&, const Options&) override { return unsupported(); }
    Status close() override { return Status::ok(); }
};
struct Power final : g::BnGnssPowerIndication {
    Status setCallback(const std::shared_ptr<g::IGnssPowerIndicationCallback>& cb) override {
        if (!cb) return Status::fromExceptionCode(EX_ILLEGAL_ARGUMENT);
        return cb->setCapabilitiesCb(0); // No measured energy counters.
    }
    Status requestGnssPowerStats() override { return unsupported(); }
};
struct Debug final : g::BnGnssDebug {
    Status getDebugData(DebugData*) override { return unsupported(); }
};
struct AGnss final : g::BnAGnss {
    Status setCallback(const std::shared_ptr<g::IAGnssCallback>& cb) override { return cb ? Status::ok() : Status::fromExceptionCode(EX_ILLEGAL_ARGUMENT); }
    Status dataConnClosed() override { return unsupported(); }
    Status dataConnFailed() override { return unsupported(); }
    Status setServer(g::IAGnssCallback::AGnssType, const std::string&, int) override { return unsupported(); }
    Status dataConnOpen(int64_t, const std::string&, ApnIpType) override { return unsupported(); }
};
struct Ril final : g::BnAGnssRil {
    Status setCallback(const std::shared_ptr<g::IAGnssRilCallback>& cb) override { return cb ? Status::ok() : Status::fromExceptionCode(EX_ILLEGAL_ARGUMENT); }
    Status setRefLocation(const AGnssRefLocation&) override { return unsupported(); }
    Status setSetId(SetIdType, const std::string&) override { return unsupported(); }
    Status updateNetworkState(const NetworkAttributes&) override { return unsupported(); }
    Status injectNiSuplMessageData(const std::vector<uint8_t>&, int) override { return unsupported(); }
};
struct Antenna final : g::BnGnssAntennaInfo {
    Status setCallback(const std::shared_ptr<g::IGnssAntennaInfoCallback>&) override { return unsupported(); }
    Status close() override { return Status::ok(); }
};
struct Visibility final : g::visibility_control::BnGnssVisibilityControl {
    Status setCallback(const std::shared_ptr<g::visibility_control::IGnssVisibilityControlCallback>& cb) override { return cb ? Status::ok() : Status::fromExceptionCode(EX_ILLEGAL_ARGUMENT); }
    Status enableNfwLocationAccess(const std::vector<std::string>& names) override { return names.empty() ? Status::ok() : unsupported(); }
};
struct Assistance final : g::gnss_assistance::BnGnssAssistanceInterface {
    Status injectGnssAssistance(const g::gnss_assistance::GnssAssistance&) override { return unsupported(); }
    Status setCallback(const std::shared_ptr<g::gnss_assistance::IGnssAssistanceCallback>&) override { return unsupported(); }
};
std::shared_ptr<g::IGnssConfiguration> configuration() { return ndk::SharedRefBase::make<Configuration>(); }
std::shared_ptr<g::IGnssMeasurementInterface> measurements() { return ndk::SharedRefBase::make<Measurements>(); }
std::shared_ptr<g::IGnssPowerIndication> power() { return ndk::SharedRefBase::make<Power>(); }
std::shared_ptr<g::IGnssDebug> debug() { return ndk::SharedRefBase::make<Debug>(); }
std::shared_ptr<g::IAGnss> agnss() { return ndk::SharedRefBase::make<AGnss>(); }
std::shared_ptr<g::IAGnssRil> ril() { return ndk::SharedRefBase::make<Ril>(); }
std::shared_ptr<g::IGnssAntennaInfo> antenna() { return ndk::SharedRefBase::make<Antenna>(); }
std::shared_ptr<g::visibility_control::IGnssVisibilityControl> visibility() { return ndk::SharedRefBase::make<Visibility>(); }
std::shared_ptr<g::gnss_assistance::IGnssAssistanceInterface> assistance() { return ndk::SharedRefBase::make<Assistance>(); }
} // namespace bengal::gnss

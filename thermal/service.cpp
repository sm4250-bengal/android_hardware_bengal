// SPDX-License-Identifier: Apache-2.0
#include "ThermalSensors.h"
#include <aidl/android/hardware/thermal/BnThermal.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <algorithm>
#include <cmath>
#include <condition_variable>
#include <map>
#include <mutex>
#include <optional>
#include <thread>

namespace t = aidl::android::hardware::thermal;
using ndk::ScopedAStatus;
using bengal::thermal::Sensors;
static ScopedAStatus failure(const char* message, int code = EX_ILLEGAL_STATE) {
    return ScopedAStatus::fromExceptionCodeWithMessage(code, message);
}
template<class Callback> struct Listener {
    std::shared_ptr<Callback> callback;
    std::optional<int> type;
    std::map<std::string, int64_t> last;
};
class Thermal final : public t::BnThermal {
  public:
    Thermal() : worker_(&Thermal::poll, this) {}
    ~Thermal() {
        { std::lock_guard lock(mutex_); stop_ = true; cv_.notify_all(); }
        worker_.join();
    }
    ScopedAStatus getTemperatures(std::vector<t::Temperature>* out) override {
        out->clear();
        if (!sensors_.initialized()) return failure("Thermal inventory unavailable");
        for (const auto& sensor : sensors_.inventory()) {
            const auto reading = sensors_.read(sensor);
            t::Temperature value;
            value.name = sensor.name;
            value.type = static_cast<t::TemperatureType>(sensor.type);
            value.value = reading.temperature;
            value.throttlingStatus = static_cast<t::ThrottlingSeverity>(reading.severity);
            out->push_back(value);
        }
        return ScopedAStatus::ok();
    }
    ScopedAStatus getTemperaturesWithType(t::TemperatureType type,
                                         std::vector<t::Temperature>* out) override {
        auto result = getTemperatures(out); filter(out, type); return result;
    }
    ScopedAStatus getTemperatureThresholds(std::vector<t::TemperatureThreshold>* out) override {
        out->clear();
        if (!sensors_.initialized()) return failure("Thermal inventory unavailable");
        for (const auto& sensor : sensors_.inventory()) {
            t::TemperatureThreshold value;
            value.name = sensor.name;
            value.type = static_cast<t::TemperatureType>(sensor.type);
            value.hotThrottlingThresholds.assign(7, bengal::thermal::unavailable);
            value.coldThrottlingThresholds.assign(7, bengal::thermal::unavailable);
            for (const auto& trip : sensor.trips) {
                const int severity = Sensors::severity(trip.type);
                if (!severity) continue;
                float& target = value.hotThrottlingThresholds[severity];
                if (std::isnan(target) || trip.temperature < target) target = trip.temperature;
            }
            out->push_back(value);
        }
        return ScopedAStatus::ok();
    }
    ScopedAStatus getTemperatureThresholdsWithType(t::TemperatureType type,
            std::vector<t::TemperatureThreshold>* out) override {
        auto result = getTemperatureThresholds(out); filter(out, type); return result;
    }
    ScopedAStatus getCoolingDevices(std::vector<t::CoolingDevice>* out) override {
        out->clear();
        if (!sensors_.initialized()) return failure("Thermal inventory unavailable");
        for (const auto& cooler : sensors_.coolers()) {
            int current, maximum;
            if (!Sensors::readInteger(cooler.path + "/cur_state", &current) ||
                !Sensors::readInteger(cooler.path + "/max_state", &maximum) ||
                current < 0 || current > maximum) {
                out->clear(); return failure("Cooling state unavailable; no integer sentinel exists");
            }
            t::CoolingDevice value;
            value.type = static_cast<t::CoolingType>(cooler.type);
            value.name = cooler.name;
            value.value = current;
            // V3's optional power statistics are unavailable, not estimated.
            value.powerLimitMw = value.powerMw = value.timeWindowMs = 0;
            out->push_back(value);
        }
        return ScopedAStatus::ok();
    }
    ScopedAStatus getCoolingDevicesWithType(t::CoolingType type,
                                            std::vector<t::CoolingDevice>* out) override {
        auto result = getCoolingDevices(out); filter(out, type); return result;
    }
    ScopedAStatus registerThermalChangedCallback(
            const std::shared_ptr<t::IThermalChangedCallback>& callback) override {
        return add(temperatures_, callback, std::nullopt);
    }
    ScopedAStatus registerThermalChangedCallbackWithType(
            const std::shared_ptr<t::IThermalChangedCallback>& callback,
            t::TemperatureType type) override {
        return add(temperatures_, callback, static_cast<int>(type));
    }
    ScopedAStatus unregisterThermalChangedCallback(
            const std::shared_ptr<t::IThermalChangedCallback>& callback) override {
        return remove(temperatures_, callback);
    }
    ScopedAStatus registerCoolingDeviceChangedCallbackWithType(
            const std::shared_ptr<t::ICoolingDeviceChangedCallback>& callback,
            t::CoolingType type) override {
        return add(coolers_, callback, static_cast<int>(type));
    }
    ScopedAStatus unregisterCoolingDeviceChangedCallback(
            const std::shared_ptr<t::ICoolingDeviceChangedCallback>& callback) override {
        return remove(coolers_, callback);
    }
    ScopedAStatus forecastSkinTemperature(int32_t seconds, float* out) override {
        *out = bengal::thermal::unavailable;
        return failure("No measured skin sensor or forecasting model",
                       seconds < 0 ? EX_ILLEGAL_ARGUMENT : EX_UNSUPPORTED_OPERATION);
    }
  private:
    template<class Value, class Type> static void filter(std::vector<Value>* values, Type type) {
        values->erase(std::remove_if(values->begin(), values->end(), [&](const Value& value) {
            return value.type != type;
        }), values->end());
    }
    template<class C> ScopedAStatus add(std::vector<Listener<C>>& list,
            const std::shared_ptr<C>& callback, std::optional<int> type) {
        if (!callback) return failure("Null callback", EX_ILLEGAL_ARGUMENT);
        if (!sensors_.initialized()) return failure("Thermal inventory unavailable");
        std::lock_guard lock(mutex_);
        if (std::any_of(list.begin(), list.end(), [&](const auto& value) {
            return value.callback->asBinder() == callback->asBinder();
        })) return failure("Callback already registered", EX_ILLEGAL_ARGUMENT);
        if (list.size() >= 64) return failure("Callback capacity exhausted");
        list.push_back({callback, type, {}}); cv_.notify_all();
        return ScopedAStatus::ok();
    }
    template<class C> ScopedAStatus remove(std::vector<Listener<C>>& list,
                                         const std::shared_ptr<C>& callback) {
        if (!callback) return failure("Null callback", EX_ILLEGAL_ARGUMENT);
        std::lock_guard lock(mutex_);
        auto found = std::find_if(list.begin(), list.end(), [&](const auto& value) {
            return value.callback->asBinder() == callback->asBinder();
        });
        if (found == list.end()) return failure("Callback not registered", EX_ILLEGAL_ARGUMENT);
        list.erase(found); return ScopedAStatus::ok();
    }
    template<class C, class Value, class Send> void notify(std::vector<Listener<C>>& list,
            const Value& value, int64_t state, Send send) {
        std::vector<std::shared_ptr<C>> callbacks;
        {
            std::lock_guard lock(mutex_);
            for (auto& listener : list) {
                if (listener.type && *listener.type != static_cast<int>(value.type)) continue;
                auto previous = listener.last.find(value.name);
                if (previous != listener.last.end() && previous->second == state) continue;
                listener.last[value.name] = state;
                callbacks.push_back(listener.callback);
            }
        }
        // Never hold our mutex across an external Binder transaction.
        for (const auto& callback : callbacks) if (!send(callback, value).isOk()) remove(list, callback);
    }
    void poll() {
        std::unique_lock lock(mutex_);
        while (!stop_) {
            if (temperatures_.empty() && coolers_.empty()) { cv_.wait(lock); continue; }
            lock.unlock();
            std::vector<t::Temperature> temperatures;
            if (getTemperatures(&temperatures).isOk()) for (const auto& value : temperatures) {
                if (!std::isfinite(value.value)) continue; // never emit a false cooling-down edge
                notify(temperatures_, value, static_cast<int64_t>(value.throttlingStatus),
                       [](const auto& cb, const auto& v) { return cb->notifyThrottling(v); });
            }
            std::vector<t::CoolingDevice> coolers;
            if (getCoolingDevices(&coolers).isOk()) for (const auto& value : coolers)
                notify(coolers_, value, value.value,
                       [](const auto& cb, const auto& v) { return cb->notifyCoolingDeviceChanged(v); });
            lock.lock();
            cv_.wait_for(lock, std::chrono::seconds(1));
        }
    }
    Sensors sensors_;
    std::mutex mutex_;
    std::condition_variable cv_;
    bool stop_ = false;
    std::vector<Listener<t::IThermalChangedCallback>> temperatures_;
    std::vector<Listener<t::ICoolingDeviceChangedCallback>> coolers_;
    std::thread worker_;
};
static_assert(static_cast<int>(t::TemperatureType::SOC) == 13);
static_assert(static_cast<int>(t::ThrottlingSeverity::SHUTDOWN) == 6);
int main(int argc, char**) {
    if (argc != 1) return 64;
    ABinderProcess_setThreadPoolMaxThreadCount(2);
    auto service = ndk::SharedRefBase::make<Thermal>();
    if (AServiceManager_addService(service->asBinder().get(),
                                  "android.hardware.thermal.IThermal/default") != STATUS_OK) return 1;
    ABinderProcess_joinThreadPool();
    return 1;
}

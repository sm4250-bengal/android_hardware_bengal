// SPDX-License-Identifier: Apache-2.0
#include "ThermalSensors.h"

#include <algorithm>
#include <charconv>
#include <cmath>
#include <filesystem>
#include <fstream>

namespace bengal::thermal {
namespace {
bool text(const std::string& path, std::string* value) {
    std::ifstream file(path);
    char bytes[257]{};
    file.read(bytes, sizeof(bytes));
    if (file.bad() || file.gcount() == 0 || file.gcount() > 256) return false;
    *value = std::string(bytes, static_cast<size_t>(file.gcount()));
    while (!value->empty() && (value->back() == '\n' || value->back() == '\r'))
        value->pop_back();
    return !value->empty();
}
bool numberSuffix(const std::string& name, const std::string& prefix) {
    return name.size() > prefix.size() && name.compare(0, prefix.size(), prefix) == 0 &&
           std::all_of(name.begin() + prefix.size(), name.end(), [](char c) {
               return c >= '0' && c <= '9';
           });
}
}  // namespace

bool Sensors::readInteger(const std::string& path, int* value) {
    std::string data;
    if (!text(path, &data)) return false;
    auto result = std::from_chars(data.data(), data.data() + data.size(), *value);
    return result.ec == std::errc{} && result.ptr == data.data() + data.size();
}

Type Sensors::classify(const std::string& name) {
    // Only the exact D3 SM6115 zone names with a demonstrated physical meaning.
    if (name == "cpu4-thermal" || name == "cpu5-thermal" || name == "cpu6-thermal" ||
        name == "cpu7-thermal" || name == "cpu45-thermal" || name == "cpu67-thermal" ||
        name == "cpu0123-thermal") return Type::Cpu;
    if (name == "gpu-thermal") return Type::Gpu;
    if (name == "mapss-thermal" || name == "cdsp-hvx-thermal" || name == "wlan-thermal" ||
        name == "camera-thermal" || name == "video-thermal" || name == "modem0-thermal" ||
        name == "modem1-thermal" || name == "display-thermal") return Type::Soc;
    // PMIC is not skin/battery/ambient, and unknown future names stay UNKNOWN.
    return Type::Unknown;
}

Sensors::Sensors(const std::string& root) {
    std::error_code error;
    std::vector<std::string> paths;
    auto iterator = std::filesystem::directory_iterator(root, error);
    const auto end = std::filesystem::directory_iterator{};
    for (; !error && iterator != end; iterator.increment(error)) {
        const auto& entry = *iterator;
        if (numberSuffix(entry.path().filename().string(), "thermal_zone"))
            paths.push_back(entry.path().string());
        if (numberSuffix(entry.path().filename().string(), "cooling_device")) {
            std::string name;
            if (!text(entry.path().string() + "/type", &name)) return;
            int type = 6;  // COMPONENT unless driver identity is known.
            if (name.compare(0, 16, "thermal-cpufreq-") == 0) type = 2;
            if (name == "5900000.gpu") type = 3;
            coolers_.push_back({entry.path().string(), name + ":" + entry.path().filename().string(), type});
        }
    }
    if (error || paths.size() > 128 || coolers_.size() > 128) return;
    std::sort(coolers_.begin(), coolers_.end(), [](const Cooler& a, const Cooler& b) {
        return a.path < b.path;
    });
    std::sort(paths.begin(), paths.end());
    for (const auto& path : paths) {
        std::string name;
        if (!text(path + "/type", &name)) { sensors_.clear(); return; }
        if (std::any_of(sensors_.begin(), sensors_.end(), [&](const Sensor& s) {
                return s.name == name;
            })) { sensors_.clear(); return; }
        Sensor sensor{path, name, classify(name), {}};
        // Snapshot static thresholds at startup; never substitute canned values.
        for (int i = 0; i < 32; ++i) {
            const auto tripPath = path + "/trip_point_" + std::to_string(i);
            std::string type;
            if (!std::filesystem::exists(tripPath + "_type", error)) {
                if (error) { sensors_.clear(); return; }
                continue;
            }
            if (!text(tripPath + "_type", &type) || !severity(type)) {
                sensors_.clear(); return;
            }
            int milli;
            if (!readInteger(tripPath + "_temp", &milli) || milli < -273150 || milli > 300000) {
                sensors_.clear(); return;
            }
            sensor.trips.push_back({type, static_cast<float>(milli) / 1000.0f});
        }
        sensors_.push_back(std::move(sensor));
    }
}

Reading Sensors::read(const Sensor& sensor) const {
    Reading result;
    int milli;
    std::string currentName;
    // Do not silently adopt a different sensor if the same zone index is reused.
    if (!text(sensor.path + "/type", &currentName) || currentName != sensor.name ||
        !readInteger(sensor.path + "/temp", &milli) || milli < -273150 || milli > 300000)
        return result;
    result.temperature = static_cast<float>(milli) / 1000.0f;
    for (const auto& trip : sensor.trips) {
        if (result.temperature < trip.temperature) continue;
        result.severity = std::max(result.severity, severity(trip.type));
    }
    return result;
}
int Sensors::severity(const std::string& type) {
    if (type == "passive") return 1;
    if (type == "hot") return 3;
    if (type == "critical") return 6;
    return 0;
}
}  // namespace bengal::thermal

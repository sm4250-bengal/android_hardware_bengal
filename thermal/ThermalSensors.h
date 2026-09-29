// SPDX-License-Identifier: Apache-2.0
#pragma once

#include <limits>
#include <string>
#include <vector>

namespace bengal::thermal {
constexpr float unavailable = std::numeric_limits<float>::quiet_NaN();
// Values match the frozen Android thermal V3 enums; checked by the Binder adapter.
enum class Type { Unknown = -1, Cpu = 0, Gpu = 1, Soc = 13 };
struct Trip {
    std::string type;
    float temperature = unavailable;
};
struct Sensor {
    std::string path;
    std::string name;
    Type type = Type::Unknown;
    std::vector<Trip> trips;
};
struct Reading {
    float temperature = unavailable;
    // Declared reporting policy, not measured UX impact or cooling engagement:
    // passive -> LIGHT(1), hot -> SEVERE(3), critical -> SHUTDOWN(6).
    int severity = 0;
};
struct Cooler { std::string path; std::string name; int type = 6; };

// Root injection exists only in this internal backend, never on the production CLI.
class Sensors {
  public:
    explicit Sensors(const std::string& root = "/sys/class/thermal");
    const std::vector<Sensor>& inventory() const { return sensors_; }
    const std::vector<Cooler>& coolers() const { return coolers_; }
    bool initialized() const { return !sensors_.empty(); }
    Reading read(const Sensor& sensor) const;
    static Type classify(const std::string& name);
    static bool readInteger(const std::string& path, int* value);
    static int severity(const std::string& tripType);
  private:
    std::vector<Sensor> sensors_;
    std::vector<Cooler> coolers_;
};
}  // namespace bengal::thermal

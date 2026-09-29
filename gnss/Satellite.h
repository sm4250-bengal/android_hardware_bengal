/* SPDX-License-Identifier: Apache-2.0 */
#pragma once
#include "satellite.h"
#include <optional>
#include <vector>
#include <utility>
namespace bengal::gnss {
enum class SvSystem { GPS, SBAS, GLONASS, QZSS, BEIDOU, GALILEO, IRNSS };
struct Satellite {
    SvSystem system;
    int id;
    float cn0, baseband, elevation, azimuth;
    bool ephemeris, almanac;
};
inline std::optional<Satellite> satellite(const loc_sv& s) {
    constexpr unsigned required = LOC_SV_SYSTEM | LOC_SV_ID | LOC_SV_CN0 |
                                  LOC_SV_ELEVATION | LOC_SV_AZIMUTH;
    if ((s.valid & required) != required || !s.id) return std::nullopt;
    Satellite out{};
    out.id = s.id;
    // LOC IDL2.134 has global numbering for Galileo/BDS/NavIC but local
    // GLONASS numbering. Android AIDL7 uses local numbering for all four.
    switch (s.system) {
        case 1: if (s.id > 32) return std::nullopt; out.system=SvSystem::GPS; break;
        case 2: if (s.id < 301 || s.id > 336) return std::nullopt;
                out.system=SvSystem::GALILEO; out.id-=300; break;
        case 3: if (!((s.id>=120 && s.id<=151) || (s.id>=183 && s.id<=191))) return std::nullopt;
                out.system=SvSystem::SBAS; break;
        case 5: if (s.id > 25) return std::nullopt; out.system=SvSystem::GLONASS; break;
        case 6: if (s.id<201 || s.id>263) return std::nullopt;
                out.system=SvSystem::BEIDOU; out.id-=200; break;
        case 7: if (s.id<193 || s.id>197) return std::nullopt; out.system=SvSystem::QZSS; break;
        case 8: if (s.id<401 || s.id>414) return std::nullopt;
                out.system=SvSystem::IRNSS; out.id-=400; break;
        default: return std::nullopt; // Deprecated COMPASS is not silently BDS.
    }
    out.cn0=s.cn0; out.elevation=s.elevation; out.azimuth=s.azimuth;
    if (s.valid & LOC_SV_BASEBAND_CN0) out.baseband=s.baseband_cn0;
    else if ((s.valid & LOC_SV_STATUS) && s.status==2 && s.cn0==0) out.baseband=0;
    else return std::nullopt; // Android permits zero for searched/not tracked.
    out.ephemeris=(s.valid & LOC_SV_INFO) && (s.info & 1);
    out.almanac=(s.valid & LOC_SV_INFO) && (s.info & 2);
    return out;
}
inline std::optional<std::vector<Satellite>> satellites(const loc_sv_report& report) {
    std::vector<Satellite> out;
    for (unsigned i=0;i<report.count;++i) if (auto sv=satellite(report.sv[i])) out.push_back(*sv);
    if (report.count && out.empty()) return std::nullopt; // Unavailable != zero SVs.
    return out;
}
/* Caller serializes all accesses. One latest report is retained even when a
 * remote callback blocks. stop/clear drop queued data; take marks it in-flight.
 * One already in-flight call may finish after stop, like location callbacks. */
template<class T> class SatelliteStream {
    bool enabled_ = false;
    std::optional<T> pending_;
public:
    void start() { enabled_=true; }
    void stop() { enabled_=false; clear(); }
    void clear() { pending_.reset(); }
    bool pending() const { return pending_.has_value(); }
    void offer(T value) { if (enabled_) pending_=std::move(value); }
    std::optional<T> take() { auto out=std::move(pending_); pending_.reset(); return out; }
};
} // namespace bengal::gnss

/* SPDX-License-Identifier: Apache-2.0 */
#include "Satellite.h"
#include <cassert>
#include <cstdio>
using namespace bengal::gnss;
int main() {
    loc_sv s{};
    s.valid=255|LOC_SV_BASEBAND_CN0; s.system=1; s.id=7; s.status=3; s.info=3;
    s.cn0=37.5; s.baseband_cn0=35.5; s.elevation=45; s.azimuth=123;
    auto v=satellite(s); assert(v && v->id==7 && v->system==SvSystem::GPS);
    assert(v->cn0==37.5 && v->baseband==35.5 && v->ephemeris && v->almanac);
    for(unsigned bit : {LOC_SV_SYSTEM, LOC_SV_ID, LOC_SV_CN0, LOC_SV_ELEVATION, LOC_SV_AZIMUTH}) {
        loc_sv bad=s; bad.valid &= ~bit; assert(!satellite(bad));
    }
    s.valid &= ~LOC_SV_BASEBAND_CN0; assert(!satellite(s));
    s.cn0=0; assert(!satellite(s)); // TRACK + unavailable baseband is not 0.
    s.status=2; assert(satellite(s)->baseband==0);
    s.valid &= ~LOC_SV_STATUS; assert(!satellite(s));
    s.valid=255|LOC_SV_BASEBAND_CN0; s.status=3; s.cn0=37.5;
    struct Case { unsigned system, input, expected; SvSystem output; };
    for (auto c : {Case{1,32,32,SvSystem::GPS}, {2,301,1,SvSystem::GALILEO},
        {2,336,36,SvSystem::GALILEO}, {3,120,120,SvSystem::SBAS},
        {3,191,191,SvSystem::SBAS}, {5,25,25,SvSystem::GLONASS},
        {6,201,1,SvSystem::BEIDOU}, {6,263,63,SvSystem::BEIDOU},
        {7,193,193,SvSystem::QZSS}, {8,401,1,SvSystem::IRNSS}, {8,414,14,SvSystem::IRNSS}}) {
        s.system=c.system; s.id=c.input; auto a=satellite(s);
        assert(a && a->id==static_cast<int>(c.expected) && a->system==c.output);
    }
    for (auto c : {Case{1,33,0,SvSystem::GPS}, {2,1,0,SvSystem::GALILEO},
        {3,152,0,SvSystem::SBAS}, {4,201,0,SvSystem::BEIDOU}, {5,26,0,SvSystem::GLONASS},
        {6,200,0,SvSystem::BEIDOU}, {7,198,0,SvSystem::QZSS}, {8,415,0,SvSystem::IRNSS}}) {
        s.system=c.system; s.id=c.input; assert(!satellite(s));
    }
    loc_sv_report report{}; auto empty=satellites(report); assert(empty && empty->empty());
    report.count=1; report.sv[0]=s; assert(!satellites(report));
    report.count=2; s.system=1; s.id=7; report.sv[1]=s;
    assert(satellites(report)->size()==1);
    SatelliteStream<int> stream;
    stream.offer(1); assert(!stream.pending());
    stream.start();
    for(int i=0;i<10000;++i) stream.offer(i);
    assert(stream.take()==9999 && !stream.pending());
    stream.offer(2); stream.stop(); assert(!stream.take());
    stream.offer(3); stream.start(); assert(!stream.take());
    stream.offer(4); stream.clear(); assert(!stream.take());
    stream.offer(5); assert(stream.take()==5);
    puts("PASS SV adapter: strict fields, true empty vs unavailable, constellation/SVID and bounded stop/restart stream");
}

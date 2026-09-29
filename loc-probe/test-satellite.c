/* SPDX-License-Identifier: Apache-2.0 */
#include "satellite.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
/* Independent little-endian wire fixture, deliberately not a native struct. */
static const uint8_t entry[28] = {
  0xff,0,0,0, 1,0,0,0, 7,0, 1, 3,0,0,0, 3,
  0,0,0x34,0x42, 0,0,0xf6,0x42, 0,0,0x16,0x42
};
int main(void) {
    const uint8_t *v[256] = {0}; unsigned n[256] = {0};
    uint8_t altitude=0, list[1+176*29]={0}; struct loc_sv_report r;
    assert(loc_satellite_decode(v,n,&r)==-1);
    v[1]=&altitude; n[1]=1;
    assert(loc_satellite_decode(v,n,&r)==0);
    v[0x10]=list; n[0x10]=1;
    assert(loc_satellite_decode(v,n,&r)==1 && r.count==0);
    list[0]=1; memcpy(list+1,entry,28); n[0x10]=29;
    assert(loc_satellite_decode(v,n,&r)==1 && r.count==1);
    assert(r.sv[0].valid==255 && r.sv[0].id==7 && r.sv[0].status==3);
    assert(r.sv[0].elevation==45 && r.sv[0].azimuth==123 && r.sv[0].cn0==37.5);
    for (unsigned len=0;len<60;++len) {
        n[0x10]=len;
        assert(loc_satellite_decode(v,n,&r)==(len==29 ? 1 : -1));
    }
    n[0x10]=29; list[0]=81; assert(loc_satellite_decode(v,n,&r)==-1);
    list[0]=1; altitude=2; assert(loc_satellite_decode(v,n,&r)==-1); altitude=0;
    n[1]=4; assert(loc_satellite_decode(v,n,&r)==-1); n[1]=1;
    /* Each optional validity bit is honored even if bytes look usable. */
    for(unsigned bit=0;bit<8;++bit) {
        list[1]=(uint8_t)(1u<<bit);
        assert(loc_satellite_decode(v,n,&r)==1 && r.sv[0].valid==(1u<<bit));
    }
    list[1]=255; list[2]=255; /* unknown validity bits ignored */
    assert(loc_satellite_decode(v,n,&r)==1 && r.sv[0].valid==255);
    list[2]=0;
    /* NaN, infinity, negative and out-of-range must lose the optional bit. */
    const uint8_t bad[][4]={{0,0,0xc0,0x7f},{0,0,0x80,0x7f},{0,0,0x80,0xbf}};
    for(unsigned field=0;field<3;++field) for(unsigned b=0;b<3;++b) {
        memcpy(list+1,entry,28); memcpy(list+1+16+4*field,bad[b],4);
        assert(loc_satellite_decode(v,n,&r)==1);
        assert(!(r.sv[0].valid & (32u<<field)));
    }
    memcpy(list+1,entry,28); list[5]=99; list[9]=0; list[11]=2; list[12]=99;
    assert(loc_satellite_decode(v,n,&r)==1 && !(r.sv[0].valid & 15));
    v[0x11]=list; n[0x11]=30; assert(loc_satellite_decode(v,n,&r)==-1);
    v[0x10]=NULL; n[0x10]=0;
    for(unsigned count=0;count<=176;++count) {
        list[0]=count;
        for(unsigned i=0;i<count;++i) {memcpy(list+1+29*i,entry,28);list[1+29*i+28]=8;}
        n[0x11]=1+count*29;
        assert(loc_satellite_decode(v,n,&r)==1 && r.count==count);
    }
    list[0]=177; assert(loc_satellite_decode(v,n,&r)==-1);
    list[0]=1; memcpy(list+1,entry,28); n[0x11]=30;
    uint8_t loss[]={1,0,0,0,0x40}; // 2 dB RF loss.
    v[0x13]=loss; n[0x13]=5;
    assert(loc_satellite_decode(v,n,&r)==1);
    assert((r.sv[0].valid & LOC_SV_BASEBAND_CN0) && r.sv[0].baseband_cn0==35.5);
    for(unsigned len=0;len<10;++len) {
        n[0x13]=len; assert(loc_satellite_decode(v,n,&r)==(len==5 ? 1 : -1));
    }
    n[0x13]=5; loss[0]=0; assert(loc_satellite_decode(v,n,&r)==-1); loss[0]=1;
    for(unsigned b=0;b<3;++b) {
        memcpy(loss+1,bad[b],4); assert(loc_satellite_decode(v,n,&r)==1);
        assert(!(r.sv[0].valid & LOC_SV_BASEBAND_CN0));
    }
    loss[1]=0;loss[2]=0;loss[3]=0x48;loss[4]=0x42; // 50 dB > measured CN0.
    assert(loc_satellite_decode(v,n,&r)==1 && !(r.sv[0].valid & LOC_SV_BASEBAND_CN0));
    v[0x11]=NULL;v[0x10]=list;n[0x10]=29;
    assert(loc_satellite_decode(v,n,&r)==-1); // RF loss requires expanded list.
    puts("PASS native SV: absent/empty, scalar validity, malformed lengths, count limits, expanded176");
}

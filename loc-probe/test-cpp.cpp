/* SPDX-License-Identifier: Apache-2.0 */
// Compile and link the actual C interface from C++, using atomic cancellation.
#include "loc.h"
#include <atomic>
#include <cassert>
#include <cstdio>
static int cancel(void *p) { return static_cast<std::atomic<bool> *>(p)->load(); }
int main() {
    std::atomic<bool> stopped{true};
    loc_observer observer{&stopped, cancel, nullptr, nullptr, 1000, nullptr, nullptr, nullptr, nullptr, nullptr, nullptr};
    assert(observer.cancelled(observer.ctx));
    const uint8_t *values[256]{};
    unsigned lengths[256]{};
    loc_fix fix{};
    assert(loc_position_decode(values, lengths, 1, &fix) == -1);
    puts("PASS: C++ caller links native C decoder and atomic cancellation interface");
}

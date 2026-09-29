// SPDX-License-Identifier: Apache-2.0
// AOSP NDK binder_status.h's non-Bionic __assert macro collides with glibc's
// repeated assert.h declarations. Keep the real function and add Bionic's
// argument order as a C++ overload. No AIDL or Binder interfaces are stubbed.
#include <cassert>
#define __assert __assert
inline void __assert(const char* file, int line, const char* expression) {
    __assert_fail(expression, file, line, "Binder header host syntax check");
}

/* SPDX-License-Identifier: Apache-2.0 */
#define LOG_TAG "BengalGnss"
#include "Gnss.h"
#include <android/binder_manager.h>
#include <android/binder_process.h>
#include <log/log.h>
#include <cstring>

int main(int argc, char** argv) {
    // Refuse accidental manual launches before creating a service/worker.
    if (argc != 2 || std::strcmp(argv[1], "--serve")) return 2;
    ABinderProcess_setThreadPoolMaxThreadCount(4);
    auto service = ndk::SharedRefBase::make<bengal::gnss::Gnss>();
    const std::string name = std::string(bengal::gnss::g::IGnss::descriptor) + "/default";
    if (AServiceManager_addService(service->asBinder().get(), name.c_str()) != STATUS_OK) return 1;
    ALOGI("registered native standalone LOC provider; receiver idle until start");
    ABinderProcess_joinThreadPool();
    return 1;
}

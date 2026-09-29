// SPDX-License-Identifier: Apache-2.0
// Reuse the actual owner's existing OS fake and real process/socket harness.
#define main legacy_desk_main
#include "desk.cpp"
#undef main

namespace {
void vendor_contracts() {
    auto old = definitions("fixture");
    require(vendor_registration_error(old[4]) == "NON_UNIQ_SYMBOL", "real legacy ambiguity reproduced");
    const auto pos = old[4].find("qcom_geni_serial_start_tx_dma");
    old[4].insert(pos, "bengal_bt_uart:");
    require(vendor_registration_error(old[4]) == "NO_BTF_ENTRY", "prefix-only patch still needs absent module BTF");
    for (const auto& def : dma_d84_definitions("fixture")) {
        require(vendor_registration_error(def).empty(), "current vendor definition registers: " + def);
        require(vendor_registration_error(def, false).empty(), "pending vendor definition registers: " + def);
    }
    puts("PASS real legacy ambiguous + prefix-only BTF negatives; current vendor definitions accepted");
}
void contracts() {
    require(dma_d84_target_matches(kDmaD84Release, "aarch64", 4096), "exact target");
    for (const auto& release : {"6.18.32-4k-g92378ced66f1", "", "6.18.32-4k-gd84cfa4479d9-dirty"})
        require(!dma_d84_target_matches(release, "aarch64", 4096), "wrong release");
    require(!dma_d84_target_matches(kDmaD84Release, "x86_64", 4096) &&
            !dma_d84_target_matches(kDmaD84Release, "aarch64", 16384) &&
            !dma_d84_target_matches(kDmaD84Release, "aarch64", -1), "architecture/page refusal");
    const auto legacy = definitions("fixture"), dma = dma_d84_definitions("fixture");
    require(legacy.size() == 7 && dma.size() == 14 && kDmaD84ProbeMask == ((1UL << dma.size()) - 1), "complete mask");
    for (size_t i = 0; i < legacy.size(); ++i) {
        if (i == 4 || i == 5) require(dma[i].find("bengal_bt_uart:") != std::string::npos &&
                dma[i].find(i == 4 ? "state=+208(%x0):x64 mapbase=+328(%x0):x64 line=+316(%x0):u32" :
                            "state=+208($arg1):x64 mapbase=+328($arg1):x64 line=+316($arg1):u32") != std::string::npos,
                "vendor sites use witnessed RAM offsets");
        else require(dma[i] == legacy[i], "five retained events");
    }
    require(dma[7].find("_flush_in bengal_bt_uart:qcom_geni_serial_flush_buffer_dma") != std::string::npos &&
            dma[8].find("r128:") == 0 && dma[8].find("remaining=+720($arg1):u32") != std::string::npos,
            "paired flush state");
    require(dma[9].find("_prep_in geni_se_tx_dma_prep se=$arg1:x64 len=$arg3:u64 iova_ptr=$arg4:x64") != std::string::npos &&
            dma[10].find("dma_addr=+0($arg4):x64 ret=$retval:s32") != std::string::npos,
            "prep saved entry argument and actual return");
    require(dma[11].find("_unprep geni_se_tx_dma_unprep se=$arg1:x64 dma_addr=$arg2:x64 len=$arg3:u64 caller=%x30:symstr") != std::string::npos,
            "unprep SE identity and caller");
    require(dma[12].find("_gate_in __uart_start state=$arg1:x64") != std::string::npos &&
            dma[12].find("sw_stop=state->port.tty->flow.stopped:u8") != std::string::npos &&
            dma[12].find("hw_stop=state->uart_port->hw_stopped:u8") != std::string::npos &&
            dma[12].find("tx_en=state->uart_port->port_dev->tx_enabled ") != std::string::npos,
            "entry observes both actual stop flags and BTF-extracted tx_enabled bitfield");
    require(dma[13].find("_gate_pm __uart_start+76 uport=%x20:x64") != std::string::npos &&
            dma[13].find("pm_ret=%x0:s32") != std::string::npos &&
            dma[13].find("pm_status=+508(%x19):s32") != std::string::npos,
            "compiled post-PM site observes actual return and serial-port runtime state");
    for (const auto& def : dma) {
        std::istringstream words(def);
        std::string word;
        while (words >> word) {
            const auto equal = word.find('=');
            if (equal != std::string::npos)
                require(word.size() - equal - 1 <= 63, "kernel fetch expression length bound");
        }
        require(def.find("handle_tx_dma") == std::string::npos &&
                def.find("membase") == std::string::npos, "no inlined symbol or MMIO fetch");
        for (const auto& other : dma) if (def != other)
            require(other.find(name_of(def)) == std::string::npos, "no event-name prefix collisions");
    }
    require(summarize(Result{}).find("mode=") == std::string::npos, "legacy status unchanged");
    puts("PASS exact target, 14 definitions, TX/PM gates, offsets, caller and legacy contracts");
}

void processes() {
    unsigned n = 0;
    for (const auto& fault : {"", "vendor-symbols", "vendor-pending", "wrong-target", "dma-registration-fail", "dma-partial-registration",
                              "late-on", "late-readback", "stop-off-fail", "trace-large", "append-collision",
                              "diag-write:_fifo/filter", "diag-read:_fifo/filter",
                              "diag-mismatch:_fifo/filter", "diag-truncated:_fifo/filter",
                              "diag-write:_gate_in/filter", "diag-read:_gate_in/filter",
                              "diag-mismatch:_gate_pm/filter", "diag-truncated:_gate_pm/filter"}) {
        Run run("dma" + std::to_string(n++));
        auto owner = run.owner(fault, 0, 1, Mode::DmaD84), waiter = run.waiter();
        const std::string f = fault;
        const bool armed = f.empty() || f == "vendor-symbols" || f == "vendor-pending" || f == "stop-off-fail" || f == "trace-large";
        require(finish(waiter) == (armed ? 0 : 1) && finish(owner) == 0, "DMA process result " + f);
        auto result = file(run.dir + "/result"), journal = file(run.dir + "/journal");
        require(result.find("mode=capture-dma-d84 schema=d84-vendor-v3") != std::string::npos, "versioned result");
        require(result.find(armed ? "mask=16383" : "mask=0") != std::string::npos, "exact mask/result");
        require(file(run.dir + "/modprobe-marker") == "released\n", "unconditional release");
        require(journal.find("RETAINED p:kprobes/foreign existing_symbol") != std::string::npos, "foreign preserved");
        if (f == "wrong-target") {
            require(result.find("setup_stage=target-d84") != std::string::npos && journal.find("OPEN") == std::string::npos &&
                    journal.find("WRITE") == std::string::npos, "unmatched target resource-free refusal");
        } else if (f.empty() || f == "vendor-symbols" || f == "vendor-pending" || f == "trace-large") {
            for (const auto& needle : {"off=1", "statistics=1", "cleanup=1", "status_saved=1"})
                require(result.find(needle) != std::string::npos, "DMA positive cleanup");
            require(journal.find("_gate_in/enable=0") != std::string::npos &&
                    journal.find("_gate_pm/enable=0") != std::string::npos &&
                    journal.find("REMOVE_INSTANCE") != std::string::npos, "all 14 disabled and removed");
            auto on = journal.find("ON_EFFECT"), off = journal.find("OFF_EFFECT", on);
            require(on < off && off < journal.find("TRACE_READ_AFTER_OFF"), "autonomous OFF before export");
            const auto filter = journal.find("_fifo/filter=comm != \"xyauto-mcu-read\"");
            const auto enable = journal.find("_fifo/enable=1");
            require(filter != std::string::npos && filter < enable && enable < on,
                    "MCU FIFO exclusion checked before event enable and tracing ON");
            require(result.find("fifo_filter_applied=1") != std::string::npos,
                    "DMA receipt names applied filter");
            for (const auto& suffix : {"_gate_in", "_gate_pm"}) {
                auto filtered = journal.find(std::string(suffix) + "/filter=mapbase == 0x4a8c000");
                auto enabled = journal.find(std::string(suffix) + "/enable=1");
                require(filtered != std::string::npos && filtered < enabled && enabled < on,
                        "BT-only gate filter checked before enable and ON");
            }
            require(result.find("gate_filters_applied=2") != std::string::npos,
                    "both gate filters attested");
            if (f == "trace-large") require(result.find("truncated=0") != std::string::npos &&
                    journal.find("bytes=262244") != std::string::npos, "DMA export preserves bytes above 256 KiB");
        } else if (f == "stop-off-fail") {
            require(result.find("off=0") != std::string::npos && result.find("exported=0") != std::string::npos &&
                    result.find("cleanup=0") != std::string::npos, "failed OFF not cleaned/exported");
        } else {
            require(result.find("armed=0") != std::string::npos, "no late or partial ARMED");
            if (f.compare(0, 5, "diag-") == 0) {
                const bool fifo = f.find("_fifo/") != std::string::npos;
                require(result.find(fifo ? "setup_stage=fifo-filter-" : "setup_stage=gate-filter-") != std::string::npos &&
                        (fifo ? result.find("fifo_filter_applied=0") != std::string::npos :
                                result.find("gate_filters_applied=2") == std::string::npos) &&
                        result.find("cleanup=1") != std::string::npos &&
                        journal.find("ON_EFFECT") == std::string::npos,
                        "filter failure refuses capture and cleans owned definitions");
            }
            if (f == "dma-partial-registration") require(journal.find("RETAINED p:kprobes/btd_") != std::string::npos &&
                    result.find("registration_uncertain=1") != std::string::npos, "failed append not adopted");
        }
        printf("PASS DMA process %s\n", f.empty() ? "success" : fault);
    }
    Run legacy("legacy-target-ignored");
    auto owner = legacy.owner("wrong-target"), waiter = legacy.waiter();
    require(finish(waiter) == 0 && finish(owner) == 0 &&
            file(legacy.dir + "/journal").find("TARGET_D84") == std::string::npos, "legacy does not consult target gate");
    require(file(legacy.dir + "/journal").find("/filter") == std::string::npos &&
            file(legacy.dir + "/result").find("fifo_filter") == std::string::npos,
            "legacy filter operations and receipt unchanged");
    puts("PASS legacy mode never invokes new target gate");
    Run old_vendor("legacy-vendor-reject");
    owner = old_vendor.owner("vendor-symbols"); waiter = old_vendor.waiter();
    require(finish(waiter) == 1 && finish(owner) == 0, "legacy vendor registration fails closed");
    const auto old_result = file(old_vendor.dir + "/result");
    require(old_result.find("setup_stage=registration-write") != std::string::npos &&
            old_result.find("setup_errno=22") != std::string::npos &&
            old_result.find("armed=0") != std::string::npos, "actual owner preserves legacy EINVAL registration failure");
    puts("PASS actual legacy owner reports registration-write errno22, releases waiter without ARMED");
}

void masks() {
    unsigned n = 0;
    for (uint64_t mask : {kProbeMask, kDmaD84ProbeMask, kDmaD84ProbeMask - 1, kDmaD84ProbeMask | 0x4000, uint64_t(0xfff), uint64_t(0x17f)}) {
        Run run("mask" + std::to_string(n++));
        auto server = fork(); require(server >= 0, "mask server");
        if (!server) {
            pollfd listener{run.endpoint.listener, POLLIN, 0};
            if (poll(&listener, 1, 2000) != 1) _exit(92);
            int fd = accept4(run.endpoint.listener, nullptr, nullptr, SOCK_CLOEXEC);
            if (fd < 0) _exit(93);
            pollfd input{fd, POLLIN, 0}; poll(&input, 1, 2000);
            Packet p; if (!receive_packet(fd, p)) _exit(94);
            p.kind = 2; p.capture = p.entered + kSecond; p.enabled = p.readback = Clock().now(); p.mask = mask;
            sent(fd, p); close(fd); _exit(0);
        }
        auto waiter = run.waiter();
        require(finish(waiter) == ((mask == kProbeMask || mask == kDmaD84ProbeMask) ? 0 : 1) && finish(server) == 0,
                "exact known masks only");
    }
    puts("PASS real waiter accepts two exact masks and rejects partial/union masks");
}
}
int main(int argc, char** argv) {
    try {
        require(argc == 2, "own scratch required"); root = argv[1];
        vendor_contracts(); contracts(); processes(); masks(); return 0;
    } catch (const std::exception& ex) { fprintf(stderr, "FAIL %s\n", ex.what()); return 1; }
}

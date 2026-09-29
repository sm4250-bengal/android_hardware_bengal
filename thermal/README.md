# Bengal Thermal HAL: read-only real sensors

`BENGAL_REAL_THERMAL=1` replaces the constant-temperature example APEX with this
vendor AIDL V3 service and dedicated read-only policy. Default0 preserves the old
provider. The independent Power knob need not be enabled to deploy this service.

Inventory is discovered once from the real thermal class, sorted and retained
across sensor read failures. The lead's D3 measurement contains17 zones:
PM6125 PMIC plus sixteen SoC TSENS zones. Exact CPU/GPU names map to CPU/GPU;
other known on-die TSENS names map to SOC, not surface/display/ambient or battery
temperature. PMIC and unrecognized names remain UNKNOWN. No skin/battery sensor
or forecast is fabricated. All measured temperature/trip units are milli°C and
are divided by1000. Missing, malformed, implausible or renamed-sensor readings
return NaN in the retained slot, never a canned30°C. Failed initialization
returns EX_ILLEGAL_STATE with a message.

The reporting policy is explicit: kernel passive trip→LIGHT, hot→SEVERE,
critical→SHUTDOWN. Below those thresholds is NONE. This is our conservative
Android reporting mapping, **not** measured UX calibration or proof a cooling
device actually engaged. Where multiple passive trips exist, the lowest is the
LIGHT threshold. Unavailable/intermediate/cold thresholds are NaN. Thresholds
are a static boot snapshot as required by AIDL; this service never writes trips,
cooling states or kernel thermal policy. Kernel hysteresis/mitigation remains
kernel-owned; this reporting mapping uses threshold crossings, not a replica of
the kernel governor. Unsupported future trip types fail inventory initialization
instead of silently claiming no throttling.

Thermal listeners receive initial valid state then severity changes, filtered
by their requested type; cooling listeners receive initial/current state changes.
Polling is1Hz only while listeners exist. Invalid temperature readings do not
emit a false cooling-down callback; callers of the read API receive NaN (severity
has no UNKNOWN enumerator and must not be interpreted without a valid reading).
No service mutex is held over callbacks. Failed callbacks are removed; registry
capacity is bounded. Cancellation may race an already copied in-flight callback.

Cooling-device values come from cur_state/max_state, with stable inventory.
Only known thermal-cpufreq and GPU driver identities receive CPU/GPU types;
others are COMPONENT. Missing state fails the call rather than inventing an
integer sentinel. Optional V3 power statistics remain their unavailable default0;
no power-consumption or cooling-effectiveness estimate is claimed.

Desk coverage lives beside Power. Root still owes Android binary/policy build,
exact `/sys/class/thermal` symlink/canonical path labeling checks (kernel source
registers parentless thermal devices under `/devices/virtual/thermal`), live
17-zone comparison/NaN behavior, callback filtering, VINTF/VTS compatibility and
AVC review. No new thermal mitigation controller is implemented here.

// SPDX-License-Identifier: Apache-2.0
package vendor.xyauto.can;
import vendor.xyauto.can.CanSnapshot;
import vendor.xyauto.can.CanLog;

/** Observation and one narrowly typed status refresh on HUD's single SE5 reader. */
@VintfStability
interface ICanMonitor {
    /** Earliest unread records after cursor; limit 1..32, cursor >=0. */
    CanSnapshot getSnapshot(long afterSequence, int limit);
    /** Transient request, reset at daemon restart. Confirmed ACC-off pauses RX. */
    void setEnabled(boolean enabled);
    /** Newest 100 bounded regular capture files, never arbitrary paths. */
    CanLog[] getLogs();
    /** Up to 65536 bytes; basename from getLogs, nonnegative offset <=8 MiB. */
    byte[] readLog(String name, long offset, int maxBytes);
    /**
     * Queue the selected XinPu/Volkswagen Polo status refresh. The daemon owns
     * the allowlisted wire bytes; callers cannot supply a profile or payload.
     * Returns a process-local request id; the live record stream reports the
     * eventual cancellation/write/result state.
     */
    long requestPoloStatusRefresh();

    /**
     * Bounded catalogue page, limit 1..32 and offset >=0. JSON object with
     * total, offset, items. Items identify CAN-box maker, vehicle brand/model,
     * OEM selection id, implementation status and adapter. Catalogue presence
     * is not a claim that the vehicle's protocol has been implemented.
     */
    String getVehicleCatalogue(int offset, int limit);
    /** JSON selection state: selected item or null, revision, error, supported. */
    String getVehicleSelection();
    /**
     * Persist an exact catalogue selection id, or empty to clear. No arbitrary
     * profile path or UART payload is accepted. Cancels the previous vehicle's
     * pending requests. Saving alone never transmits a vehicle command.
     */
    void selectVehicle(String selectionId);
    /** Queue only the selected adapter's implemented bounded status requests. */
    long requestVehicleStatusRefresh();

    /**
     * Bounded JSON controls for the exact selected profile, including observed
     * values and current write availability. Missing values are unknown, not
     * defaults. Implemented controls are not a claim of physical verification.
     */
    String getVehicleCapabilities();
    /**
     * Queue a generated profile control by ID and bounded integer value.
     * Both daemon session and selection revision must match the capabilities
     * reply. No caller-supplied wire bytes or profile path are accepted. The
     * returned request ID reports queue admission, not vehicle acceptance;
     * actual host write/cancellation events appear in getSnapshot records.
     */
    long setVehicleControl(String sessionId, long selectionRevision, String controlId, int value);
}

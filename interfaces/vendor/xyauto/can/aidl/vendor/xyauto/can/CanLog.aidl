// SPDX-License-Identifier: Apache-2.0
package vendor.xyauto.can;
@VintfStability
parcelable CanLog {
    String name;
    /** Snapshot size, <=8 MiB. Active log may append after this observation. */
    long bytes;
}

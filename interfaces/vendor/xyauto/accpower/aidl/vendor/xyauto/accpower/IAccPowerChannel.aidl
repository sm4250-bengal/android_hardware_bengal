// SPDX-License-Identifier: Apache-2.0
package vendor.xyauto.accpower;

import android.os.ParcelFileDescriptor;

/**
 * Offers one already-connected, unnamed AF_UNIX SOCK_SEQPACKET endpoint.
 *
 * The own-UID power app creates the pair. HUD authenticates the transaction
 * UID against kernel peer credentials and the dedicated app peer context,
 * duplicates a validated FD into its bounded worker queue, or closes it.
 * A successful Binder submission is not admission or permission to sleep:
 * the BACP SNAPSHOT/START handshake on this connection establishes admission.
 * No descriptor transfer is allowed inside the subsequent BACP records.
 */
@VintfStability
interface IAccPowerChannel {
    oneway void offerChannel(in ParcelFileDescriptor endpoint);
}

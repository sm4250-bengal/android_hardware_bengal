// SPDX-License-Identifier: Apache-2.0
//! Stable AIDL transfers a connected app-owned endpoint; no vendor listener.

use super::Channel;
use binder::{BinderFeatures, Interface, ParcelFileDescriptor, ThreadState};
use std::sync::Arc;
use vendor_xyauto_accpower::aidl::vendor::xyauto::accpower::IAccPowerChannel::{
    BnAccPowerChannel, IAccPowerChannel,
};

pub(crate) const ENABLE_PROPERTY: &str = "ro.vendor.bengal.acc_power_channel";
const INSTANCE: &str = "vendor.xyauto.accpower.IAccPowerChannel/default";

pub(crate) fn register(channel: Arc<Channel>) -> binder::Result<()> {
    let service = BnAccPowerChannel::new_binder(Bootstrap { channel }, BinderFeatures::default());
    binder::add_service(INSTANCE, service.as_binder())?;
    Ok(())
}

struct Bootstrap {
    channel: Arc<Channel>,
}
impl Interface for Bootstrap {}
impl IAccPowerChannel for Bootstrap {
    fn offerChannel(&self, endpoint: &ParcelFileDescriptor) -> binder::Result<()> {
        // Capture inside this actual incoming transaction. No PID lookup,
        // requesting-SID feature or self-reported principal is involved.
        if ThreadState::is_handling_transaction() {
            let caller = ThreadState::get_calling_uid();
            // AIDL is oneway: even success here is not acceptance. Only the
            // authenticated BACP SNAPSHOT/START proves channel admission.
            self.channel.offer(endpoint.as_ref(), caller);
        }
        Ok(())
    }
}

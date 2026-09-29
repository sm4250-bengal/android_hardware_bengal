// SPDX-License-Identifier: Apache-2.0
//! Bounded CAN API. Hardware ownership remains in the CAN runtime.
use crate::can::{monitor, CAPTURE_DIR};
use binder::{BinderFeatures, ExceptionCode, Interface, Status};
use std::path::Path;
use vendor_xyauto_can::aidl::vendor::xyauto::can::{
    ICanMonitor::{BnCanMonitor, ICanMonitor}, CanSnapshot::CanSnapshot, CanLog::CanLog,
};

/// V4 service name, paired with VINTF and a dedicated app SELinux domain.
pub const SERVICE_NAME: &str = "vendor.xyauto.can.ICanMonitor/default";

/// Register observation without opening any port.
pub fn register(monitor: monitor::SharedMonitor) -> binder::Result<()> {
    let service = BnCanMonitor::new_binder(CanService(monitor), BinderFeatures::default());
    Ok(binder::add_service(SERVICE_NAME, service.as_binder())?)
}

struct CanService(monitor::SharedMonitor);
impl Interface for CanService {}

fn error(code: ExceptionCode) -> Status { Status::new_exception(code, None) }
fn io_error(e: std::io::Error) -> Status {
    Status::new_service_specific_error(e.raw_os_error().unwrap_or(5), None)
}

impl ICanMonitor for CanService {
    fn getVehicleCapabilities(&self) -> binder::Result<String> {
        let m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        Ok(m.vehicle_capabilities_json())
    }

    fn setVehicleControl(&self, session: &str, revision: i64, id: &str, value: i32) -> binder::Result<i64> {
        if revision < 0 || session.len() > 256 || id.is_empty() || id.len() > 128 {
            return Err(error(ExceptionCode::ILLEGAL_ARGUMENT));
        }
        let mut m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        m.set_vehicle_control(session, revision as u64, id, value)
            .map(|request| request as i64)
            .map_err(|reason| Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(reason)))
    }

    fn getVehicleCatalogue(&self, offset: i32, limit: i32) -> binder::Result<String> {
        let m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        m.vehicle.catalogue_json(offset, limit)
            .map_err(|reason| Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(reason)))
    }

    fn getVehicleSelection(&self) -> binder::Result<String> {
        let m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        Ok(m.vehicle.state_json())
    }

    fn selectVehicle(&self, id: &str) -> binder::Result<()> {
        let mut m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        m.select_vehicle(id)
            .map_err(|reason| Status::new_exception_str(ExceptionCode::ILLEGAL_ARGUMENT, Some(reason)))
    }

    fn requestVehicleStatusRefresh(&self) -> binder::Result<i64> {
        self.requestPoloStatusRefresh()
    }

    fn getSnapshot(&self, after: i64, limit: i32) -> binder::Result<CanSnapshot> {
        let m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        let (gap, records) = m.batch(after, limit).map_err(|_| error(ExceptionCode::ILLEGAL_ARGUMENT))?;
        Ok(CanSnapshot {
            sessionId: m.session.clone(), enabled: m.enabled,
            state: m.state.clone(), detail: m.detail.clone(),
            bytesReceived: m.stats.bytes as i64, validFrames: m.stats.frames_ok as i64,
            badFrames: m.stats.frames_bad as i64, discardedBytes: m.stats.discarded as i64,
            lastRxAgeMs: m.last_rx_age_ms(), oldestSequence: m.oldest() as i64,
            latestSequence: m.latest() as i64, gap,
            captureName: m.capture_name.clone(), captureState: m.capture_state.clone(),
            records: records.into_iter().map(|r| r.json).collect(),
        })
    }

    fn setEnabled(&self, enabled: bool) -> binder::Result<()> {
        let mut m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        if !enabled {
            // Cancellation is a queue operation only; the reader thread owns
            // the tty and will observe the disabled state before any write.
            m.reset_request_session("disabled");
        }
        m.requested = Some(enabled);
        // This is a request, not a fabricated successful UART open.
        m.state(enabled, if enabled { "waiting" } else { "stopping" },
            if enabled { "Start requested; checking SE5 and ACC" } else { "Stop requested; waiting for RX thread" });
        Ok(())
    }

    fn requestPoloStatusRefresh(&self) -> binder::Result<i64> {
        let mut m = self.0.lock().map_err(|_| error(ExceptionCode::ILLEGAL_STATE))?;
        if m.vehicle.active_profile() != Some("xinpu-vw-pq") {
            return Err(Status::new_exception_str(ExceptionCode::ILLEGAL_STATE,
                Some("Select a vehicle with an implemented status adapter first")));
        }
        // This only queues a single typed request. It never opens a port or
        // performs UART I/O on the Binder thread. The reader records the
        // eventual write/cancellation/result in the bounded event stream.
        m.request_polo_status_refresh()
            .map(|id| id as i64)
            .map_err(|reason| Status::new_exception_str(ExceptionCode::ILLEGAL_STATE, Some(reason)))
    }

    fn getLogs(&self) -> binder::Result<Vec<CanLog>> {
        Ok(monitor::logs(Path::new(CAPTURE_DIR)).map_err(io_error)?
            .into_iter().map(|(name, bytes)| CanLog { name, bytes: bytes as i64 }).collect())
    }

    fn readLog(&self, name: &str, offset: i64, max_bytes: i32) -> binder::Result<Vec<u8>> {
        monitor::read_log(Path::new(CAPTURE_DIR), name, offset, max_bytes).map_err(io_error)
    }
}

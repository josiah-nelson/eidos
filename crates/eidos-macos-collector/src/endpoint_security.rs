use crate::protocol::EndpointEventCounts;
use eidos_observe::EndpointSecurityState;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct Counters {
    opens: AtomicU64,
    closes: AtomicU64,
    mappings: AtomicU64,
    executions: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> EndpointEventCounts {
        EndpointEventCounts {
            opens: self.opens.load(Ordering::Relaxed),
            closes: self.closes.load(Ordering::Relaxed),
            mappings: self.mappings.load(Ordering::Relaxed),
            executions: self.executions.load(Ordering::Relaxed),
        }
    }
}

pub struct Lane {
    #[cfg(feature = "endpoint-security")]
    native: *mut NativeLane,
    pub counters: Arc<Counters>,
}

#[cfg(feature = "endpoint-security")]
#[repr(C)]
struct NativeLane {
    _private: [u8; 0],
}

#[cfg(feature = "endpoint-security")]
extern "C" {
    fn eidos_es_create(
        output: *mut *mut NativeLane,
        callback: extern "C" fn(*mut libc::c_void, u32),
        context: *mut libc::c_void,
    ) -> libc::c_int;
    fn eidos_es_destroy(lane: *mut NativeLane);
}

#[cfg(feature = "endpoint-security")]
extern "C" fn count_event(context: *mut libc::c_void, kind: u32) {
    let counters = unsafe { &*(context as *const Counters) };
    match kind {
        1 => counters.opens.fetch_add(1, Ordering::Relaxed),
        2 => counters.closes.fetch_add(1, Ordering::Relaxed),
        3 => counters.mappings.fetch_add(1, Ordering::Relaxed),
        4 => counters.executions.fetch_add(1, Ordering::Relaxed),
        _ => 0,
    };
}

impl Lane {
    pub fn start(enabled: bool) -> (Self, EndpointSecurityState) {
        let counters = Arc::new(Counters::default());
        if !enabled {
            return (
                Self {
                    #[cfg(feature = "endpoint-security")]
                    native: std::ptr::null_mut(),
                    counters,
                },
                EndpointSecurityState::Off,
            );
        }
        #[cfg(feature = "endpoint-security")]
        {
            let mut native = std::ptr::null_mut();
            let code = unsafe {
                eidos_es_create(
                    &mut native,
                    count_event,
                    Arc::as_ptr(&counters) as *mut libc::c_void,
                )
            };
            let state = match code {
                0 => EndpointSecurityState::Available,
                1 => EndpointSecurityState::NotEntitled,
                2 => EndpointSecurityState::NotPermitted,
                3 => EndpointSecurityState::NotPrivileged,
                4 => EndpointSecurityState::TooManyClients,
                _ => EndpointSecurityState::InternalError,
            };
            (Self { native, counters }, state)
        }
        #[cfg(not(feature = "endpoint-security"))]
        {
            (Self { counters }, EndpointSecurityState::NotEntitled)
        }
    }
}

#[cfg(feature = "endpoint-security")]
impl Drop for Lane {
    fn drop(&mut self) {
        if !self.native.is_null() {
            unsafe { eidos_es_destroy(self.native) };
        }
    }
}

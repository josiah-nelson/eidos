//! Lane supervision. Each lane runs on its own threads against the shared
//! state and checks the shutdown flag and its configuration switch itself.

use crate::daemon::Shared;
use crate::protocol::Response;
use std::sync::Arc;
use std::thread::JoinHandle;

pub fn start(shared: Arc<Shared>) -> Vec<JoinHandle<()>> {
    vec![crate::usn_lane::start(shared)]
}

pub fn probe_now(_shared: &Arc<Shared>, _volume: Option<&str>) -> Response {
    Response::Error {
        message: "the enumeration probe is not available in this build".into(),
    }
}

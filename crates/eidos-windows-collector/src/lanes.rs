//! Lane supervision. Each lane runs on its own threads against the shared
//! state and checks the shutdown flag and its configuration switch itself.

use crate::daemon::Shared;
use crate::protocol::Response;
use std::sync::Arc;
use std::thread::JoinHandle;

pub fn start(shared: Arc<Shared>) -> Vec<JoinHandle<()>> {
    let (content_tx, content_rx) = std::sync::mpsc::sync_channel(crate::content_probe::QUEUE_DEPTH);
    *shared.content_tx.lock().unwrap() = Some(content_tx);
    vec![
        crate::content_probe::start(shared.clone(), content_rx),
        crate::usn_lane::start(shared.clone()),
        crate::access_lane::start(shared.clone()),
        crate::enumeration_probe::start(shared),
    ]
}

pub fn probe_now(shared: &Arc<Shared>, volume: Option<&str>) -> Response {
    crate::enumeration_probe::run_detached(shared.clone(), volume.map(str::to_string));
    Response::Accepted
}

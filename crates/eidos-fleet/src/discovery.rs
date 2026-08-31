//! Link-local master advertisement and discovery via DNS-SD/mDNS.

use crate::config::FleetConfig;
use crate::identity::NodeIdentity;
use crate::status::DiscoveredMaster;
use eidos_catalog::fleet::NodeId;
use eidos_domain::UnixNanos;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::{Mutex, RwLock};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

pub const SERVICE_TYPE: &str = "_eidos-fleet._tcp.local.";
const REFRESH: Duration = Duration::from_secs(2);
const STALE_AFTER: Duration = Duration::from_secs(3 * 60);

#[derive(Default)]
pub struct DiscoveryState {
    masters: Mutex<HashMap<String, DiscoveredMaster>>,
    error: Mutex<Option<String>>,
}

impl DiscoveryState {
    pub fn masters(&self) -> Vec<DiscoveredMaster> {
        let mut masters: Vec<_> = self.masters.lock().values().cloned().collect();
        masters.sort_by(|a, b| a.name.cmp(&b.name).then(a.node_id.cmp(&b.node_id)));
        masters
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().clone()
    }

    fn fail(&self, error: impl ToString) {
        *self.error.lock() = Some(error.to_string());
    }
}

pub async fn run(
    state: Arc<DiscoveryState>,
    identity: Arc<NodeIdentity>,
    config: Arc<RwLock<FleetConfig>>,
    listening: Arc<Mutex<Option<String>>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let daemon = match ServiceDaemon::new() {
        Ok(daemon) => daemon,
        Err(error) => {
            state.fail(format!("local discovery could not start: {error}"));
            return;
        }
    };
    let receiver = match daemon.browse(SERVICE_TYPE) {
        Ok(receiver) => receiver,
        Err(error) => {
            state.fail(format!("local discovery browse failed: {error}"));
            let _ = daemon.shutdown();
            return;
        }
    };
    let mut registered: Option<(String, u16)> = None;
    let mut ticker = tokio::time::interval(REFRESH);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                refresh_registration(&state, &daemon, &identity, &config, &listening, &mut registered);
                let cutoff = UnixNanos::now().0 - STALE_AFTER.as_nanos().min(i64::MAX as u128) as i64;
                state.masters.lock().retain(|_, master| master.last_seen_at.0 >= cutoff);
            }
            event = receiver.recv_async() => match event {
                Ok(ServiceEvent::ServiceResolved(info)) => {
                    if let Some(master) = resolved_master(&info, identity.node_id) {
                        state.masters.lock().insert(info.fullname.clone(), master);
                        *state.error.lock() = None;
                    }
                }
                Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                    state.masters.lock().remove(&fullname);
                }
                Ok(_) => {}
                Err(error) => {
                    state.fail(format!("local discovery stopped: {error}"));
                    break;
                }
            },
            _ = shutdown.changed() => break,
        }
    }
    if let Some((fullname, _)) = registered {
        let _ = daemon.unregister(&fullname);
    }
    let _ = daemon.stop_browse(SERVICE_TYPE);
    let _ = daemon.shutdown();
}

fn refresh_registration(
    state: &DiscoveryState,
    daemon: &ServiceDaemon,
    identity: &NodeIdentity,
    config: &RwLock<FleetConfig>,
    listening: &Mutex<Option<String>>,
    registered: &mut Option<(String, u16)>,
) {
    let should_advertise = config.read().central;
    let port = listening
        .lock()
        .as_deref()
        .and_then(|address| address.parse::<std::net::SocketAddr>().ok())
        .map(|address| address.port());
    if !should_advertise || port.is_none() {
        if let Some((fullname, _)) = registered.take() {
            let _ = daemon.unregister(&fullname);
        }
        return;
    }
    let port = port.expect("checked above");
    let instance = format!(
        "{}-{}",
        dns_label(&identity.name),
        &identity.node_id.to_hex()[..8]
    );
    let hostname = format!("{}.local.", dns_label(&identity.name));
    let node_id = identity.node_id.to_hex();
    let fingerprint = identity.fingerprint_hex();
    let properties = [
        ("name", identity.name.as_str()),
        ("node", node_id.as_str()),
        ("fingerprint", fingerprint.as_str()),
        ("protocol", "1"),
    ];
    let service = match ServiceInfo::new(
        SERVICE_TYPE,
        &instance,
        &hostname,
        "",
        port,
        &properties[..],
    ) {
        Ok(service) => service.enable_addr_auto(),
        Err(error) => {
            state.fail(format!("local master advertisement is invalid: {error}"));
            return;
        }
    };
    let fullname = service.get_fullname().to_string();
    if registered
        .as_ref()
        .is_some_and(|(current, current_port)| current == &fullname && *current_port == port)
    {
        return;
    }
    if let Some((old, _)) = registered.take() {
        let _ = daemon.unregister(&old);
    }
    match daemon.register(service) {
        Ok(()) => {
            *registered = Some((fullname, port));
            *state.error.lock() = None;
        }
        Err(error) => state.fail(format!("local master advertisement failed: {error}")),
    }
}

fn resolved_master(
    info: &mdns_sd::ResolvedService,
    local_node: NodeId,
) -> Option<DiscoveredMaster> {
    let node_id = NodeId::parse_hex(info.get_property_val_str("node")?)?;
    if node_id == local_node {
        return None;
    }
    let fingerprint = info.get_property_val_str("fingerprint")?.to_string();
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut endpoints: Vec<_> = info
        .get_addresses()
        .iter()
        .map(|address| {
            let address = address.to_string();
            if address.contains(':') {
                format!("[{address}]:{}", info.get_port())
            } else {
                format!("{address}:{}", info.get_port())
            }
        })
        .collect();
    endpoints.sort();
    endpoints.dedup();
    if endpoints.is_empty() {
        return None;
    }
    Some(DiscoveredMaster {
        node_id,
        name: info
            .get_property_val_str("name")
            .unwrap_or(info.get_hostname())
            .to_string(),
        fingerprint,
        endpoints,
        last_seen_at: UnixNanos::now(),
    })
}

fn dns_label(value: &str) -> String {
    let label: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let label = label.trim_matches('-');
    if label.is_empty() {
        "eidos-node".into()
    } else {
        label.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_labels_are_bounded_and_never_empty() {
        assert_eq!(dns_label("My Workstation!"), "my-workstation");
        assert_eq!(dns_label("///"), "eidos-node");
        assert!(dns_label(&"a".repeat(100)).len() <= 48);
    }
}

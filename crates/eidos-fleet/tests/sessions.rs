//! Live fleet sessions over loopback TLS (sprint track C gate): enrollment
//! by invitation, replication that converges, sessions initiated from
//! either side that resume the same cursor, simultaneous dials that leave
//! exactly one session, and peers that fail closed before any payload.

use eidos_catalog::fleet::PeerRole;
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::{SourceId, SourceKind};
use eidos_fleet::enroll::{create_invite, enroll};
use eidos_fleet::status::Direction;
use eidos_fleet::wire::{self, Message};
use eidos_fleet::{Fleet, FleetConfig, NodeIdentity};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

const SETTLE: Duration = Duration::from_secs(90);

struct Host {
    dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    source: Option<SourceId>,
    fleet: Option<Arc<Fleet>>,
}

impl Host {
    fn new(name: &str, central: bool, listen: bool, with_source: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
        let host = catalog.ensure_host(name, "windows").unwrap();
        let source = if with_source {
            std::fs::create_dir_all(root.join("a/b")).unwrap();
            std::fs::write(root.join("a/one.txt"), vec![b'1'; 100]).unwrap();
            std::fs::write(root.join("a/b/two.txt"), vec![b'2'; 200]).unwrap();
            let source = catalog
                .add_source(&NewSource {
                    host_id: host,
                    name: "docs".into(),
                    kind: SourceKind::WindowsLocal,
                    root_path: root.display().to_string(),
                    aliases: vec![],
                })
                .unwrap();
            Some(source)
        } else {
            None
        };
        // A stable identity named like the host, so roster names are readable.
        NodeIdentity::load_or_create(dir.path(), name).unwrap();
        FleetConfig {
            central,
            listen: listen.then(|| "127.0.0.1:0".to_string()),
            ..FleetConfig::default()
        }
        .store(dir.path())
        .unwrap();
        let h = Host {
            dir,
            root,
            catalog,
            source,
            fleet: None,
        };
        if with_source {
            h.scan();
            h.scan();
        }
        h
    }

    fn scan(&self) {
        let lister = eidos_scanner::default_lister();
        run_scan(
            &self.catalog,
            self.source.unwrap(),
            lister.as_ref(),
            &RunScanOptions::default(),
        )
        .unwrap();
    }

    fn start(&mut self) -> Arc<Fleet> {
        let fleet = Fleet::start(self.catalog.clone(), self.dir.path()).unwrap();
        self.fleet = Some(fleet.clone());
        fleet
    }

    fn stop(&mut self) {
        if let Some(f) = self.fleet.take() {
            f.shutdown();
        }
    }

    fn fleet(&self) -> &Arc<Fleet> {
        self.fleet.as_ref().unwrap()
    }

    async fn listening(&self) -> String {
        wait_for(SETTLE, || self.fleet().status().listening)
            .await
            .expect("listener bound")
    }

    fn image(&self, source: SourceId) -> BTreeSet<(String, u64)> {
        let mut out = BTreeSet::new();
        self.catalog
            .for_each_projection_row(source, |row| {
                // Compare paths relative to the root: the replica keeps the
                // origin's root path verbatim, so absolute paths match too.
                out.insert((row.path.clone(), row.size));
                Ok(())
            })
            .unwrap();
        out
    }

    fn replica_source(&self) -> Option<SourceId> {
        self.catalog
            .replica_sources()
            .unwrap()
            .first()
            .map(|r| r.source_id)
    }
}

async fn wait_for<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let started = Instant::now();
    loop {
        if let Some(v) = probe() {
            return Some(v);
        }
        if started.elapsed() > timeout {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Wait until the central's replica of `node`'s source equals the node's
/// image, and return the replica source id.
async fn converged(node: &Host, central: &Host) -> SourceId {
    let want = node.image(node.source.unwrap());
    wait_for(SETTLE, || {
        let source = central.replica_source()?;
        (central.image(source) == want).then_some(source)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "did not converge; central status: {:#?}",
            central.fleet().status()
        )
    })
}

async fn enroll_node(node: &Host, central: &Host) {
    let endpoint = central.listening().await;
    let invite = create_invite(
        &central.catalog,
        central.fleet().identity(),
        &central.fleet().config(),
        &endpoint,
        None,
    )
    .unwrap();
    let outcome = enroll(
        &node.catalog,
        node.fleet().identity(),
        &invite,
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(outcome.central, central.fleet().identity().node_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_by_invitation_then_replication_converges_and_follows_changes() {
    let mut central = Host::new("central", true, true, false);
    let mut node = Host::new("node-a", false, false, true);
    central.start();
    node.start();
    enroll_node(&node, &central).await;

    // The invitation is single use.
    let peers = central.catalog.fleet_peers().unwrap();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].role, PeerRole::Node);
    assert_eq!(peers[0].node_id, node.fleet().identity().node_id);
    let mine = node.catalog.fleet_peers().unwrap();
    assert_eq!(mine[0].role, PeerRole::Central);
    assert!(mine[0].endpoint.is_some());

    let replica = converged(&node, &central).await;
    let status = node.fleet().status();
    assert!(status.enrolled && status.sync_enabled);
    assert_eq!(status.sessions.len(), 1);
    assert_eq!(status.sessions[0].direction, Direction::Outbound);
    let cstatus = central.fleet().status();
    assert_eq!(cstatus.sessions[0].direction, Direction::Inbound);
    assert_eq!(cstatus.replica_sources.len(), 1);
    assert!(cstatus.replica_sources[0].connected);
    assert!(cstatus.counters.batches_applied >= 1);
    assert_eq!(cstatus.counters.fences, 0);

    // Local work keeps flowing while enrolled: a change and a deletion.
    std::fs::write(node.root.join("a/one.txt"), vec![b'x'; 1_000]).unwrap();
    std::fs::remove_file(node.root.join("a/b/two.txt")).unwrap();
    node.scan();
    assert_eq!(converged(&node, &central).await, replica);
    let names: BTreeSet<String> = central
        .catalog
        .list_sources()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains("node-a/docs"), "{names:?}");
    node.stop();
    central.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn either_side_may_initiate_and_the_cursor_survives_a_direction_change() {
    let mut central = Host::new("central", true, true, false);
    let mut node = Host::new("node-b", false, true, true);
    central.start();
    node.start();
    enroll_node(&node, &central).await;
    let replica = converged(&node, &central).await;
    let batches_before = central.fleet().status().counters.batches_applied;
    let full_resyncs_before = central.fleet().status().counters.full_resyncs;

    // Give the central the node's endpoint and take the central's away
    // from the node: from now on only the central dials.
    let node_endpoint = node.listening().await;
    let node_id = node.fleet().identity().node_id;
    let central_id = central.fleet().identity().node_id;
    central
        .catalog
        .fleet_set_peer_endpoint(node_id, Some(&node_endpoint))
        .unwrap();
    node.catalog.fleet_set_peer_endpoint(central_id, None).unwrap();
    // Drop the running session by restarting the node's fleet runtime.
    node.stop();
    wait_for(SETTLE, || (!central.fleet().is_connected(node_id)).then_some(()))
        .await
        .expect("session dropped");
    node.start();
    let listen = node.listening().await;
    central
        .catalog
        .fleet_set_peer_endpoint(node_id, Some(&listen))
        .unwrap();

    wait_for(SETTLE, || {
        let s = node.fleet().status();
        (s.sessions.len() == 1 && s.sessions[0].direction == Direction::Inbound).then_some(())
    })
    .await
    .expect("central-initiated session");
    // New work converges over the reversed session without a resync.
    std::fs::write(node.root.join("a/three.txt"), vec![b'3'; 300]).unwrap();
    node.scan();
    assert_eq!(converged(&node, &central).await, replica);
    let after = central.fleet().status();
    assert_eq!(after.counters.full_resyncs, full_resyncs_before);
    assert!(after.counters.batches_applied > batches_before);
    assert_eq!(central.catalog.replica_sources().unwrap().len(), 1, "no duplicate source registration");
    node.stop();
    central.stop();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_initiation_leaves_exactly_one_session_on_both_sides() {
    let mut central = Host::new("central", true, true, false);
    let mut node = Host::new("node-c", false, true, true);
    central.start();
    node.start();
    enroll_node(&node, &central).await;
    let node_endpoint = node.listening().await;
    let node_id = node.fleet().identity().node_id;
    central
        .catalog
        .fleet_set_peer_endpoint(node_id, Some(&node_endpoint))
        .unwrap();
    // Both dial each other from now on; let several dial rounds pass.
    let replica = converged(&node, &central).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let ns = node.fleet().status();
    let cs = central.fleet().status();
    assert_eq!(ns.sessions.len(), 1, "{ns:#?}");
    assert_eq!(cs.sessions.len(), 1, "{cs:#?}");
    assert_ne!(ns.sessions[0].direction, cs.sessions[0].direction);
    assert!(
        cs.counters.duplicate_sessions_closed + ns.counters.duplicate_sessions_closed >= 1,
        "a duplicate was resolved: {:?} {:?}",
        cs.counters,
        ns.counters
    );
    // Progress did not reset.
    std::fs::write(node.root.join("a/four.txt"), vec![b'4'; 40]).unwrap();
    node.scan();
    assert_eq!(converged(&node, &central).await, replica);
    assert_eq!(central.fleet().status().counters.full_resyncs, 0);
    node.stop();
    central.stop();
}

/// Open a raw TLS connection to the central as `identity` and send frames.
async fn raw_connect(
    identity: &NodeIdentity,
    central: &Host,
) -> tokio_rustls_stream::Stream {
    let endpoint = central.listening().await;
    let (stream, _) = eidos_fleet::tls::connect(
        identity,
        &endpoint,
        central.fleet().identity().fingerprint,
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    stream
}

mod tokio_rustls_stream {
    pub type Stream = eidos_fleet::tls::ClientStream;
}

async fn send(stream: &mut tokio_rustls_stream::Stream, msg: &Message) {
    let bytes = wire::encode(msg).unwrap();
    wire::write_frame(stream, &bytes, wire::DEFAULT_MAX_FRAME_BYTES)
        .await
        .unwrap();
}

async fn recv(stream: &mut tokio_rustls_stream::Stream) -> Result<Message, wire::FrameError> {
    tokio::time::timeout(
        Duration::from_secs(10),
        wire::read_frame(stream, wire::DEFAULT_MAX_FRAME_BYTES),
    )
    .await
    .expect("reply in time")
    .map(|(m, _)| m)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_peers_bad_invitations_and_foreign_versions_fail_closed_before_any_payload() {
    let mut central = Host::new("central", true, true, false);
    central.start();
    let stranger_dir = tempfile::tempdir().unwrap();
    let stranger = NodeIdentity::load_or_create(stranger_dir.path(), "stranger").unwrap();

    // A stranger offering a source is refused with no inventory disclosed.
    let mut s = raw_connect(&stranger, &central).await;
    send(
        &mut s,
        &Message::Hello(wire::Hello {
            node_id: stranger.node_id,
            name: "stranger".into(),
            platform: "windows".into(),
            role: wire::Role::Node,
            nonce: 1,
            versions: vec![wire::PROTOCOL_VERSION],
            features: vec![],
            max_frame_bytes: 1 << 20,
            credit_bytes: 1 << 20,
        }),
    )
    .await;
    match recv(&mut s).await.unwrap() {
        Message::Goodbye { reason } => assert_eq!(reason, "unknown peer"),
        other => panic!("{other:?}"),
    }

    // A bad invitation secret is refused and nothing is enrolled.
    let mut s = raw_connect(&stranger, &central).await;
    send(
        &mut s,
        &Message::Enroll {
            secret: "00".repeat(32),
            name: "stranger".into(),
            platform: "windows".into(),
        },
    )
    .await;
    assert!(matches!(
        recv(&mut s).await.unwrap(),
        Message::EnrollRejected { .. }
    ));
    assert!(central.catalog.fleet_peers().unwrap().is_empty());

    // An enrolled node speaking a version we do not have fails closed.
    let mut node = Host::new("node-d", false, false, false);
    node.start();
    enroll_node(&node, &central).await;
    let mut s = raw_connect(node.fleet().identity(), &central).await;
    send(
        &mut s,
        &Message::Hello(wire::Hello {
            node_id: node.fleet().identity().node_id,
            name: "node-d".into(),
            platform: "windows".into(),
            role: wire::Role::Node,
            nonce: 2,
            versions: vec![99],
            features: vec![],
            max_frame_bytes: 1 << 20,
            credit_bytes: 1 << 20,
        }),
    )
    .await;
    match recv(&mut s).await.unwrap() {
        Message::Goodbye { reason } => assert!(reason.contains("version"), "{reason}"),
        other => panic!("{other:?}"),
    }

    // A malformed frame from an enrolled node ends that connection only.
    let mut s = raw_connect(node.fleet().identity(), &central).await;
    send(
        &mut s,
        &Message::Hello(wire::Hello {
            node_id: node.fleet().identity().node_id,
            name: "node-d".into(),
            platform: "windows".into(),
            role: wire::Role::Node,
            nonce: 3,
            versions: vec![wire::PROTOCOL_VERSION],
            features: vec![],
            max_frame_bytes: 1 << 20,
            credit_bytes: 1 << 20,
        }),
    )
    .await;
    assert!(matches!(recv(&mut s).await.unwrap(), Message::Hello(_)));
    s.write_all(&5u32.to_be_bytes()).await.unwrap();
    s.write_all(b"{{{{{").await.unwrap();
    s.flush().await.unwrap();
    match recv(&mut s).await {
        Ok(Message::Goodbye { .. }) | Err(_) => {}
        other => panic!("{other:?}"),
    }
    let counters = central.fleet().status().counters;
    assert!(counters.frames_malformed >= 1);
    assert!(counters.connections_refused_unknown_peer >= 2);
    assert!(counters.connections_refused_version >= 1);
    // The central still serves: a fresh, well-formed session works.
    let mut s = raw_connect(node.fleet().identity(), &central).await;
    send(
        &mut s,
        &Message::Hello(wire::Hello {
            node_id: node.fleet().identity().node_id,
            name: "node-d".into(),
            platform: "windows".into(),
            role: wire::Role::Node,
            nonce: 4,
            versions: vec![wire::PROTOCOL_VERSION],
            features: vec![],
            max_frame_bytes: 1 << 20,
            credit_bytes: 1 << 20,
        }),
    )
    .await;
    assert!(matches!(recv(&mut s).await.unwrap(), Message::Hello(_)));
    node.stop();
    central.stop();
}

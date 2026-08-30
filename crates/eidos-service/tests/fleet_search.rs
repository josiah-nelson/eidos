//! Central search over replicated sources (sprint track D gate): rows
//! applied by the fleet runtime reach the ordinary catalog index through
//! the existing follower, hits carry the origin host and source, per-source
//! completeness reports freshness and content incompleteness truthfully, and
//! `host:` narrows to one node.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::NewSource;
use eidos_domain::{CoverageKind, SearchRequest, SourceId, SourceKind};
use eidos_fleet::enroll::{create_invite, enroll};
use eidos_fleet::{Fleet, FleetConfig, NodeIdentity};
use eidos_search::exec::{search_with_content, ExecOptions};
use eidos_service::state::AppState;
use eidos_service::ServiceConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower::ServiceExt;

struct Host {
    _dir: tempfile::TempDir,
    data: PathBuf,
    root: PathBuf,
    state: Arc<AppState>,
    fleet: Option<Arc<Fleet>>,
    source: Option<SourceId>,
}

fn open_state(data: &Path) -> Arc<AppState> {
    let cfg = ServiceConfig {
        data_dir: data.to_path_buf(),
        scan_threads: 2,
        auto_reconcile: false,
        content: false,
        content_workers: 1,
        fleet: false,
        ..Default::default()
    };
    Arc::new(AppState::open(&cfg).unwrap())
}

impl Host {
    fn new(name: &str, central: bool, with_source: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let root = dir.path().join("root");
        std::fs::create_dir_all(&data).unwrap();
        NodeIdentity::load_or_create(&data, name).unwrap();
        FleetConfig {
            central,
            listen: central.then(|| "127.0.0.1:0".to_string()),
            ..FleetConfig::default()
        }
        .store(&data)
        .unwrap();
        let state = open_state(&data);
        let source = if with_source {
            std::fs::create_dir_all(root.join("reports/q3")).unwrap();
            std::fs::write(root.join("reports/summary.txt"), vec![b's'; 120]).unwrap();
            std::fs::write(root.join("reports/q3/figures.csv"), vec![b'f'; 340]).unwrap();
            let sid = state
                .catalog
                .add_source(&NewSource {
                    host_id: state.host_id,
                    name: "reports".into(),
                    kind: SourceKind::WindowsLocal,
                    root_path: root.display().to_string(),
                    aliases: vec![],
                })
                .unwrap();
            for _ in 0..2 {
                run_scan(
                    &state.catalog,
                    sid,
                    state.lister.as_ref(),
                    &RunScanOptions::default(),
                )
                .unwrap();
            }
            eidos_service::follower::follow_once(&state).unwrap();
            Some(sid)
        } else {
            None
        };
        Host {
            _dir: dir,
            data,
            root,
            state,
            fleet: None,
            source,
        }
    }

    fn start_fleet(&mut self) {
        let fleet = Fleet::start(self.state.catalog.clone(), &self.data).unwrap();
        *self.state.fleet.lock() = Some(fleet.clone());
        self.fleet = Some(fleet);
    }

    fn fleet(&self) -> &Arc<Fleet> {
        self.fleet.as_ref().unwrap()
    }

    fn search(&self, q: &str) -> eidos_domain::SearchResponse {
        let parsed = eidos_query::parse(q).unwrap();
        let r = SearchRequest::new(parsed.query);
        search_with_content(
            &self.state.index,
            Some(&self.state.content_index),
            &self.state.catalog,
            &r,
            &ExecOptions::default(),
        )
        .unwrap()
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn central_search_returns_the_replicated_union_with_truthful_origin_and_coverage() {
    let mut central = Host::new("central-host", true, false);
    let mut node = Host::new("laptop", false, true);
    central.start_fleet();
    node.start_fleet();
    let endpoint = wait_for(Duration::from_secs(30), || {
        central.fleet().status().listening
    })
    .await
    .expect("listener");
    let invite = create_invite(
        &central.state.catalog,
        central.fleet().identity(),
        &central.fleet().config(),
        &endpoint,
        None,
    )
    .unwrap();
    enroll(
        &node.state.catalog,
        node.fleet().identity(),
        &invite,
        Duration::from_secs(10),
    )
    .await
    .unwrap();

    // Converge: the central's replica cursor reaches the node's head.
    let node_head = node
        .state
        .catalog
        .sync_source(node.source.unwrap())
        .ok()
        .flatten()
        .map(|s| s.head_seq);
    let replica = wait_for(Duration::from_secs(90), || {
        let r = central
            .state
            .catalog
            .replica_sources()
            .ok()?
            .into_iter()
            .next()?;
        let head = node
            .state
            .catalog
            .sync_source(node.source.unwrap())
            .ok()??
            .head_seq;
        (r.admission.applied_seq >= head && head > 0).then_some(r.source_id)
    })
    .await
    .unwrap_or_else(|| {
        panic!(
            "no convergence (node head {node_head:?}): {:#?}",
            central.fleet().status()
        )
    });

    // The ordinary follower projects the replicated rows.
    eidos_service::follower::follow_once(&central.state).unwrap();

    // The source-list API carries the same remote freshness and explicit
    // metadata-only content facts as search completeness.
    let response = eidos_service::api::router(central.state.clone(), None)
        .oneshot(
            Request::builder()
                .uri("/api/sources")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    let sources: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let replica_view = sources
        .as_array()
        .unwrap()
        .iter()
        .find(|source| {
            source["source"]["id"]
                .as_str()
                .and_then(|id| id.parse::<i64>().ok())
                == Some(replica.0)
        })
        .expect("replica source view");
    assert_eq!(replica_view["completeness"]["content_complete"], false);
    assert_eq!(replica_view["completeness"]["freshness"], "live");
    assert_eq!(replica_view["completeness"]["remote"]["connected"], true);

    // Catalog-level guards protect in-process CLI and benchmark callers too,
    // before they touch the origin path or create local content work.
    let scan = run_scan(
        &central.state.catalog,
        replica,
        central.state.lister.as_ref(),
        &RunScanOptions::default(),
    );
    assert!(scan.is_err());
    assert_eq!(
        central
            .state
            .catalog
            .get_source(replica)
            .unwrap()
            .unwrap()
            .kind,
        SourceKind::Remote
    );
    assert!(central
        .state
        .catalog
        .set_content_policy(replica, true, 1)
        .is_err());
    assert!(central
        .state
        .catalog
        .requeue_archives(Some(replica))
        .is_err());

    let r = central.search("figures");
    assert_eq!(r.hits.len(), 1, "{r:?}");
    let hit = &r.hits[0];
    assert_eq!(hit.source_id, replica);
    assert_ne!(
        hit.host_id, central.state.host_id,
        "the hit names the origin host"
    );
    assert_eq!(hit.size, 340);
    assert!(hit.path.as_deref().unwrap_or("").ends_with("figures.csv"));
    assert_eq!(hit.content.state, eidos_domain::ContentState::NotReplicated);
    let c = r
        .completeness
        .iter()
        .find(|c| c.source_id == replica)
        .expect("completeness for the replica");
    assert!(c.metadata_complete);
    assert!(!c.content_complete);
    let remote = c.remote.as_ref().expect("remote completeness");
    assert_eq!(remote.node_name, "laptop");
    assert!(remote.connected);
    assert_eq!(remote.applied_seq, remote.reported_head);
    assert_eq!(c.freshness, eidos_domain::Freshness::Live);
    assert!(
        r.coverage.full,
        "metadata query over a connected replica is full: {:?}",
        r.coverage
    );

    // The union: every live entry of the node's source is searchable centrally.
    let node_count = node.search("source:reports").total.value;
    let central_count = central.search(&format!("source:{}", replica.0)).total.value;
    assert_eq!(central_count, node_count);

    // Content queries say the content is not here rather than answering
    // from nothing.
    let r = central.search("content:anything");
    let reasons: Vec<_> = r
        .coverage
        .sources
        .iter()
        .flat_map(|s| s.degraded.iter().map(|d| d.kind))
        .collect();
    assert!(
        reasons.contains(&CoverageKind::ContentNotReplicated),
        "{reasons:?}"
    );

    // `host:` narrows to one node's sources, by name.
    assert_eq!(central.search("host:laptop figures").hits.len(), 1);
    let own = central.state.host_name.clone();
    let own_result = central.search(&format!("host:{own} figures"));
    assert_eq!(own_result.hits.len(), 0);
    assert!(
        own_result.completeness.is_empty(),
        "host scope must also narrow completeness: {own_result:?}"
    );
    assert!(own_result.coverage.full);
    let union = central.search(&format!("host:{own} OR host:laptop"));
    assert_eq!(
        union.completeness.len(),
        1,
        "OR branches must not intersect their coverage scopes: {union:?}"
    );
    let excluded = central.search("-host:laptop");
    assert_eq!(
        excluded.completeness.len(),
        1,
        "a negated host does not remove it from coverage scope: {excluded:?}"
    );
    assert_eq!(
        central
            .search(&format!("host:{} figures", hit.host_id.0))
            .hits
            .len(),
        1
    );

    // A change on the node reaches central search.
    std::fs::write(node.root.join("reports/q3/notes.md"), b"# q3").unwrap();
    run_scan(
        &node.state.catalog,
        node.source.unwrap(),
        node.state.lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    wait_for(Duration::from_secs(60), || {
        eidos_service::follower::follow_once(&central.state).ok()?;
        (central.search("notes").hits.len() == 1).then_some(())
    })
    .await
    .expect("the new file became searchable centrally");

    // When the node goes away the replica is preserved and says so.
    node.fleet().shutdown();
    wait_for(Duration::from_secs(30), || {
        let c = central.state.catalog.source_completeness(replica).ok()?;
        (!c.remote?.connected).then_some(())
    })
    .await
    .expect("disconnect observed");
    let r = central.search("figures");
    assert_eq!(
        r.hits.len(),
        1,
        "results are preserved while the node is offline"
    );
    let cov = r
        .coverage
        .sources
        .iter()
        .find(|s| s.source_id == replica)
        .unwrap();
    assert!(
        cov.degraded.iter().any(|d| d.kind == CoverageKind::Offline),
        "{cov:?}"
    );
    assert!(
        cov.watermark.is_some(),
        "authoritative as of the last applied batch"
    );
    assert!(!r.coverage.full);
    central.fleet().shutdown();
}

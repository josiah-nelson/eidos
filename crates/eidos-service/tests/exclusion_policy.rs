//! Policy recovery uses only disposable synthetic sources, never installed data.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use eidos_catalog::{
    exclusions::{ApplyExclusions, ExclusionRule, PreviewExclusions, RuleKind},
    NewSource,
};
use eidos_domain::{ContentState, SourceId, SourceKind};
use eidos_service::{
    content_workers::{apply_policies_once, commit_and_publish, reserve_and_claim, top_up_queue},
    scanner::{run_full_scan, ScanProgress},
    state::AppState,
    ServiceConfig,
};
use std::{path::PathBuf, sync::Arc};
use tower::ServiceExt;

struct Fx {
    dir: tempfile::TempDir,
    root: PathBuf,
    config: ServiceConfig,
    state: Arc<AppState>,
    source: SourceId,
}
impl Fx {
    fn new(files: usize, internal: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("source");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        for n in 0..files {
            std::fs::write(
                root.join(format!("docs/n{n}.txt")),
                "policyfixture constellation signal\n",
            )
            .unwrap();
        }
        let config = ServiceConfig {
            data_dir: if internal {
                root.join(".eidos")
            } else {
                dir.path().join("data")
            },
            log_dir: internal.then(|| root.join("logs")),
            content: false,
            fleet: false,
            auto_reconcile: false,
            update_check: false,
            ..Default::default()
        };
        if let Some(logs) = &config.log_dir {
            std::fs::create_dir_all(logs).unwrap();
            std::fs::write(logs.join("service.log"), "must not be read").unwrap();
        }
        let state = Arc::new(AppState::open(&config).unwrap());
        let source = state
            .catalog
            .add_source(&NewSource {
                host_id: state.host_id,
                name: "fixture".into(),
                kind: SourceKind::WindowsGeneric,
                root_path: root.display().to_string(),
                aliases: vec![],
            })
            .unwrap();
        run_full_scan(&state, source, &ScanProgress::new(source)).unwrap();
        Self {
            dir,
            root,
            config,
            state,
            source,
        }
    }
    fn id(&self, relative: &str) -> eidos_domain::ObjectId {
        self.state
            .catalog
            .resolve_relative(self.source, relative)
            .unwrap()
            .unwrap()
    }
    fn apply(&self, rules: Vec<ExclusionRule>) {
        let revision = self
            .state
            .catalog
            .exclusion_policy(self.source)
            .unwrap()
            .revision;
        self.state
            .catalog
            .apply_exclusions(
                self.source,
                &ApplyExclusions {
                    expected_revision: revision,
                    rules,
                },
            )
            .unwrap();
    }
    fn settle(&self) {
        for _ in 0..30 {
            apply_policies_once(&self.state).unwrap();
            let status = self.state.catalog.exclusion_policy(self.source).unwrap();
            assert!(status.error.is_none(), "{status:?}");
            assert!(status.repair_error.is_none(), "{status:?}");
            if status.phase == "applied" && status.repair_phase == "applied" {
                return;
            }
        }
        panic!("policy did not settle");
    }
    fn index(&self, relative: &str) {
        let id = self.id(relative);
        let generation = self
            .state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .generation;
        let result = eidos_search::pipeline::process_object(
            &self.state.catalog,
            &self.state.content_index,
            id,
            generation,
            &Default::default(),
            None,
        )
        .unwrap();
        assert!(
            matches!(result, eidos_search::pipeline::ProcessResult::Indexed(_)),
            "{result:?}"
        );
        self.state.content_workers.pending_publish.lock().push(id);
        commit_and_publish(&self.state).unwrap();
    }
    fn hits(&self, query: &str) -> usize {
        eidos_service::follower::follow_once(&self.state).unwrap();
        eidos_search::exec::search_with_content(
            &self.state.index,
            Some(&self.state.content_index),
            &self.state.catalog,
            &eidos_domain::SearchRequest::new(eidos_query::parse(query).unwrap().query),
            &Default::default(),
        )
        .unwrap()
        .hits
        .len()
    }
}

fn folder(path: &str) -> ExclusionRule {
    ExclusionRule {
        id: "folder".into(),
        kind: RuleKind::Directory,
        pattern: path.into(),
        include: false,
    }
}
fn include(pattern: &str) -> ExclusionRule {
    ExclusionRule {
        id: "allow".into(),
        kind: RuleKind::Regex,
        pattern: pattern.into(),
        include: true,
    }
}

#[test]
fn catalog_only_apply_preserves_metadata_purges_old_content_and_reincludes() {
    let f = Fx::new(300, false);
    f.index("docs/n0.txt");
    assert_eq!(f.hits("content:constellation"), 1);
    let id = f.id("docs/n0.txt");
    let old = f.state.catalog.content_record(id).unwrap().unwrap();
    let before = f.state.catalog.source_counts(f.source).unwrap();
    f.apply(vec![folder("docs")]);
    assert!(
        !f.state
            .catalog
            .content_target(id)
            .unwrap()
            .unwrap()
            .content_enabled
    );
    apply_policies_once(&f.state).unwrap();
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.revision, 1);
    assert_eq!(status.engine_version, eidos_catalog::policy::POLICY_VERSION);
    assert_eq!(status.processed, 128);
    assert_ne!(status.phase, "applied");
    // Remove access to the source entirely. Application must use only catalog facts.
    let moved = f.dir.path().join("temporarily-offline");
    std::fs::rename(&f.root, &moved).unwrap();
    f.settle();
    let after = f.state.catalog.source_counts(f.source).unwrap();
    assert_eq!(
        (after.files, after.logical_bytes),
        (before.files, before.logical_bytes)
    );
    assert_eq!(after.content_excluded, 300);
    assert_eq!(f.state.content_index.num_docs(), 0);
    assert_eq!(f.hits("content:constellation"), 0);
    assert_eq!(f.hits("name:=n0.txt"), 1, "metadata remains searchable");
    assert!(f.state.catalog.content_record(id).unwrap().is_none());
    assert!(f
        .state
        .catalog
        .chunks_for(id, old.generation, &[0])
        .unwrap()
        .is_empty());
    assert!(
        f.state.catalog.finish_content(&old, true).is_err(),
        "late extraction must not restore excluded content"
    );
    let decision = &f.state.catalog.policy_decisions(id).unwrap()[0];
    assert_eq!(decision.reason, "user_exclude");
    assert_eq!(decision.rule, "operator:1:folder");
    assert_eq!(
        decision.policy_version,
        eidos_catalog::policy::POLICY_VERSION
    );
    assert!(f
        .state
        .catalog
        .source_completeness(f.source)
        .unwrap()
        .policy_note
        .is_none());
    f.apply(vec![folder("docs"), include(r"^docs/n0\.txt$")]);
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Pending
    );
    std::fs::rename(&moved, &f.root).unwrap();
    f.index("docs/n0.txt");
    assert!(f.state.content_index.num_docs() > 0);
    assert_eq!(
        f.state
            .catalog
            .source_counts(f.source)
            .unwrap()
            .content_excluded,
        299
    );
}

#[test]
fn cleanup_ack_failure_is_durable_and_restart_retry_finishes_it() {
    let mut f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.apply(vec![folder("docs")]);
    f.state.catalog.with_writer(|conn| { conn.execute_batch("CREATE TRIGGER fail_policy_ack BEFORE DELETE ON policy_cleanup BEGIN SELECT RAISE(ABORT, 'fixture cleanup acknowledgement failed'); END;")?; Ok(()) }).unwrap();
    apply_policies_once(&f.state).unwrap();
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.phase, "purging");
    let writes = f.state.catalog.writer_stats().acquisitions;
    apply_policies_once(&f.state).unwrap();
    assert_eq!(
        f.state.catalog.writer_stats().acquisitions,
        writes,
        "a stopped application must remain idle until retry"
    );
    assert!(status
        .error
        .unwrap()
        .contains("fixture cleanup acknowledgement failed"));
    assert_eq!(
        f.state.content_index.num_docs(),
        0,
        "index commit succeeded before catalog acknowledgement failed"
    );
    let source = f.source;
    f.state.request_shutdown();
    // No helper threads are running; drop all writer holders before reopen.
    drop(f.state);
    f.state = Arc::new(AppState::open(&f.config).unwrap());
    assert!(f
        .state
        .catalog
        .exclusion_policy(source)
        .unwrap()
        .error
        .is_some());
    assert!(f.state.catalog.policy_applying(source).unwrap());
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER fail_policy_ack;")?;
            Ok(())
        })
        .unwrap();
    f.state.catalog.set_policy_error(source, None).unwrap();
    f.settle();
    assert_eq!(
        f.state.catalog.policy_cleanup_batch(source).unwrap().len(),
        0
    );
    assert_eq!(
        f.state.catalog.exclusion_policy(source).unwrap().revision,
        1
    );
}

#[test]
fn apply_waits_for_claimed_file_and_blocks_new_claims_and_scans() {
    let f = Fx::new(2, false);
    f.state
        .content_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    top_up_queue(&f.state).unwrap();
    let (reservation, jobs) = reserve_and_claim(&f.state, "fixture", 1).unwrap().unwrap();
    f.apply(vec![folder("docs")]);
    assert!(reserve_and_claim(&f.state, "next", 1).unwrap().is_none());
    apply_policies_once(&f.state).unwrap();
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .processed,
        0
    );
    assert!(f
        .state
        .catalog
        .begin_scan(f.source, eidos_catalog::scan::ScanKind::Reconcile)
        .is_err());
    f.state.catalog.delete_job(jobs[0].id).unwrap();
    drop(reservation);
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .source_counts(f.source)
            .unwrap()
            .content_excluded,
        2
    );
}

#[test]
fn self_stores_have_visible_boundaries_and_no_enumerated_children() {
    let f = Fx::new(2, true);
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, ".eidos/catalog.db")
        .unwrap()
        .is_none());
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, "logs/service.log")
        .unwrap()
        .is_none());
    let boundary = f.id(".eidos");
    assert!(f
        .state
        .catalog
        .policy_decisions(boundary)
        .unwrap()
        .iter()
        .any(|p| p.reason == "self_store" && p.stage == "inventory"));
    let completeness = f.state.catalog.source_completeness(f.source).unwrap();
    assert!(!completeness.metadata_complete);
    assert!(completeness
        .policy_note
        .unwrap()
        .contains("unknown or last-known"));
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    assert!(
        !f.state
            .catalog
            .directory_aggregate(root)
            .unwrap()
            .unwrap()
            .complete
    );
    let preview = f
        .state
        .catalog
        .preview_exclusions(
            f.source,
            &PreviewExclusions {
                rules: vec![include(".*")],
                paths: vec![
                    ".eidos/catalog.db".into(),
                    "logs/service.log".into(),
                    "docs/n0.txt".into(),
                ],
            },
        )
        .unwrap();
    assert_eq!(preview[0].reason, "self_store");
    assert_eq!(preview[1].reason, "self_store");
    assert_eq!(preview[2].state, ContentState::Pending);
}

#[test]
fn newly_configured_store_purges_old_content_without_reenumeration() {
    let f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.state
        .catalog
        .configure_protected_paths(&[f.config.data_dir.clone(), f.root.join("docs")])
        .unwrap();
    f.settle();
    assert_eq!(f.state.content_index.num_docs(), 0);
    assert_eq!(
        f.state
            .catalog
            .get_object(f.id("docs/n0.txt"))
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Excluded
    );
    assert!(
        f.state
            .catalog
            .content_target(f.id("docs/n0.txt"))
            .unwrap()
            .unwrap()
            .content_state
            == ContentState::Excluded
    );
    run_full_scan(&f.state, f.source, &ScanProgress::new(f.source)).unwrap();
    assert!(
        f.state
            .catalog
            .resolve_relative(f.source, "docs/n0.txt")
            .unwrap()
            .is_some(),
        "last-known metadata must not turn into silent absence"
    );
}

#[test]
fn nested_store_boundaries_stay_relative_and_never_protect_the_whole_source() {
    let f = Fx::new(1, false);
    let nested = f.root.join("docs").join("store");
    std::fs::create_dir_all(&nested).unwrap();
    f.state
        .catalog
        .configure_protected_paths(std::slice::from_ref(&nested))
        .unwrap();
    let policy = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(policy.protected_directories, vec!["docs/store".to_owned()]);
    // A boundary two components deep must not swallow its siblings.
    assert!(!f
        .state
        .catalog
        .path_is_protected(f.source, &f.root.join("docs").display().to_string())
        .unwrap());
    assert!(f
        .state
        .catalog
        .path_is_protected(f.source, &nested.join("catalog.db").display().to_string())
        .unwrap());
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .get_object(f.id("docs/n0.txt"))
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Pending,
    );
    // Protecting the source root itself is still the whole-root boundary.
    f.state
        .catalog
        .configure_protected_paths(std::slice::from_ref(&f.root))
        .unwrap();
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .protected_directories,
        vec![String::new()]
    );
}

#[test]
fn a_file_rename_repairs_only_the_changed_object_without_closing_source_admission() {
    use eidos_catalog::changes::ChangeEvent;
    let f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.apply(vec![folder("hidden")]);
    f.settle();
    let (_, snapshot) = native(&f, "docs/n0.txt");
    let parent = f
        .state
        .catalog
        .get_object(f.id("docs"))
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    let rename = |from: &str, to: &str| {
        f.state
            .catalog
            .apply_changes(
                f.source,
                &[
                    ChangeEvent::Unlink {
                        parent,
                        name: from.into(),
                    },
                    ChangeEvent::Link {
                        parent,
                        name: to.into(),
                        snapshot: snapshot.clone(),
                    },
                ],
                None,
            )
            .unwrap();
    };
    // Still outside every rule: nothing to reapply, so content claims and
    // scans for this source must stay open.
    rename("n0.txt", "n1.txt");
    assert!(!f.state.catalog.policy_applying(f.source).unwrap());
    assert_eq!(f.state.content_index.num_docs(), 1);
    assert!(
        f.state
            .catalog
            .content_target(f.id("docs/n1.txt"))
            .unwrap()
            .unwrap()
            .content_enabled
    );
    // A rename that changes the decision schedules only that object's repair.
    f.apply(vec![folder("docs/quarantine.txt")]);
    f.settle();
    rename("n1.txt", "quarantine.txt");
    assert!(!f.state.catalog.policy_applying(f.source).unwrap());
    let repairing = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(repairing.repair_phase, "applying");
    assert_eq!(repairing.repair_pending, 1);
    assert!(
        f.state
            .catalog
            .content_target(f.id("docs/quarantine.txt"))
            .unwrap()
            .unwrap()
            .content_enabled,
        "subtree repair must not close admission for the source"
    );
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .get_object(f.id("docs/quarantine.txt"))
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Excluded
    );
    assert_eq!(f.state.content_index.num_docs(), 0);
}

#[test]
fn hard_links_keep_canonical_path_policy_until_the_canonical_link_is_removed() {
    use eidos_catalog::changes::ChangeEvent;
    let f = Fx::new(1, false);
    f.apply(vec![folder("quarantine.txt")]);
    f.settle();
    f.index("docs/n0.txt");
    let id = f.id("docs/n0.txt");
    let before_generation = f.state.catalog.get_object(id).unwrap().unwrap().generation;
    let (_, mut snapshot) = native(&f, "docs/n0.txt");
    snapshot.link_count = 2;
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[ChangeEvent::Link {
                parent,
                name: "quarantine.txt".into(),
                snapshot,
            }],
            None,
        )
        .unwrap();
    assert_eq!(f.id("quarantine.txt"), id);
    assert_eq!(
        f.state.catalog.get_object(id).unwrap().unwrap().generation,
        before_generation,
        "adding a noncanonical hard link must not force re-extraction"
    );
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applied"
    );

    f.state
        .catalog
        .apply_changes(
            f.source,
            &[ChangeEvent::Unlink {
                parent: native(&f, "docs").0,
                name: "n0.txt".into(),
            }],
            None,
        )
        .unwrap();
    let repairing = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(repairing.repair_phase, "applying");
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Excluded
    );
    assert_eq!(f.state.content_index.num_docs(), 0);
}

#[test]
fn only_objects_that_stored_content_queue_a_derived_index_deletion() {
    let f = Fx::new(300, false);
    f.index("docs/n0.txt");
    let indexed = f.id("docs/n0.txt");
    f.apply(vec![folder("docs")]);
    for _ in 0..10 {
        f.state.catalog.apply_policy_batch(f.source).unwrap();
    }
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.phase, "purging");
    assert_eq!(status.changed, 300);
    // 299 of them never produced chunks or a content record, so queuing them
    // would commit a page of no-op index deletions for each 128 objects.
    assert_eq!(
        f.state
            .catalog
            .policy_cleanup_batch(f.source)
            .unwrap()
            .iter()
            .map(|cleanup| cleanup.object_id)
            .collect::<Vec<_>>(),
        vec![indexed]
    );
    f.settle();
    assert_eq!(f.state.content_index.num_docs(), 0);
    assert_eq!(
        f.state
            .catalog
            .source_counts(f.source)
            .unwrap()
            .content_excluded,
        300
    );
}

#[test]
fn a_move_during_full_application_keeps_its_cursor_and_queues_subtree_repair() {
    use eidos_catalog::changes::ChangeEvent;
    let f = Fx::new(300, false);
    f.apply(vec![folder("hidden")]);
    f.state.catalog.apply_policy_batch(f.source).unwrap();
    let progressed = f
        .state
        .catalog
        .exclusion_policy(f.source)
        .unwrap()
        .processed;
    assert_eq!(progressed, 128);
    let (_, snapshot) = native(&f, "docs");
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[
                ChangeEvent::Unlink {
                    parent,
                    name: "docs".into(),
                },
                ChangeEvent::Link {
                    parent,
                    name: "hidden".into(),
                    snapshot,
                },
            ],
            None,
        )
        .unwrap();
    // The running full revision keeps its cursor. The move is recorded in the
    // independent repair frontier instead of rewinding this source-wide pass.
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .processed,
        progressed
    );
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applying"
    );
    // Both operations converge and the repair observes the moved subtree.
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .get_object(f.id("hidden/n0.txt"))
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Excluded
    );
    assert_eq!(
        f.state
            .catalog
            .source_counts(f.source)
            .unwrap()
            .content_excluded,
        300
    );
}

async fn request(
    f: &Fx,
    method: &str,
    endpoint: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let response = eidos_service::api::router(f.state.clone(), None)
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/sources/{}/policy{endpoint}", f.source.0))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn api_validates_previews_and_rejects_stale_revisions_without_mutation() {
    let f = Fx::new(1, false);
    let bad = serde_json::json!({ "rules": [include("[")], "paths": [] });
    assert!(!request(&f, "POST", "/preview", bad).await.0.is_success());
    assert_eq!(
        f.state.catalog.exclusion_policy(f.source).unwrap().revision,
        0
    );
    let (status, preview) = request(
        &f,
        "POST",
        "/preview",
        serde_json::json!({ "rules": [folder("docs")], "paths": ["docs/n0.txt", "docs2/n0.txt"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preview[0]["state"], "excluded");
    assert_eq!(preview[1]["state"], "pending");
    let body = serde_json::json!({ "expected_revision": 0, "rules": [folder("docs")] });
    let (status, applied) = request(&f, "POST", "", body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(applied["revision"], 1);
    assert_eq!(applied["phase"], "applying");
    assert_eq!(applied["processed"], "0");
    assert_eq!(request(&f, "POST", "", body).await.0, StatusCode::CONFLICT);
    f.settle();
    let (_, status) = request(&f, "GET", "", serde_json::Value::Null).await;
    let decoded: eidos_catalog::exclusions::ExclusionPolicy =
        serde_json::from_value(status).unwrap();
    assert_eq!(decoded.phase, "applied");
}

fn native(
    f: &Fx,
    relative: &str,
) -> (
    eidos_catalog::changes::NativeKey,
    eidos_catalog::changes::ObjectSnapshot,
) {
    let obj = f.state.catalog.get_object(f.id(relative)).unwrap().unwrap();
    let identity = obj.native.unwrap();
    (
        identity.into(),
        eidos_catalog::changes::ObjectSnapshot {
            native: identity,
            kind: obj.kind,
            size: obj.size,
            allocated: obj.allocated,
            attributes: obj.attributes,
            created: obj.created,
            modified: obj.modified,
            changed: obj.changed,
            accessed: obj.accessed,
            reparse_tag: obj.reparse_tag,
            link_count: obj.link_count,
        },
    )
}

fn move_root_directory(f: &Fx, from: &str, to: &str) {
    use eidos_catalog::changes::ChangeEvent;
    let (_, snapshot) = native(f, from);
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[
                ChangeEvent::Unlink {
                    parent,
                    name: from.into(),
                },
                ChangeEvent::Link {
                    parent,
                    name: to.into(),
                    snapshot,
                },
            ],
            None,
        )
        .unwrap();
}

#[test]
fn native_changes_cannot_reinsert_internal_children_and_advance_the_checkpoint() {
    use eidos_catalog::changes::{ChangeEvent, Checkpoint};
    let f = Fx::new(1, true);
    let (parent, _) = native(&f, ".eidos");
    let (_, mut snapshot) = native(&f, "docs/n0.txt");
    snapshot.native = eidos_domain::NativeIdentity::from_u128(
        snapshot.native.volume_serial,
        u128::MAX - 19,
        eidos_domain::IdentityConfidence::Native,
    );
    let cp = Checkpoint {
        kind: "fixture".into(),
        value: serde_json::json!({ "offset": 123 }),
    };
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[ChangeEvent::Link {
                parent,
                name: "new-log.txt".into(),
                snapshot,
            }],
            Some(&cp),
        )
        .unwrap();
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, ".eidos/new-log.txt")
        .unwrap()
        .is_none());
    assert_eq!(f.state.catalog.checkpoint(f.source).unwrap().unwrap().0, cp);
}

#[test]
fn moving_a_catalogued_subtree_across_an_internal_store_boundary_cannot_recreate_it() {
    use eidos_catalog::changes::{ChangeEvent, Checkpoint};
    let f = Fx::new(1, true);
    f.index("docs/n0.txt");
    let id = f.id("docs/n0.txt");
    let (_, snapshot) = native(&f, "docs");
    let (protected_parent, _) = native(&f, ".eidos");
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let root_parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    let checkpoint = Checkpoint {
        kind: "fixture".into(),
        value: serde_json::json!({ "offset": 321 }),
    };
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[
                ChangeEvent::Unlink {
                    parent: root_parent,
                    name: "docs".into(),
                },
                ChangeEvent::Link {
                    parent: protected_parent,
                    name: "moved".into(),
                    snapshot,
                },
            ],
            Some(&checkpoint),
        )
        .unwrap();
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, ".eidos/moved/n0.txt")
        .unwrap()
        .is_none());
    assert!(f
        .state
        .catalog
        .get_object(id)
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
    assert!(f.state.catalog.content_target(id).unwrap().is_none());
    assert_eq!(
        f.state.catalog.checkpoint(f.source).unwrap().unwrap().0,
        checkpoint
    );
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applied",
        "immutable protection is enforced before a repair can admit the path"
    );
}

#[test]
fn directory_move_reapplies_descendant_rules_without_a_byte_change_or_rescan() {
    use eidos_catalog::changes::ChangeEvent;
    let f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.apply(vec![folder("hidden")]);
    f.settle();
    let id = f.id("docs/n0.txt");
    let (_, snapshot) = native(&f, "docs");
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[
                ChangeEvent::Unlink {
                    parent,
                    name: "docs".into(),
                },
                ChangeEvent::Link {
                    parent,
                    name: "hidden".into(),
                    snapshot,
                },
            ],
            None,
        )
        .unwrap();
    assert!(!f.state.catalog.policy_applying(f.source).unwrap());
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applying"
    );
    f.settle();
    assert_eq!(f.id("hidden/n0.txt"), id);
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Excluded
    );
    assert_eq!(f.state.content_index.num_docs(), 0);
}

#[test]
fn an_in_flight_move_is_fenced_at_publication_and_a_second_move_cannot_ack_stale_cleanup() {
    let f = Fx::new(1, false);
    f.apply(vec![folder("hidden")]);
    f.settle();
    let id = f.id("docs/n0.txt");
    let generation = f.state.catalog.get_object(id).unwrap().unwrap().generation;
    let result = eidos_search::pipeline::process_object(
        &f.state.catalog,
        &f.state.content_index,
        id,
        generation,
        &Default::default(),
        None,
    )
    .unwrap();
    assert!(matches!(
        result,
        eidos_search::pipeline::ProcessResult::Indexed(_)
    ));

    // The path changes after extraction stored its `indexing` record but
    // before the derived index is committed and acknowledged.
    move_root_directory(&f, "docs", "hidden");
    assert!(
        f.state
            .catalog
            .write_chunks(
                id,
                generation,
                &[eidos_content::Chunk {
                    ordinal: 99,
                    byte_start: 0,
                    byte_end: 1,
                    line_start: 0,
                    line_end: 0,
                    text: "x".into(),
                    split_line: false,
                }],
            )
            .is_err(),
        "a moved-into-exclusion generation must reject later chunk writes"
    );
    f.state.content_workers.pending_publish.lock().push(id);
    assert_eq!(commit_and_publish(&f.state).unwrap(), 0);
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_ne!(status.repair_phase, "applied");
    assert!(f
        .state
        .catalog
        .policy_cleanup_batch(f.source)
        .unwrap()
        .iter()
        .any(|cleanup| cleanup.object_id == id));
    assert_ne!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Indexed
    );

    // Move back while the first derived-index delete is still pending. The
    // cleanup row must survive, and the reset repair frontier must observe the
    // newest path before coverage can become complete.
    move_root_directory(&f, "hidden", "docs");
    assert!(f
        .state
        .catalog
        .policy_cleanup_batch(f.source)
        .unwrap()
        .iter()
        .any(|cleanup| cleanup.object_id == id));
    f.settle();
    assert_eq!(f.state.content_index.num_docs(), 0);
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Pending
    );
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.repair_phase, "applied");
    assert!(status.repair_processed >= 2);
}

#[test]
fn an_old_cleanup_fences_only_its_object_and_cannot_delete_the_reincluded_generation() {
    use eidos_domain::JobStage;
    let f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.apply(vec![folder("hidden")]);
    f.settle();
    let id = f.id("docs/n0.txt");

    move_root_directory(&f, "docs", "hidden");
    for _ in 0..10 {
        f.state.catalog.apply_policy_repair_batch(f.source).unwrap();
        if f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase
            == "purging"
        {
            break;
        }
    }
    let old_cleanup = f.state.catalog.policy_cleanup_batch(f.source).unwrap();
    assert_eq!(old_cleanup.len(), 1);

    // Re-inclusion can finish its frontier while the prior object-wide index
    // delete is still pending. That object must remain inadmissible even though
    // the rest of the source stays open.
    move_root_directory(&f, "hidden", "docs");
    for _ in 0..10 {
        f.state.catalog.apply_policy_repair_batch(f.source).unwrap();
        if f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase
            == "purging"
        {
            break;
        }
    }
    assert!(!f.state.catalog.policy_applying(f.source).unwrap());
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Pending
    );
    top_up_queue(&f.state).unwrap();
    assert!(f
        .state
        .catalog
        .claim_job(&[JobStage::ContentText], "cleanup-fenced")
        .unwrap()
        .is_none());
    assert!(
        !f.state
            .catalog
            .content_target(id)
            .unwrap()
            .unwrap()
            .content_enabled
    );

    // Model a worker that crossed its admission check just before cleanup was
    // recorded: let it store the current generation, then restore the older
    // token before its staged index write is committed and acknowledged.
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute("DELETE FROM policy_cleanup WHERE object_id = ?1", [id.0])?;
            Ok(())
        })
        .unwrap();
    let in_flight_generation = f.state.catalog.get_object(id).unwrap().unwrap().generation;
    let result = eidos_search::pipeline::process_object(
        &f.state.catalog,
        &f.state.content_index,
        id,
        in_flight_generation,
        &Default::default(),
        None,
    )
    .unwrap();
    assert!(matches!(
        result,
        eidos_search::pipeline::ProcessResult::Indexed(_)
    ));
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute(
                "INSERT INTO policy_cleanup(object_id, source_id, generation)
                 VALUES (?1, ?2, ?3)",
                [id.0, f.source.0, i64::from(old_cleanup[0].generation)],
            )?;
            Ok(())
        })
        .unwrap();
    f.state.content_workers.pending_publish.lock().push(id);
    assert_eq!(commit_and_publish(&f.state).unwrap(), 0);
    assert_eq!(
        f.state
            .catalog
            .get_object(id)
            .unwrap()
            .unwrap()
            .content_state,
        ContentState::Pending
    );
    assert!(f.state.catalog.get_object(id).unwrap().unwrap().generation > in_flight_generation);

    // The final publication fence advanced cleanup, so an acknowledgement for
    // the older selected token cannot erase the newer request.
    f.state
        .catalog
        .acknowledge_policy_cleanup(f.source, &old_cleanup)
        .unwrap();
    let current_cleanup = f.state.catalog.policy_cleanup_batch(f.source).unwrap();
    assert_eq!(current_cleanup.len(), 1);
    assert!(current_cleanup[0].generation > old_cleanup[0].generation);

    f.state.content_index.delete_object(id);
    f.state.content_index.commit().unwrap();
    assert!(f
        .state
        .catalog
        .claim_job(&[JobStage::ContentText], "between-delete-and-ack")
        .unwrap()
        .is_none());
    f.state
        .catalog
        .acknowledge_policy_cleanup(f.source, &current_cleanup)
        .unwrap();

    // Only the exact cleanup acknowledgement opens this object. The current
    // generation can then publish and a later repair turn cannot wipe it.
    f.index("docs/n0.txt");
    apply_policies_once(&f.state).unwrap();
    assert_eq!(f.state.content_index.num_docs(), 1);
    assert_eq!(f.hits("content:constellation"), 1);
}

#[test]
fn sustained_tiny_subtree_moves_still_admit_and_process_unaffected_queued_content() {
    use eidos_domain::JobStage;
    let f = Fx::new(1, false);
    std::fs::write(
        f.root.join("stable.txt"),
        "policyfixture unaffected progress\n",
    )
    .unwrap();
    run_full_scan(&f.state, f.source, &ScanProgress::new(f.source)).unwrap();
    f.apply(vec![folder("hidden")]);
    f.settle();

    move_root_directory(&f, "docs", "hidden");
    move_root_directory(&f, "hidden", "docs");
    move_root_directory(&f, "docs", "hidden");
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.phase, "applied");
    assert_eq!(status.repair_phase, "applying");
    assert_eq!(status.repair_pending, 1, "repeated root moves deduplicate");

    top_up_queue(&f.state).unwrap();
    let stable = f.id("stable.txt");
    let mut progressed = false;
    for attempt in 0..10 {
        let Some((_permit, jobs)) = f
            .state
            .catalog
            .claim_jobs_admitted(
                &[JobStage::ContentText],
                &format!("repair-fixture-{attempt}"),
                1,
                &mut |_| Some(()),
            )
            .unwrap()
        else {
            break;
        };
        let job = &jobs[0];
        let object = job.object_id.unwrap();
        let result = eidos_search::pipeline::process_object(
            &f.state.catalog,
            &f.state.content_index,
            object,
            job.object_generation,
            &Default::default(),
            Some(job.id),
        )
        .unwrap();
        if object == stable && matches!(result, eidos_search::pipeline::ProcessResult::Indexed(_)) {
            progressed = true;
            break;
        }
        if matches!(result, eidos_search::pipeline::ProcessResult::Skipped(_)) {
            f.state.catalog.complete_job(job.id).unwrap();
        }
    }
    assert!(
        progressed,
        "an unaffected queued file must actually be extracted"
    );
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applying",
        "unaffected progress must not require repair completion"
    );
}

#[test]
fn repair_completion_cannot_apply_or_erase_a_coexisting_full_revision() {
    let f = Fx::new(300, false);
    f.apply(vec![folder("hidden")]);
    f.state.catalog.apply_policy_batch(f.source).unwrap();
    let full_progress = f
        .state
        .catalog
        .exclusion_policy(f.source)
        .unwrap()
        .processed;
    move_root_directory(&f, "docs", "hidden");
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute(
                "UPDATE source_policy SET error = 'fixture full revision error' WHERE source_id = ?1",
                [f.source.0],
            )?;
            Ok(())
        })
        .unwrap();
    for _ in 0..10 {
        f.state.catalog.apply_policy_repair_batch(f.source).unwrap();
        let repair = f.state.catalog.exclusion_policy(f.source).unwrap();
        if repair.repair_phase == "purging" {
            break;
        }
    }
    f.state
        .catalog
        .acknowledge_policy_cleanup(f.source, &[])
        .unwrap();
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.repair_phase, "applied");
    assert_eq!(status.phase, "applying");
    assert_eq!(status.processed, full_progress);
    assert_eq!(status.error.as_deref(), Some("fixture full revision error"));
}

#[test]
fn repair_frontier_and_checkpoint_roll_back_with_a_failed_native_move() {
    use eidos_catalog::changes::Checkpoint;
    let f = Fx::new(1, false);
    f.apply(vec![folder("hidden")]);
    f.settle();
    let old = Checkpoint {
        kind: "fixture".into(),
        value: serde_json::json!({ "offset": 10 }),
    };
    let next = Checkpoint {
        kind: "fixture".into(),
        value: serde_json::json!({ "offset": 20 }),
    };
    f.state.catalog.set_checkpoint(f.source, &old).unwrap();
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_repair_frontier BEFORE INSERT ON policy_repair_frontier
                 BEGIN SELECT RAISE(ABORT, 'fixture repair frontier failed'); END;",
            )?;
            Ok(())
        })
        .unwrap();

    let (_, snapshot) = native(&f, "docs");
    let root = f
        .state
        .catalog
        .get_source(f.source)
        .unwrap()
        .unwrap()
        .root_object_id
        .unwrap();
    let parent = f
        .state
        .catalog
        .get_object(root)
        .unwrap()
        .unwrap()
        .native
        .unwrap()
        .into();
    let result = f.state.catalog.apply_changes(
        f.source,
        &[
            eidos_catalog::changes::ChangeEvent::Unlink {
                parent,
                name: "docs".into(),
            },
            eidos_catalog::changes::ChangeEvent::Link {
                parent,
                name: "hidden".into(),
                snapshot,
            },
        ],
        Some(&next),
    );
    assert!(result.is_err());
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER fail_repair_frontier;")?;
            Ok(())
        })
        .unwrap();
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, "docs/n0.txt")
        .unwrap()
        .is_some());
    assert!(f
        .state
        .catalog
        .resolve_relative(f.source, "hidden/n0.txt")
        .unwrap()
        .is_none());
    assert_eq!(
        f.state.catalog.checkpoint(f.source).unwrap().unwrap().0,
        old
    );
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applied"
    );
}

#[test]
fn restart_resumes_the_exact_subtree_frontier_cursor() {
    let mut f = Fx::new(300, false);
    f.apply(vec![folder("hidden")]);
    f.settle();
    move_root_directory(&f, "docs", "hidden");
    f.state.catalog.apply_policy_repair_batch(f.source).unwrap();
    let before = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(before.repair_phase, "applying");
    assert!(before.repair_processed > 0);
    assert!(before.repair_pending > 0);
    let source = f.source;
    f.state.request_shutdown();
    drop(f.state);
    f.state = Arc::new(AppState::open(&f.config).unwrap());
    let reopened = f.state.catalog.exclusion_policy(source).unwrap();
    assert_eq!(reopened.repair_processed, before.repair_processed);
    assert_eq!(reopened.repair_pending, before.repair_pending);
    assert_eq!(reopened.repair_phase, "applying");
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .source_counts(source)
            .unwrap()
            .content_excluded,
        300
    );
}

#[test]
fn tombstoned_repair_roots_are_discarded_without_sticking_coverage() {
    let f = Fx::new(1, false);
    f.apply(vec![folder("hidden")]);
    f.settle();
    move_root_directory(&f, "docs", "hidden");
    let (directory, _) = native(&f, "hidden");
    f.state
        .catalog
        .apply_changes(
            f.source,
            &[eidos_catalog::changes::ChangeEvent::Delete { object: directory }],
            None,
        )
        .unwrap();
    f.settle();
    let status = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(status.repair_phase, "applied");
    assert_eq!(status.repair_pending, 0);
}

#[test]
fn subtree_purge_ack_failure_is_durable_and_retryable_without_blocking_full_policy() {
    let f = Fx::new(1, false);
    f.index("docs/n0.txt");
    f.apply(vec![folder("hidden")]);
    f.settle();
    move_root_directory(&f, "docs", "hidden");
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch(
                "CREATE TRIGGER fail_repair_ack BEFORE DELETE ON policy_cleanup
                 BEGIN SELECT RAISE(ABORT, 'fixture repair acknowledgement failed'); END;",
            )?;
            Ok(())
        })
        .unwrap();
    apply_policies_once(&f.state).unwrap();
    let failed = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(failed.phase, "applied");
    assert!(failed.error.is_none());
    assert_eq!(failed.repair_phase, "purging");
    assert!(failed
        .repair_error
        .as_deref()
        .is_some_and(|e| e.contains("fixture repair acknowledgement failed")));
    assert_eq!(
        f.state.content_index.num_docs(),
        0,
        "derived delete commits before its failed acknowledgement"
    );
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER fail_repair_ack;")?;
            Ok(())
        })
        .unwrap();
    f.state.catalog.set_policy_error(f.source, None).unwrap();
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .repair_phase,
        "applied"
    );
}

#[test]
fn excluding_an_archive_removes_its_manifest_and_virtual_search_entries() {
    use eidos_archive::fixture::{build, Entry};
    let f = Fx::new(1, false);
    std::fs::write(
        f.root.join("docs/pack.zip"),
        build(
            &[Entry::file("nested/archivemarker.txt", b"fixture")],
            b"",
            false,
        ),
    )
    .unwrap();
    run_full_scan(&f.state, f.source, &ScanProgress::new(f.source)).unwrap();
    let id = f.id("docs/pack.zip");
    let generation = f.state.catalog.get_object(id).unwrap().unwrap().generation;
    eidos_search::pipeline::process_object(
        &f.state.catalog,
        &f.state.content_index,
        id,
        generation,
        &Default::default(),
        None,
    )
    .unwrap();
    assert!(f.state.catalog.archive_record(id).unwrap().is_some());
    assert_eq!(f.hits("name:=archivemarker.txt"), 1);
    f.apply(vec![folder("docs")]);
    f.settle();
    assert!(f.state.catalog.archive_record(id).unwrap().is_none());
    assert_eq!(f.hits("name:=archivemarker.txt"), 0);
    assert_eq!(f.hits("name:=pack.zip"), 1);
}

#[test]
fn catalog_fault_rolls_back_the_batch_cursor_and_retry_preserves_progress() {
    let f = Fx::new(300, false);
    f.apply(vec![folder("docs")]);
    apply_policies_once(&f.state).unwrap();
    let first = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(first.processed, 128);
    f.state.catalog.with_writer(|conn| { conn.execute_batch("CREATE TRIGGER fail_policy_batch BEFORE UPDATE OF generation ON objects BEGIN SELECT RAISE(ABORT, 'fixture policy batch failed'); END;")?; Ok(()) }).unwrap();
    apply_policies_once(&f.state).unwrap();
    let failed = f.state.catalog.exclusion_policy(f.source).unwrap();
    assert_eq!(failed.processed, 128);
    assert_eq!(failed.changed, first.changed);
    assert!(failed
        .error
        .unwrap()
        .contains("fixture policy batch failed"));
    f.state
        .catalog
        .with_writer(|conn| {
            conn.execute_batch("DROP TRIGGER fail_policy_batch;")?;
            Ok(())
        })
        .unwrap();
    f.state.catalog.set_policy_error(f.source, None).unwrap();
    f.settle();
    assert_eq!(
        f.state
            .catalog
            .exclusion_policy(f.source)
            .unwrap()
            .processed,
        302
    );
    assert_eq!(
        f.state
            .catalog
            .source_counts(f.source)
            .unwrap()
            .content_excluded,
        300
    );
}

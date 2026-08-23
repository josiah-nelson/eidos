//! Search semantic golden tests on a synthetic fixture: Q-4 (heterogeneous
//! descendants), Q-6 name regex + subtree size, `.dmp` inventory, exact
//! case-sensitive names, regex/glob/substring, sorting, facets, cursors,
//! completeness, and follower updates.

use eidos_catalog::scan::{run_scan, RunScanOptions};
use eidos_catalog::{Catalog, NewSource};
use eidos_domain::*;
use eidos_query::parse;
use eidos_search::exec::{search, ExecOptions};
use eidos_search::CatalogIndex;
use std::path::PathBuf;
use std::sync::Arc;

struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    catalog: Arc<Catalog>,
    index: Arc<CatalogIndex>,
    source: SourceId,
}

fn write(p: &PathBuf, n: usize) {
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, vec![b'x'; n]).unwrap();
}

fn fixture() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    // Q-4: .idb and .cs in distinct nested folders under "proj"; "proj/sub"
    // contains only .cs; "other" has .cs but no .idb.
    write(&root.join("proj/ida/sample.idb"), 50);
    write(&root.join("proj/ida/sample.id0"), 10);
    write(&root.join("proj/src/Program.cs"), 300);
    write(&root.join("proj/src/util/Helpers.cs"), 200);
    write(&root.join("proj/README.md"), 40);
    write(&root.join("other/app/Main.cs"), 100);
    write(&root.join("other/notes.txt"), 10);
    // Q-6: similarly named asset directories with numeric copy suffixes.
    write(&root.join("assets/192.0.2.130/a.bin"), 1000);
    write(&root.join("assets/192.0.2.130/b.bin"), 1000);
    write(&root.join("assets/192.0.2.130 (01)/a.bin"), 1000);
    write(&root.join("assets/192.0.2.130 (02)/a.bin"), 4000);
    write(&root.join("assets/192.0.2.131/a.bin"), 9000);
    // Q-7: dumps.
    write(&root.join("dumps/crash1.dmp"), 5000);
    write(&root.join("proj/src/crash2.DMP"), 6000);
    // Exact case.
    // NTFS is case-insensitive: one file, mixed-case name.
    write(&root.join("logs/Qz.log"), 10);
    write(&root.join("logs/QZ-endpoint.txt"), 10);
    write(&root.join("logs/Zephyr diagnostics.md"), 10);

    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "windows").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "fx".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    let lister = eidos_scanner::default_lister();
    run_scan(
        &catalog,
        source,
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    let index = CatalogIndex::open(dir.path().join("index")).unwrap();
    let rebuilt = index.sync_sources(&catalog).unwrap();
    assert_eq!(rebuilt.len(), 1);
    index.reload().unwrap();
    Fx {
        _dir: dir,
        root,
        catalog,
        index,
        source,
    }
}

impl Fx {
    fn run(&self, q: &str) -> SearchResponse {
        self.run_req(self.req(q))
    }
    fn req(&self, q: &str) -> SearchRequest {
        let parsed = parse(q).unwrap();
        let mut r = SearchRequest::new(parsed.query);
        r.explain = true;
        r
    }
    fn run_req(&self, req: SearchRequest) -> SearchResponse {
        search(&self.index, &self.catalog, &req, &ExecOptions::default()).unwrap()
    }
    fn names(&self, q: &str) -> Vec<String> {
        let mut v: Vec<String> = self.run(q).hits.into_iter().map(|h| h.name).collect();
        v.sort();
        v
    }
    fn dir_req(&self, q: &str) -> SearchRequest {
        let mut r = self.req(q);
        r.mode = ResultMode::Directories;
        r
    }
}

#[test]
fn index_build_counts_and_completeness() {
    let fx = fixture();
    let r = fx.run("");
    assert!(r.total.exact);
    let counts = fx.catalog.source_counts(fx.source).unwrap();
    assert_eq!(counts.files, 17);
    assert_eq!(r.total.value, counts.files, "files only by default");
    assert_eq!(r.completeness.len(), 1);
    assert!(r.completeness[0].metadata_complete);
    assert!(!r.completeness[0].content_complete);
    assert_eq!(
        fx.index.num_docs(),
        counts.files + counts.directories + 1,
        "files + directories + root entry"
    );
}

#[test]
fn q4_heterogeneous_descendants_ranks_tightest_directory_first() {
    let fx = fixture();
    let r = fx.run_req(fx.dir_req("has:idb has:cs"));
    let names: Vec<(&str, Option<f32>)> =
        r.hits.iter().map(|h| (h.name.as_str(), h.score)).collect();
    assert_eq!(
        names[0].0, "proj",
        "proj is the tightest container of both kinds: {names:?}"
    );
    // The root also contains both but only through proj; it ranks second.
    assert_eq!(r.hits.len(), 2, "{names:?}");
    assert!(r.hits[0].score.unwrap() > r.hits[1].score.unwrap());
    assert_eq!(r.hits[0].directory.as_ref().unwrap().file_count, 6);
    assert!(r
        .explanation
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .any(|s| s.stage == "rank"));
    // Minimum counts: proj has 2 .cs files, the root has 3 (Main.cs is
    // under "other"); requiring 4 finds nothing.
    let r = fx.run_req(fx.dir_req("has:cs>=2 has:idb"));
    assert_eq!(r.hits[0].name, "proj");
    let r = fx.run_req(fx.dir_req("has:cs>=3 has:idb"));
    assert_eq!(r.hits.len(), 1);
    assert_eq!(
        r.hits[0].name, "",
        "only the root satisfies >=3 .cs plus .idb"
    );
    let r = fx.run_req(fx.dir_req("has:cs>=4 has:idb"));
    assert_eq!(r.total.value, 0);
}

#[test]
fn q6_name_regex_and_subtree_size() {
    let fx = fixture();
    let mut req = fx.dir_req("name:/^192\\.0\\.2\\.130( \\(\\d+\\))?$/");
    req.sort = Sort {
        field: SortField::SubtreeSize,
        descending: true,
    };
    let r = fx.run_req(req);
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["192.0.2.130 (02)", "192.0.2.130", "192.0.2.130 (01)"]
    );
    assert_eq!(r.hits[0].directory.as_ref().unwrap().logical_bytes, 4000);
    assert!(r.hits.iter().all(|h| h.kind == ObjectKind::Directory));
}

#[test]
fn q7_dump_inventory_is_case_insensitive_on_extension() {
    let fx = fixture();
    assert_eq!(fx.names("ext:dmp"), vec!["crash1.dmp", "crash2.DMP"]);
    assert_eq!(fx.names("*.dmp"), vec!["crash1.dmp", "crash2.DMP"]);
}

#[test]
fn exact_case_sensitive_name() {
    let fx = fixture();
    assert_eq!(fx.names("name:=Qz.log"), vec!["Qz.log"]);
    assert!(
        fx.names("name:=qz.log").is_empty(),
        "case-sensitive exact rejects qz.log"
    );
    assert_eq!(fx.names("name:=iqz.log"), vec!["Qz.log"]);
    assert_eq!(fx.names("name:~Qz"), vec!["Qz.log"]);
    assert!(fx.names("name:~qz").is_empty());
    assert_eq!(fx.names("name:qz"), vec!["QZ-endpoint.txt", "Qz.log"]);
    assert!(fx.names("name:/^qz\\.log$/c").is_empty());
    assert_eq!(fx.names("name:/^qz\\.log$/"), vec!["Qz.log"]);
    assert_eq!(fx.names("name:/^Qz\\.log$/c"), vec!["Qz.log"]);
    let r = fx.run("name:=Qz.log");
    let steps = &r.explanation.unwrap().steps;
    assert!(steps.iter().any(|s| s.stage == "verify"), "{steps:?}");
}

#[test]
fn ranked_terms_phrase_and_path_scoping() {
    let fx = fixture();
    assert_eq!(
        fx.names("zephyr diagnostics"),
        vec!["Zephyr diagnostics.md"]
    );
    assert_eq!(
        fx.names("\"zephyr diagnostics\""),
        vec!["Zephyr diagnostics.md"]
    );
    assert_eq!(fx.names("helpers ext:cs"), vec!["Helpers.cs"]);
    let under = format!("path:{}", fx.root.join("proj").display());
    assert_eq!(
        fx.names(&format!("{under} ext:cs")),
        vec!["Helpers.cs", "Program.cs"]
    );
    assert_eq!(
        fx.names("path:*\\src\\* ext:cs"),
        vec!["Helpers.cs", "Program.cs"]
    );
    assert_eq!(fx.names("ext:cs -path:util"), vec!["Main.cs", "Program.cs"]);
    let src = fx
        .catalog
        .resolve_relative(fx.source, "proj/src")
        .unwrap()
        .unwrap();
    assert_eq!(
        fx.names(&format!("in:o:{} kind:file", src.0)),
        vec!["Helpers.cs", "Program.cs", "crash2.DMP"]
    );
    assert_eq!(
        fx.names(&format!("in:o:{}~1 kind:file", src.0)),
        vec!["Program.cs", "crash2.DMP"]
    );
}

#[test]
fn size_time_sort_and_cursor() {
    let fx = fixture();
    assert_eq!(fx.names("size:>=4000 ext:bin"), vec!["a.bin", "a.bin"]);
    let mut req = fx.req("ext:bin");
    req.sort = Sort {
        field: SortField::Size,
        descending: true,
    };
    req.limit = 2;
    let r1 = fx.run_req(req.clone());
    assert_eq!(r1.hits[0].size, 9000);
    assert_eq!(r1.hits[1].size, 4000);
    assert_eq!(r1.total.value, 5);
    let cursor = r1.next_cursor.clone().expect("more pages");
    req.cursor = Some(cursor);
    let r2 = fx.run_req(req.clone());
    assert_eq!(r2.hits.len(), 2);
    assert!(r2.hits.iter().all(|h| h.size == 1000));
    req.cursor = r2.next_cursor.clone();
    let r3 = fx.run_req(req);
    assert_eq!(r3.hits.len(), 1);
    assert!(r3.next_cursor.is_none());
    // Name sort ascending via fast field.
    let mut req = fx.req("ext:cs");
    req.sort = Sort {
        field: SortField::Name,
        descending: false,
    };
    let r = fx.run_req(req);
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["Helpers.cs", "Main.cs", "Program.cs"]);
    assert_eq!(fx.run("mtime:>=1d ext:cs").total.value, 3);
    assert_eq!(fx.run("mtime:<2000-01-01 ext:cs").total.value, 0);
}

#[test]
fn facets() {
    let fx = fixture();
    let mut req = fx.req("");
    req.facets = vec![
        FacetRequest {
            field: FacetField::Extension,
            limit: 10,
        },
        FacetRequest {
            field: FacetField::Source,
            limit: 10,
        },
        FacetRequest {
            field: FacetField::SizeBucket,
            limit: 10,
        },
    ];
    let r = fx.run_req(req);
    let ext = r
        .facets
        .iter()
        .find(|f| f.field == FacetField::Extension)
        .unwrap();
    let cs = ext.values.iter().find(|v| v.value == "cs").unwrap();
    assert_eq!(cs.count, 3);
    let bin = ext.values.iter().find(|v| v.value == "bin").unwrap();
    assert_eq!(bin.count, 5);
    let src = r
        .facets
        .iter()
        .find(|f| f.field == FacetField::Source)
        .unwrap();
    assert_eq!(src.values[0].label.as_deref(), Some("fx"));
    assert_eq!(src.values[0].count, 17);
    let sizes = r
        .facets
        .iter()
        .find(|f| f.field == FacetField::SizeBucket)
        .unwrap();
    assert_eq!(sizes.values.iter().map(|v| v.count).sum::<u64>(), 17);
}

/// Every size and modification-time bucket carries clauses that reproduce
/// exactly the bucket the user clicked — including the first (open below),
/// the last (open above), and the exclusions — in both result modes.
#[test]
fn range_bucket_clauses_reproduce_their_bucket() {
    let fx = fixture();
    for (mode, base) in [
        (ResultMode::Files, ""),
        (ResultMode::Directories, ""),
        // An existing bound on the same field must still combine, not be
        // silently replaced.
        (ResultMode::Files, "size:>=100"),
    ] {
        let mut req = fx.req(base);
        req.mode = mode;
        req.facets = vec![
            FacetRequest {
                field: FacetField::SizeBucket,
                limit: 10,
            },
            FacetRequest {
                field: FacetField::ModifiedBucket,
                limit: 10,
            },
        ];
        let r = fx.run_req(req);
        let total = r.total.value;
        assert!(total > 0, "{mode:?} {base:?} has no results");
        let mut open_below = 0;
        let mut open_above = 0;
        for facet in &r.facets {
            let mut summed = 0;
            for v in &facet.values {
                let range = v.range.as_ref().unwrap_or_else(|| {
                    panic!("{:?} bucket {} has no clause", facet.field, v.value)
                });
                open_below += u32::from(range.from.is_none());
                open_above += u32::from(range.to.is_none());
                let mut include = fx.req(&format!("{base} {}", range.clause));
                include.mode = mode;
                assert_eq!(
                    fx.run_req(include).total.value,
                    v.count,
                    "{:?} include {} in {mode:?}",
                    facet.field,
                    range.clause
                );
                let mut exclude = fx.req(&format!("{base} {}", range.exclude));
                exclude.mode = mode;
                assert_eq!(
                    fx.run_req(exclude).total.value,
                    total - v.count,
                    "{:?} exclude {} in {mode:?}",
                    facet.field,
                    range.exclude
                );
                summed += v.count;
            }
            assert_eq!(summed, total, "{:?} buckets cover every hit", facet.field);
        }
        // Everything in the fixture is small, so the open-below first bucket
        // is always exercised above; the open-above bucket is whichever
        // modification-time bucket the run lands in (`facets.rs` unit tests
        // pin the clause text for both ends).
        assert_eq!(r.facets.len(), 2);
        assert!(open_below >= 1, "no open-ended first bucket in {mode:?}");
        let _ = open_above;
    }
}

/// The buckets mix per-file and per-subtree values in `both` mode, so they
/// are labelled but carry no clause rather than a clause that means
/// something else.
#[test]
fn range_buckets_are_display_only_in_both_mode() {
    let fx = fixture();
    let mut req = fx.req("");
    req.mode = ResultMode::Both;
    req.facets = vec![FacetRequest {
        field: FacetField::SizeBucket,
        limit: 10,
    }];
    let r = fx.run_req(req);
    let sizes = &r.facets[0];
    assert!(!sizes.values.is_empty());
    assert!(sizes.values.iter().all(|v| v.range.is_none()));
    assert!(sizes.values.iter().all(|v| v.label.is_some()));
}

#[test]
fn content_clause_is_rejected_truthfully() {
    let fx = fixture();
    let req = fx.req("content:Zephyr");
    let err = search(&fx.index, &fx.catalog, &req, &ExecOptions::default()).unwrap_err();
    assert!(err.to_string().contains("content"), "{err}");
}

#[test]
fn follower_applies_catalog_changes() {
    let fx = fixture();
    // Rescan after a filesystem change → new generation → rebuild.
    write(&fx.root.join("proj/src/New.cs"), 5);
    let lister = eidos_scanner::default_lister();
    run_scan(
        &fx.catalog,
        fx.source,
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    // Before sync, completeness reports the lag.
    let r = fx.run("ext:cs");
    assert!(!r.completeness[0].metadata_complete);
    assert!(r.completeness[0]
        .note
        .as_deref()
        .unwrap_or("")
        .contains("rebuilding"));
    let (rebuilt, _) = fx.index.follow_once(&fx.catalog, 100).unwrap();
    assert_eq!(rebuilt.len(), 1);
    fx.index.reload().unwrap();
    assert_eq!(
        fx.names("ext:cs"),
        vec!["Helpers.cs", "Main.cs", "New.cs", "Program.cs"]
    );
    assert!(fx.run("ext:cs").completeness[0].metadata_complete);

    // Incremental change through the outbox path.
    use eidos_catalog::changes::{ChangeEvent, NativeKey, ObjectSnapshot};
    let parent = fx
        .catalog
        .resolve_relative(fx.source, "proj/src")
        .unwrap()
        .unwrap();
    let parent_native = fx
        .catalog
        .get_object(parent)
        .unwrap()
        .unwrap()
        .native
        .unwrap();
    let snap = ObjectSnapshot {
        native: NativeIdentity::from_u128(
            parent_native.volume_serial,
            0xABCDEF,
            IdentityConfidence::Native,
        ),
        kind: ObjectKind::File,
        attributes: FileAttributes(0x20),
        size: 77,
        allocated: 4096,
        link_count: 1,
        created: Some(UnixNanos::now()),
        modified: Some(UnixNanos::now()),
        changed: None,
        accessed: None,
        reparse_tag: 0,
    };
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Link {
                parent: NativeKey::from(parent_native),
                name: "Incremental.cs".into(),
                snapshot: snap,
            }],
            None,
        )
        .unwrap();
    let r = fx.run("ext:cs");
    assert!(
        r.warnings.iter().any(|w| w.contains("not yet reflected")),
        "{:?}",
        r.warnings
    );
    let (_, follow) = fx.index.follow_once(&fx.catalog, 100).unwrap();
    assert!(follow.unwrap().documents_added >= 1);
    fx.index.reload().unwrap();
    assert!(fx.names("ext:cs").contains(&"Incremental.cs".to_string()));
    // Delete through the outbox.
    fx.catalog
        .apply_changes(
            fx.source,
            &[ChangeEvent::Delete {
                object: NativeKey {
                    volume_serial: parent_native.volume_serial,
                    id: 0xABCDEF,
                },
            }],
            None,
        )
        .unwrap();
    fx.index.follow_once(&fx.catalog, 100).unwrap();
    fx.index.reload().unwrap();
    assert!(!fx.names("ext:cs").contains(&"Incremental.cs".to_string()));
    assert_eq!(fx.catalog.outbox_pending().unwrap(), 0);
}

#[test]
fn later_subtree_event_readds_a_descendant_seen_earlier_in_the_batch() {
    let fx = fixture();
    let ancestor = fx
        .catalog
        .resolve_relative(fx.source, "proj")
        .unwrap()
        .unwrap();
    let descendant = fx
        .catalog
        .resolve_relative(fx.source, "proj/src/util/Helpers.cs")
        .unwrap()
        .unwrap();
    let now = UnixNanos::now();
    let rows = [
        eidos_catalog::jobs::OutboxRow {
            seq: 1,
            source_id: fx.source,
            object_id: descendant,
            op: "upsert".into(),
            generation: 1,
            created_at: now,
        },
        eidos_catalog::jobs::OutboxRow {
            seq: 2,
            source_id: fx.source,
            object_id: ancestor,
            op: "subtree".into(),
            generation: 1,
            created_at: now,
        },
    ];

    fx.index.apply_outbox(&fx.catalog, &rows).unwrap();
    fx.index.reload().unwrap();
    assert!(fx.names("name:=Helpers.cs").contains(&"Helpers.cs".into()));
}

#[test]
fn invalid_queries_error_cleanly() {
    let fx = fixture();
    let mut req = fx.req("name:/(unclosed/");
    req.explain = false;
    assert!(search(&fx.index, &fx.catalog, &req, &ExecOptions::default()).is_err());
    let req = SearchRequest {
        cursor: Some("garbage".into()),
        ..fx.req("ext:cs")
    };
    assert!(search(&fx.index, &fx.catalog, &req, &ExecOptions::default()).is_err());
    let req = SearchRequest {
        limit: 10_000,
        ..fx.req("ext:cs")
    };
    assert!(search(&fx.index, &fx.catalog, &req, &ExecOptions::default()).is_err());
}

#[test]
fn verification_clauses_are_rejected_inside_or_and_not() {
    let fx = fixture();
    for q in [
        "ext:cs -name:=Main.cs",
        "ext:cs OR name:~Main",
        "-name:/main/c",
        "-has:cs>=2",
    ] {
        let parsed = parse(q).unwrap();
        let req = SearchRequest::new(parsed.query);
        let err = search(&fx.index, &fx.catalog, &req, &ExecOptions::default())
            .err()
            .unwrap_or_else(|| panic!("{q} should be rejected"));
        assert!(err.to_string().contains("inside OR or NOT"), "{q}: {err}");
    }
    // Case-insensitive substring/glob/regex still work there, exactly.
    assert_eq!(fx.names("ext:cs -path:util"), vec!["Main.cs", "Program.cs"]);
    assert_eq!(
        fx.names("ext:cs -name:/^main/"),
        vec!["Helpers.cs", "Program.cs"]
    );
    assert_eq!(
        fx.names("(name:helpers OR name:/^main/) ext:cs"),
        vec!["Helpers.cs", "Main.cs"]
    );
}

#[test]
fn recreated_index_is_rebuilt_despite_recorded_generation() {
    let fx = fixture();
    assert!(fx.run("ext:cs").total.value > 0);
    let dir = fx.index.dir().to_path_buf();
    drop(fx.index.clone());
    // Simulate a schema bump: wipe the index files but keep the catalog's
    // projection state.
    let meta = dir.join("eidos-schema.json");
    std::fs::remove_file(&meta).unwrap();
    let reopened = CatalogIndex::open(&dir).unwrap();
    assert!(reopened.is_empty());
    let rebuilt = reopened.sync_sources(&fx.catalog).unwrap();
    assert_eq!(rebuilt.len(), 1, "recreated index must rebuild the source");
    reopened.reload().unwrap();
    assert!(reopened.num_docs() > 0);
}

#[test]
fn empty_index_with_recorded_documents_is_rebuilt() {
    let fx = fixture();
    let dir = fx.index.dir().to_path_buf();
    drop(fx.index.clone());
    // Wipe everything (meta included) so a fresh open creates a v-current
    // index with no "recreated" memory, then open twice: the second open
    // must still rebuild because the catalog record claims documents.
    for e in std::fs::read_dir(&dir).unwrap() {
        let e = e.unwrap();
        if e.file_type().unwrap().is_file() {
            std::fs::remove_file(e.path()).unwrap();
        }
    }
    let first = CatalogIndex::open(&dir).unwrap();
    assert!(first.take_recreated());
    drop(first);
    let second = CatalogIndex::open(&dir).unwrap();
    assert!(!second.take_recreated());
    let rebuilt = second.sync_sources(&fx.catalog).unwrap();
    assert_eq!(rebuilt.len(), 1);
    second.reload().unwrap();
    assert!(second.num_docs() > 0);
}

/// Beyond `LAZY_VERIFY_MIN` candidates, substring/regex clauses verify
/// lazily in sort order: hits are exact, totals may be upper bounds.
#[test]
fn large_candidate_sets_verify_lazily_in_sort_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    for i in 0..2_600u32 {
        // "item" names plus decoys that share trigrams but not the substring.
        let name = if i % 5 == 0 {
            format!("sub{i}/ite_m-{i:04}.txt")
        } else {
            format!("sub{}/item-{i:04}.txt", i % 7)
        };
        write(&root.join(name), 1 + (i % 13) as usize);
    }
    let catalog = Catalog::open(dir.path().join("catalog.db")).unwrap();
    let host = catalog.ensure_host("h", "windows").unwrap();
    let source = catalog
        .add_source(&NewSource {
            host_id: host,
            name: "lazy".into(),
            kind: SourceKind::WindowsGeneric,
            root_path: root.display().to_string(),
            aliases: vec![],
        })
        .unwrap();
    let lister = eidos_scanner::default_lister();
    run_scan(
        &catalog,
        source,
        lister.as_ref(),
        &RunScanOptions::default(),
    )
    .unwrap();
    let index = CatalogIndex::open(dir.path().join("index")).unwrap();
    index.sync_sources(&catalog).unwrap();
    index.reload().unwrap();
    let run = |q: &str, sort: SortField, desc: bool, limit: u32, cursor: Option<String>| {
        let parsed = parse(q).unwrap();
        let mut r = SearchRequest::new(parsed.query);
        r.sort = Sort {
            field: sort,
            descending: desc,
        };
        r.limit = limit;
        r.cursor = cursor;
        r.explain = true;
        search(&index, &catalog, &r, &ExecOptions::default()).unwrap()
    };
    // 2,080 real matches among 2,600 trigram candidates ("ite_m" shares
    // "ite" but not "item").
    let r = run("name:item", SortField::Name, false, 10, None);
    assert_eq!(r.hits.len(), 10);
    assert!(r.hits.iter().all(|h| h.name.contains("item")));
    assert!(!r.total.exact, "upper bound expected: {:?}", r.total);
    assert!(r.total.value >= 2_080);
    let names: Vec<&str> = r.hits.iter().map(|h| h.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted, "name order across segments");
    assert_eq!(names[0], "item-0001.txt");
    // Second page continues the order.
    let r2 = run(
        "name:item",
        SortField::Name,
        false,
        10,
        r.next_cursor.clone(),
    );
    assert_eq!(r2.hits[0].name, "item-0013.txt");
    // Descending size order with lazy verification.
    let r = run("name:item", SortField::Size, true, 5, None);
    assert_eq!(r.hits.len(), 5);
    assert!(r.hits.iter().all(|h| h.size == 13));
    assert!(r.hits.iter().all(|h| h.name.contains("item")));
    // Walking to the end makes the total exact.
    let mut cursor = None;
    let mut seen = 0usize;
    let last = loop {
        let r = run("name:item", SortField::Name, false, 1_000, cursor.take());
        seen += r.hits.len();
        if r.next_cursor.is_none() {
            break r;
        }
        cursor = r.next_cursor.clone();
    };
    assert_eq!(seen, 2_080);
    assert!(last.total.exact, "{:?}", last.total);
    assert_eq!(last.total.value, 2_080);
    // Small sets stay eager and exact.
    let r = run("name:ite_m", SortField::Name, false, 10, None);
    assert!(r.total.exact);
    assert_eq!(r.total.value, 520);
}

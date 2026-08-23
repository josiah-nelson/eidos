//! `eidos activity`: indexing activity from the running service.

use clap::Args;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct ActivityArgs {
    /// Service base URL.
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    /// Print raw JSON.
    #[arg(long)]
    json: bool,
    /// Refresh every N seconds until interrupted.
    #[arg(long)]
    watch: Option<u64>,
}

pub fn run(args: ActivityArgs) -> anyhow::Result<()> {
    loop {
        let value: serde_json::Value = ureq::get(format!("{}/api/activity", args.url))
            .call()?
            .body_mut()
            .read_json()?;
        if args.json {
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            print_activity(&value);
        }
        match args.watch {
            Some(secs) => std::thread::sleep(Duration::from_secs(secs.max(1))),
            None => return Ok(()),
        }
        println!();
    }
}

fn g<'a>(v: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
    let mut cur = v;
    for p in path.split('.') {
        cur = &cur[p];
    }
    cur
}

fn n(v: &serde_json::Value, path: &str) -> u64 {
    g(v, path).as_u64().unwrap_or(0)
}

fn print_activity(v: &serde_json::Value) {
    let w = &v["workers"];
    println!(
        "content: {}  workers: {}  throughput: {}/s  indexed: {} files / {}  unsupported: {}  failed: {}  retried: {}",
        if v["content_enabled"].as_bool().unwrap_or(false) { "on" } else { "off" },
        n(w, "workers"),
        crate::profile::human_bytes(w["throughput_bytes_per_s"].as_f64().unwrap_or(0.0) as u64),
        n(w, "files_indexed"),
        crate::profile::human_bytes(n(w, "bytes_read")),
        n(w, "files_unsupported"),
        n(w, "files_failed"),
        n(w, "files_retried"),
    );
    println!(
        "jobs: queued {}  running {}  failed {}  oldest queued {} s   commits {}  pending publish {}  uncommitted docs {}  content index docs {}",
        n(v, "jobs.queued"),
        n(v, "jobs.running"),
        n(v, "jobs.failed"),
        n(v, "jobs.oldest_queued_age_ms") / 1000,
        n(w, "commits"),
        n(w, "pending_publish"),
        n(w, "uncommitted_documents"),
        n(v, "content_index_documents"),
    );
    if let Some(e) = w["last_error"].as_str() {
        println!("last error: {e}");
    }
    let rb = &v["content_rebuild"];
    if let Some(phase) = rb["phase"].as_str().filter(|p| *p != "idle") {
        println!(
            "content index rebuild: {phase}  {} / {} docs  {} s{}",
            n(rb, "docs"),
            n(rb, "chunks"),
            n(rb, "elapsed_ms") / 1000,
            rb["error"]
                .as_str()
                .map(|e| format!("  error: {e}"))
                .unwrap_or_default()
        );
    }
    if let Some(sources) = v["sources"].as_array() {
        println!(
            "{:<4} {:<16} {:<18} {:<8} {:>7} {:>7}  states",
            "id", "name", "state", "content", "queued", "running"
        );
        for s in sources {
            let states = s["content_states"]
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            println!(
                "{:<4} {:<16} {:<18} {:<8} {:>7} {:>7}  {} ({} indexed)",
                n(s, "source_id"),
                s["name"].as_str().unwrap_or(""),
                s["state"].as_str().unwrap_or(""),
                if s["content_enabled"].as_bool().unwrap_or(false) {
                    format!(
                        "on {}/{}",
                        n(s, "content_reserved"),
                        n(s, "content_concurrency")
                    )
                } else {
                    "off".into()
                },
                n(s, "jobs_queued"),
                n(s, "jobs_running"),
                states,
                crate::profile::human_bytes(n(s, "content_bytes_indexed")),
            );
            if let Some(deferred) = s["reconciliation_deferred"].as_object() {
                println!(
                    "     automatic rescan deferred: {} (next check {})",
                    deferred["reason"].as_str().unwrap_or("busy"),
                    deferred["next_eligible_at"].as_str().unwrap_or("unknown")
                );
            }
        }
    }
    if let Some(cur) = w["current"].as_array() {
        for c in cur {
            println!(
                "  {} object {} ({}) {} s",
                c["worker"].as_str().unwrap_or(""),
                n(c, "object_id"),
                crate::profile::human_bytes(n(c, "size")),
                n(c, "started_ms_ago") / 1000
            );
        }
    }
    if let Some(f) = v["recent_failures"].as_array() {
        for j in f.iter().take(5) {
            println!(
                "  failed job {} object {:?}: {}",
                n(j, "id"),
                j["object_id"],
                j["last_error"].as_str().unwrap_or("")
            );
        }
    }
}

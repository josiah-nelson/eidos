//! Runtime admission ceilings; all values are explicit on replacement.

use anyhow::Context;
use clap::Args;

#[derive(Args, Debug)]
pub struct ResourceArgs {
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    /// Threads per new metadata enumeration (1..64). Supply all three limits to save.
    #[arg(long, requires_all = ["concurrent_scans", "minimum_free_mib"], value_parser = clap::value_parser!(u32).range(1..=64))]
    scan_threads: Option<u32>,
    /// Metadata scans allowed at once, including replay/publication (1..16).
    #[arg(long, requires_all = ["scan_threads", "minimum_free_mib"], value_parser = clap::value_parser!(u32).range(1..=16))]
    concurrent_scans: Option<u32>,
    /// Free MiB reserved on the data volume; zero disables this check.
    #[arg(long, requires_all = ["scan_threads", "concurrent_scans"])]
    minimum_free_mib: Option<u32>,
    /// Show process memory and configured cache/index budgets instead of limits.
    #[arg(long, conflicts_with_all = ["scan_threads", "concurrent_scans", "minimum_free_mib"])]
    memory: bool,
    /// Show shared-device reader limits, membership, reservations and probe errors.
    #[arg(long, conflicts_with_all = ["memory", "scan_threads", "concurrent_scans", "minimum_free_mib"])]
    devices: bool,
    /// Save readers per backing device (1..64). Separate from source/pool limits.
    #[arg(long, conflicts_with_all = ["memory", "scan_threads", "concurrent_scans", "minimum_free_mib"], value_parser = clap::value_parser!(u32).range(1..=64))]
    device_readers: Option<u32>,
    #[arg(long)]
    json: bool,
}

pub fn run(args: ResourceArgs) -> anyhow::Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let devices = args.devices || args.device_readers.is_some();
    let url = format!(
        "{}/api/{}",
        args.url.trim_end_matches('/'),
        if args.memory {
            "memory"
        } else if devices {
            "devices"
        } else {
            "resources"
        }
    );
    let mut response = if let Some(readers_per_device) = args.device_readers {
        agent
            .post(&url)
            .send_json(serde_json::json!({ "readers_per_device": readers_per_device }))?
    } else if let (Some(scan_threads), Some(concurrent_scans), Some(minimum_free_mib)) = (
        args.scan_threads,
        args.concurrent_scans,
        args.minimum_free_mib,
    ) {
        agent
            .post(&url)
            .send_json(serde_json::json!({ "scan_threads": scan_threads,
            "concurrent_scans": concurrent_scans, "minimum_free_mib": minimum_free_mib }))?
    } else {
        agent.get(&url).call()?
    };
    let status = response.status();
    // Report the status even when the body is not the JSON API error shape
    // (a wrong URL, a proxy, or a build without this route).
    let body = response.body_mut().read_json::<serde_json::Value>();
    anyhow::ensure!(
        status.is_success(),
        "{}: {}",
        status,
        body.as_ref()
            .ok()
            .and_then(|body| body["error"].as_str())
            .unwrap_or(if args.memory {
                "memory diagnostics request failed"
            } else if devices {
                "shared-device request failed"
            } else {
                "resource request failed"
            })
    );
    let body = body.context(if args.memory {
        "read memory diagnostics"
    } else if devices {
        "read shared-device limits"
    } else {
        "read resource limits"
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else if devices {
        println!(
            "reader limit per device: {}  shared unknown fallback: {}  topology draining: {}",
            body["budget"]["readers_per_device"],
            body["budget"]["unresolved_shared_fallback"],
            body["budget"]["topology_draining"]
        );
        println!(
            "topology sample age (s): {}  stale: {}",
            body["sample_age_s"].as_str().unwrap_or("unavailable"),
            body["stale"]
        );
        if let Some(rows) = body["budget"]["devices"].as_array() {
            for device in rows {
                println!(
                    "{}: content readers {}  scan threads {}  peak {}  sources {}",
                    device["key"].as_str().unwrap_or("unknown"),
                    device["content_readers"],
                    device["scan_threads"],
                    device["peak_readers"],
                    device["sources"]
                );
            }
        }
        if let Some(errors) = body["source_errors"].as_object() {
            for (source, error) in errors {
                println!(
                    "source {source} topology error: {}",
                    error.as_str().unwrap_or("topology unavailable")
                );
            }
        }
        if let Some(roots) = body["source_roots"].as_object() {
            for (source, root) in roots {
                println!(
                    "source {source} root: {}",
                    root.as_str().unwrap_or("unknown root")
                );
            }
        }
        println!("Covers scan enumerators and content readers, not writers, native feeds, query I/O or the single background topology probe. OS disks may hide shared RAID/virtual storage.");
    } else if args.memory {
        let value = |v: &serde_json::Value| v.as_str().unwrap_or("unavailable").to_owned();
        // Name the process these counters describe: `--url` may address another
        // machine's service, and only some counters exist on every platform.
        println!(
            "sampled process: {}",
            body["process"]["pid"]
                .as_u64()
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "no sample yet".to_owned())
        );
        println!(
            "resident bytes: {}  peak resident bytes: {}  private committed bytes: {}",
            value(&body["process"]["resident_bytes"]),
            value(&body["process"]["peak_resident_bytes"]),
            value(&body["process"]["private_commit_bytes"])
        );
        println!(
            "sample age (s): {}  stale: {}",
            value(&body["sample_age_s"]),
            body["stale"]
        );
        println!("catalog baseline connections: {}  page-cache target bytes each: {}  baseline target bytes: {}",
            body["catalog"]["baseline_connections"], value(&body["catalog"]["page_cache_per_connection_bytes"]), value(&body["catalog"]["page_cache_baseline_target_bytes"]));
        println!("mapped file limit per connection: {}  name-index writer budget: {}  content-index writer budget: {}",
            value(&body["catalog"]["mmap_per_connection_limit_bytes"]), value(&body["catalog_writer_budget_bytes"]), value(&body["content_writer_budget_bytes"]));
        println!("Budgets are not allocated RAM or a hard process limit; scan connections and other allocations are additional.");
        if let Some(error) = body["error"].as_str() {
            println!("memory sample unavailable: {error}");
        }
    } else {
        let limits = &body["limits"];
        println!(
            "metadata threads: {}  concurrent scans: {}  data-volume reserve: {} MiB",
            limits["scan_threads"], limits["concurrent_scans"], limits["minimum_free_mib"]
        );
        // 64-bit values use the API's decimal-string convention; print the
        // number, not a quoted JSON string.
        println!(
            "active metadata scans: {}  free bytes: {}",
            body["active_scans"],
            body["free_bytes"].as_str().unwrap_or("unknown")
        );
        if let Some(reason) = body["admission_blocked"].as_str() {
            println!("waiting: {reason}");
        }
    }
    Ok(())
}

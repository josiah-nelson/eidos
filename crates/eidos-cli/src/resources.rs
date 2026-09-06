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
    #[arg(long)]
    json: bool,
}

pub fn run(args: ResourceArgs) -> anyhow::Result<()> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let url = format!("{}/api/resources", args.url.trim_end_matches('/'));
    let mut response = if let (Some(scan_threads), Some(concurrent_scans), Some(minimum_free_mib)) = (
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
    let body: serde_json::Value = response
        .body_mut()
        .read_json()
        .context("read resource limits")?;
    anyhow::ensure!(
        status.is_success(),
        "{}: {}",
        status,
        body["error"].as_str().unwrap_or("resource request failed")
    );
    if args.json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        let limits = &body["limits"];
        println!(
            "metadata threads: {}  concurrent scans: {}  data-volume reserve: {} MiB",
            limits["scan_threads"], limits["concurrent_scans"], limits["minimum_free_mib"]
        );
        println!(
            "active metadata scans: {}  free bytes: {}",
            body["active_scans"], body["free_bytes"]
        );
        if let Some(reason) = body["admission_blocked"].as_str() {
            println!("waiting: {reason}");
        }
    }
    Ok(())
}

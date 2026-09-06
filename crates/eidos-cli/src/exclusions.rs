//! Versioned policy controls through the running service; never opens the catalog.
use clap::{Args, Subcommand};
use eidos_catalog::exclusions::ExclusionRule;
use std::{io::Read, path::PathBuf};

#[derive(Args, Debug)]
pub struct ExclusionArgs {
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    /// Source ID from `eidos source list` or the Sources page.
    source: i64,
    #[command(subcommand)]
    command: ExclusionCommand,
}

#[derive(Subcommand, Debug)]
enum ExclusionCommand {
    /// Show rules, immutable protection, progress and error as JSON.
    Status,
    /// Validate a JSON rule array and preview relative paths without saving.
    Preview { rules: PathBuf, paths: Vec<String> },
    /// Apply a JSON rule array; revision must match Status (optimistic concurrency).
    Apply {
        rules: PathBuf,
        #[arg(long)]
        revision: u32,
    },
    /// Retry a stopped application after resolving its error.
    Retry,
}

fn read_rules(path: &PathBuf) -> anyhow::Result<Vec<ExclusionRule>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(512 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() <= 512 * 1024, "rule file exceeds 512 KiB");
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn run(args: ExclusionArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.source > 0, "source ID must be positive");
    let base = format!(
        "{}/api/sources/{}/policy",
        args.url.trim_end_matches('/'),
        args.source
    );
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(15)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = match args.command {
        ExclusionCommand::Status => agent.get(&base).call()?,
        ExclusionCommand::Retry => agent
            .post(&format!("{base}/retry"))
            .send_json(serde_json::json!({}))?,
        ExclusionCommand::Preview { rules, paths } => agent
            .post(&format!("{base}/preview"))
            .send_json(serde_json::json!({ "rules": read_rules(&rules)?, "paths": paths }))?,
        ExclusionCommand::Apply { rules, revision } => agent.post(&base).send_json(
            serde_json::json!({ "rules": read_rules(&rules)?, "expected_revision": revision }),
        )?,
    };
    let status = response.status();
    let body: serde_json::Value = response.body_mut().read_json()?;
    anyhow::ensure!(
        status.is_success(),
        "{status}: {}",
        body["error"].as_str().unwrap_or("policy request failed")
    );
    println!("{}", serde_json::to_string_pretty(&body)?);
    Ok(())
}

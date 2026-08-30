//! `eidos content`: operator controls for content work in the running
//! service — pausing and resuming extraction, and retrying failures.

use anyhow::{bail, Context};
use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct ContentArgs {
    /// Service base URL.
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    #[command(subcommand)]
    command: ContentCommand,
}

#[derive(Subcommand, Debug)]
enum ContentCommand {
    /// Report what extraction is doing and how complete content search is.
    Status {
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Stop claiming new content jobs.
    ///
    /// Batches already claimed finish, commit, and publish normally, so
    /// nothing in flight is abandoned. The queue is left alone and the pause
    /// survives a service restart — `eidos content resume` is the only way
    /// back.
    Pause {
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Claim content jobs again after a pause.
    Resume {
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Show or set the global extraction worker pool.
    ///
    /// The pool is shared across every source; per-volume concurrency caps
    /// apply on top. A new size takes effect at once and survives a
    /// restart (it is persisted next to the catalog and overrides
    /// `--content-workers`).
    Workers {
        /// New pool size (clamped to 1..=64); omit to show the current one.
        count: Option<u32>,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Requeue failed content work: one job by id, or a whole source.
    ///
    /// Transient failures already retry on their own; this is for the
    /// terminal ones (deterministic, corrupt, unsupported, resource_limit)
    /// after the underlying problem is fixed. A source retry also covers
    /// objects whose extraction failed for good, whose job already
    /// finished.
    Retry {
        /// Job id (from `eidos activity` or the Activity page).
        job: Option<i64>,
        /// Source name or id: retry that source's failed content jobs.
        #[arg(long, conflicts_with = "job")]
        source: Option<String>,
        /// Only failures of this class (transient, unsupported,
        /// deterministic, resource_limit, corrupt).
        #[arg(long, requires = "source")]
        class: Option<String>,
        /// Only failures whose error message starts with this text.
        #[arg(long, requires = "source")]
        reason_prefix: Option<String>,
        /// Requeue at most this many (pass a preview's count to bound the
        /// action to what it reported).
        #[arg(long, requires = "source")]
        limit: Option<u32>,
        /// Report what would be retried without changing anything.
        #[arg(long)]
        preview: bool,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(args: ContentArgs) -> anyhow::Result<()> {
    match args.command {
        ContentCommand::Status { json } => {
            let value: serde_json::Value = ureq::get(format!("{}/api/content/status", args.url))
                .call()
                .context("read content status")?
                .body_mut()
                .read_json()?;
            print_status(&value, json)?;
        }
        ContentCommand::Pause { json } => switch(&args.url, "pause", json)?,
        ContentCommand::Resume { json } => switch(&args.url, "resume", json)?,
        ContentCommand::Workers { count, json } => {
            let url = format!("{}/api/content/workers", args.url);
            let value: serde_json::Value = match count {
                Some(n) => ureq::post(&url)
                    .send_json(serde_json::json!({ "workers": n }))
                    .context("resize the worker pool")?
                    .body_mut()
                    .read_json()?,
                None => ureq::get(&url)
                    .call()
                    .context("read the worker pool size")?
                    .body_mut()
                    .read_json()?,
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "workers: {}  (max {})",
                    plain(&value["workers"]),
                    plain(&value["max"])
                );
            }
        }
        ContentCommand::Retry {
            job,
            source,
            class,
            reason_prefix,
            limit,
            preview,
            json,
        } => {
            let mut body = serde_json::Map::new();
            body.insert("preview".into(), preview.into());
            let url = match (job, source) {
                (Some(id), _) => format!("{}/api/jobs/{id}/retry", args.url),
                (None, Some(name)) => {
                    let sid = resolve_source(&args.url, &name)?;
                    if let Some(c) = class {
                        body.insert("class".into(), c.into());
                    }
                    if let Some(p) = reason_prefix {
                        body.insert("reason_prefix".into(), p.into());
                    }
                    if let Some(l) = limit {
                        body.insert("limit".into(), l.into());
                    }
                    format!("{}/api/sources/{sid}/content/retry", args.url)
                }
                (None, None) => bail!("pass a job id or --source NAME"),
            };
            let value: serde_json::Value = ureq::post(&url)
                .send_json(serde_json::Value::Object(body))?
                .body_mut()
                .read_json()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_report(&value);
            }
        }
    }
    Ok(())
}

/// Integers arrive as JSON numbers or, for large-int fields, as decimal
/// strings; print either without quotes.
fn plain(v: &serde_json::Value) -> String {
    v.as_str()
        .map(String::from)
        .unwrap_or_else(|| v.to_string())
}

/// Flip the pause and report the state that came back, so the operator sees
/// in one step whether workers stopped outright or are still draining.
fn switch(base: &str, action: &str, json: bool) -> anyhow::Result<()> {
    let value: serde_json::Value = ureq::post(format!("{base}/api/content/{action}"))
        .send_empty()
        .with_context(|| format!("{action} content extraction"))?
        .body_mut()
        .read_json()?;
    print_status(&value, json)
}

fn print_status(v: &serde_json::Value, json: bool) -> anyhow::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(v)?);
        return Ok(());
    }
    println!(
        "content: {}  search: {}",
        v["flow"].as_str().unwrap_or("unknown"),
        v["search"].as_str().unwrap_or("unknown"),
    );
    if let Some(detail) = v["detail"].as_str() {
        println!("  {detail}");
    }
    Ok(())
}

/// Accept a source id or a source name (matched against `/api/sources`).
fn resolve_source(base: &str, name: &str) -> anyhow::Result<i64> {
    if let Ok(id) = name.parse::<i64>() {
        return Ok(id);
    }
    let sources: serde_json::Value = ureq::get(format!("{base}/api/sources"))
        .call()
        .context("list sources")?
        .body_mut()
        .read_json()?;
    sources
        .as_array()
        .into_iter()
        .flatten()
        .find(|s| s["source"]["name"].as_str() == Some(name))
        .and_then(|s| s["source"]["id"].as_i64())
        .with_context(|| format!("no source named '{name}'"))
}

fn print_report(v: &serde_json::Value) {
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let reasons = |k: &str| {
        v[k].as_object()
            .map(|m| {
                m.iter()
                    .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    };
    println!(
        "{} {} job(s), {} — skipped {} rejected {}",
        if v["preview"].as_bool().unwrap_or(false) {
            "would requeue"
        } else {
            "requeued"
        },
        n("accepted"),
        crate::profile::human_bytes(n("bytes")),
        n("skipped"),
        n("rejected"),
    );
    let skipped = reasons("skipped_reasons");
    if !skipped.is_empty() {
        println!("  skipped: {skipped}");
    }
    let rejected = reasons("rejected_reasons");
    if !rejected.is_empty() {
        println!("  rejected: {rejected}");
    }
}

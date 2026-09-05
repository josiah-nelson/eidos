//! `eidos fleet ...`: identity, master role, approval-based joining, and
//! sync status.
//!
//! Commands talk to the running service over the loopback API so the
//! service's own runtime picks changes up at once. The ones that make sense
//! without a running service (`identity`, `master`, `join`) fall back to
//! working on the data directory directly when the service does not answer.

use anyhow::{anyhow, Context};
use clap::{Args, Subcommand};
use eidos_catalog::Catalog;
use eidos_domain::SyncPolicy;
use eidos_fleet::enroll::JoinOutcome;
use eidos_fleet::{FleetConfig, FleetStatus, NodeIdentity};
use serde::de::DeserializeOwned;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct FleetArgs {
    /// Service base URL.
    #[arg(
        long,
        env = "EIDOS_URL",
        default_value = "http://127.0.0.1:7700",
        global = true
    )]
    url: String,
    /// Data directory, for commands that can work without the service.
    #[arg(long, env = "EIDOS_DATA_DIR", default_value = "data", global = true)]
    data_dir: PathBuf,
    /// Print raw JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: FleetCommand,
}

#[derive(Subcommand, Debug)]
pub enum FleetCommand {
    /// Peers, sessions, per-source cursors, backlog, and counters.
    Status,
    /// This installation's node id, name, and certificate fingerprint.
    Identity,
    /// Make this node the master, or disable its master role.
    #[command(alias = "central")]
    Master {
        /// Listen address for the dedicated sync endpoint, e.g. 0.0.0.0:7710.
        #[arg(long, conflicts_with = "no_listen")]
        listen: Option<String>,
        /// Stop accepting joins and replicas (the listener stays).
        #[arg(long)]
        disable: bool,
        /// Stop listening.
        #[arg(long)]
        no_listen: bool,
    },
    /// Ask a master, by IP address or discovered host, to approve this node.
    Join { master: String },
    /// Cancel the local pending or rejected join attempt.
    CancelJoin,
    /// Approve a pending node request (master only).
    Approve { request: String },
    /// Reject a pending node request (master only).
    Reject { request: String },
    /// Stop transferring without forgetting the master (resume later without a resync).
    Pause,
    /// Resume transfer after `pause`.
    Resume,
    /// Leave the fleet: forget the master and drop the ledgers.
    Leave,
    /// Maintain a peer: set the endpoint this side dials, enable/disable, or forget it.
    Peer {
        /// Node id (32 hex characters).
        node: String,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long)]
        disable: bool,
        /// Forget the node and retire its replicated sources (master only).
        #[arg(
            long,
            conflicts_with_all = ["endpoint", "enable", "disable"]
        )]
        forget: bool,
    },
    /// Keep a source on this host only, or let it follow fleet membership again.
    Policy {
        /// Source name.
        source: String,
        #[arg(long, conflicts_with = "inherit")]
        local_only: bool,
        #[arg(long)]
        inherit: bool,
    },
}

fn agent() -> ureq::Agent {
    ureq::config::Config::builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn api_error(status: u16, body: &str) -> anyhow::Error {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| body.to_string());
    anyhow!("service answered {status}: {message}")
}

fn get<T: DeserializeOwned>(url: &str, path: &str) -> anyhow::Result<T> {
    let mut response = agent()
        .get(format!("{}{path}", url.trim_end_matches('/')))
        .call()
        .with_context(|| format!("connecting to {url}"))?;
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string()?;
    if status >= 300 {
        return Err(api_error(status, &body));
    }
    Ok(serde_json::from_str(&body)?)
}

fn send<T: DeserializeOwned>(
    url: &str,
    method: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<T> {
    let full = format!("{}{path}", url.trim_end_matches('/'));
    let agent = agent();
    let mut response = match method {
        "DELETE" => agent.delete(&full).call(),
        _ => agent.post(&full).send_json(body),
    }
    .with_context(|| format!("connecting to {url}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string()?;
    if status >= 300 {
        return Err(api_error(status, &text));
    }
    Ok(serde_json::from_str(&text)?)
}

fn service_reachable(url: &str) -> bool {
    get::<serde_json::Value>(url, "/api/health").is_ok()
}

fn api_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
}

fn source_id_by_name(sources: &[serde_json::Value], name: &str) -> Option<i64> {
    sources
        .iter()
        .find(|source| source["source"]["name"].as_str() == Some(name))
        .and_then(|source| api_i64(&source["source"]["id"]))
}

pub fn run(args: FleetArgs) -> anyhow::Result<()> {
    let url = args.url.clone();
    match args.command {
        FleetCommand::Status => {
            let status: FleetStatus = get(&url, "/api/fleet")?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
        }
        FleetCommand::Identity => {
            if service_reachable(&url) {
                let status: FleetStatus = get(&url, "/api/fleet")?;
                println!("node id:      {}", status.node_id);
                println!("name:         {}", status.name);
                println!("fingerprint:  {}", status.fingerprint);
                println!(
                    "role:         {}",
                    if status.central { "master" } else { "node" }
                );
            } else {
                let identity =
                    NodeIdentity::load_or_create(&args.data_dir, &eidos_domain::bench::hostname())?;
                println!("node id:      {}", identity.node_id);
                println!("name:         {}", identity.name);
                println!("fingerprint:  {}", identity.fingerprint_hex());
                println!(
                    "(service not running; read from {})",
                    args.data_dir.display()
                );
            }
        }
        FleetCommand::Master {
            listen,
            disable,
            no_listen,
        } => {
            if service_reachable(&url) {
                let body = serde_json::json!({
                    "central": !disable,
                    "listen": if no_listen { Some(String::new()) } else { listen.clone() },
                });
                let config: FleetConfig = send(&url, "POST", "/api/fleet/central", &body)?;
                println!(
                    "master: {}  listen: {}",
                    config.central,
                    config.listen.as_deref().unwrap_or("(none)")
                );
            } else {
                if let Some(l) = listen.as_deref() {
                    l.parse::<std::net::SocketAddr>()
                        .map_err(|e| anyhow!("listen address: {e}"))?;
                }
                let config = FleetConfig::edit_locked(&args.data_dir, move |config| {
                    if disable {
                        config.central = false;
                    } else {
                        if config.pending_join.is_some() {
                            return Err(anyhow!(
                                "cancel the pending join before making this node the master"
                            ));
                        }
                        config.central = true;
                        if listen.is_none() && config.listen.is_none() && !no_listen {
                            config.listen = Some(format!(
                                "0.0.0.0:{}",
                                eidos_fleet::config::DEFAULT_SYNC_PORT
                            ));
                        }
                    }
                    if no_listen {
                        config.listen = None;
                    } else if let Some(l) = listen {
                        config.listen = Some(l);
                    }
                    Ok(())
                })?;
                println!(
                    "master: {}  listen: {}  (written to {}; takes effect when the service starts or on its next tick)",
                    config.central,
                    config.listen.as_deref().unwrap_or("(none)"),
                    FleetConfig::path(&args.data_dir).display()
                );
            }
        }
        FleetCommand::Join { master } => {
            if service_reachable(&url) {
                let status: FleetStatus = send(
                    &url,
                    "POST",
                    "/api/fleet/join",
                    &serde_json::json!({ "master": master }),
                )?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    let pending = status
                        .pending_join
                        .as_ref()
                        .ok_or_else(|| anyhow!("join completed without a pending target"))?;
                    println!(
                        "waiting for {} to approve this node (request {})",
                        pending.master_name, pending.request_id
                    );
                }
            } else {
                if FleetConfig::load(&args.data_dir)?.central {
                    return Err(anyhow!("a fleet master cannot join another master"));
                }
                let catalog = Arc::new(Catalog::open(args.data_dir.join("catalog.db"))?);
                let identity =
                    NodeIdentity::load_or_create(&args.data_dir, &eidos_domain::bench::hostname())?;
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let outcome = rt.block_on(eidos_fleet::enroll::request_join(
                    &catalog,
                    &identity,
                    &master,
                    Duration::from_secs(15),
                ))?;
                match outcome {
                    JoinOutcome::Pending(target) => {
                        let shown = target.clone();
                        FleetConfig::edit_locked(&args.data_dir, move |config| {
                            config.pending_join = Some(target);
                            Ok(())
                        })?;
                        println!(
                            "waiting for {} to approve this node (request {}); the service will complete the join",
                            shown.master_name, shown.request_id
                        );
                    }
                    JoinOutcome::Joined(joined) => println!(
                        "joined master {} ({}) at {}",
                        joined.master_name, joined.master, joined.endpoint
                    ),
                    JoinOutcome::Rejected(reason) => {
                        return Err(anyhow!("the master rejected the join request: {reason}"))
                    }
                }
            }
        }
        FleetCommand::CancelJoin => {
            if service_reachable(&url) {
                let _: FleetStatus =
                    send(&url, "DELETE", "/api/fleet/join", &serde_json::json!({}))?;
            } else {
                FleetConfig::edit_locked(&args.data_dir, |config| {
                    config.pending_join = None;
                    Ok(())
                })?;
            }
            println!("pending join cleared");
        }
        FleetCommand::Approve { request } => {
            let status: FleetStatus = send(
                &url,
                "POST",
                &format!("/api/fleet/join-requests/{request}"),
                &serde_json::json!({ "approve": true }),
            )?;
            println!(
                "approved request {request}; {} request(s) still waiting",
                status.join_requests.len()
            );
        }
        FleetCommand::Reject { request } => {
            let status: FleetStatus = send(
                &url,
                "POST",
                &format!("/api/fleet/join-requests/{request}"),
                &serde_json::json!({ "approve": false }),
            )?;
            println!(
                "rejected request {request}; {} request(s) still waiting",
                status.join_requests.len()
            );
        }
        FleetCommand::Pause => {
            let status: FleetStatus = send(
                &url,
                "POST",
                "/api/fleet/sync",
                &serde_json::json!({ "enabled": false }),
            )?;
            println!("sync paused (enrolled: {})", status.enrolled);
        }
        FleetCommand::Resume => {
            let status: FleetStatus = send(
                &url,
                "POST",
                "/api/fleet/sync",
                &serde_json::json!({ "enabled": true }),
            )?;
            println!("sync resumed (enrolled: {})", status.enrolled);
        }
        FleetCommand::Leave => {
            let status: FleetStatus =
                send(&url, "POST", "/api/fleet/leave", &serde_json::json!({}))?;
            println!("left the fleet (enrolled: {})", status.enrolled);
        }
        FleetCommand::Peer {
            node,
            endpoint,
            enable,
            disable,
            forget,
        } => {
            if forget {
                let view: serde_json::Value = send(
                    &url,
                    "DELETE",
                    &format!("/api/fleet/peers/{node}"),
                    &serde_json::json!({}),
                )?;
                println!(
                    "forgot {node}; retired {} replicated source(s)",
                    view["retired_sources"]
                );
            } else {
                let body = serde_json::json!({
                    "endpoint": endpoint,
                    "enabled": if enable { Some(true) } else if disable { Some(false) } else { None },
                });
                let view: serde_json::Value =
                    send(&url, "POST", &format!("/api/fleet/peers/{node}"), &body)?;
                println!("{}", serde_json::to_string_pretty(&view)?);
            }
        }
        FleetCommand::Policy {
            source,
            local_only,
            inherit,
        } => {
            if !local_only && !inherit {
                return Err(anyhow!("choose --local-only or --inherit"));
            }
            let sources: Vec<serde_json::Value> = get(&url, "/api/sources")?;
            let id = source_id_by_name(&sources, &source)
                .ok_or_else(|| anyhow!("no source named {source}"))?;
            let policy = if local_only {
                SyncPolicy::LocalOnly
            } else {
                SyncPolicy::Inherit
            };
            let view: serde_json::Value = send(
                &url,
                "POST",
                &format!("/api/sources/{id}/sync-policy"),
                &serde_json::json!({ "policy": policy.as_str() }),
            )?;
            println!(
                "{}: sync policy {}",
                source,
                view["source"]["sync_policy"].as_str().unwrap_or_default()
            );
        }
    }
    Ok(())
}

fn print_status(s: &FleetStatus) {
    println!(
        "{} ({}) {} fingerprint {}",
        s.name,
        s.node_id,
        if s.central { "master" } else { "node" },
        s.fingerprint
    );
    println!(
        "joined: {}  sync: {}  listening: {}",
        s.enrolled,
        if s.sync_enabled { "on" } else { "off" },
        s.listening.as_deref().unwrap_or("no")
    );
    if let Some(join) = &s.pending_join {
        println!(
            "join request {} -> {} at {}{}",
            join.request_id,
            join.master_name,
            join.endpoint,
            join.rejected_reason
                .as_deref()
                .map(|reason| format!(" (rejected: {reason})"))
                .unwrap_or_default()
        );
    }
    if !s.join_requests.is_empty() {
        println!("join requests waiting for approval:");
        for request in &s.join_requests {
            println!(
                "  {} {} ({}, {}) from {}",
                request.request_id,
                request.name,
                request.platform,
                request.node_id,
                request.remote_addr.as_deref().unwrap_or("unknown address")
            );
        }
    }
    if !s.discovered_masters.is_empty() {
        println!("masters discovered on this network:");
        for master in &s.discovered_masters {
            println!(
                "  {} ({}) {}",
                master.name,
                master.node_id,
                master.endpoints.join(", ")
            );
        }
    }
    for d in &s.degraded {
        println!("DEGRADED: {d}");
    }
    if !s.peers.is_empty() {
        println!("peers:");
        for p in &s.peers {
            println!(
                "  {} {} [{}] {}{} endpoint={}{}",
                p.node_id,
                p.name,
                p.role,
                if p.connected {
                    "connected"
                } else {
                    "disconnected"
                },
                if p.enabled { "" } else { " (disabled)" },
                p.endpoint.as_deref().unwrap_or("-"),
                p.last_error
                    .as_deref()
                    .map(|e| format!(" last error: {e}"))
                    .unwrap_or_default()
            );
        }
    }
    for session in &s.sessions {
        println!(
            "session with {} ({:?}, {} ms since activity, credit {} bytes):",
            session.peer_name,
            session.direction,
            session.last_activity_ms_ago,
            session.credit_remaining
        );
        for src in &session.sources {
            println!(
                "  source {} {:?}: {} cursor {}/{} batches {} rows {}{}",
                src.source_id,
                src.role,
                src.phase,
                src.cursor,
                src.head,
                src.batches,
                src.rows,
                src.last_error
                    .as_deref()
                    .map(|e| format!(" error: {e}"))
                    .unwrap_or_default()
            );
        }
    }
    if !s.local_sources.is_empty() {
        println!("local sources:");
        for l in &s.local_sources {
            println!(
                "  {} {} policy={} ledger={} head={} compacted={} backlog={} rows/{} tombstones{}{}",
                l.source_id,
                l.name,
                l.policy,
                if l.enabled {
                    if l.ready { "ready" } else { "backfilling" }
                } else {
                    "off"
                },
                l.head_seq,
                l.compacted_through,
                l.backlog_rows,
                l.backlog_tombstones,
                l.backlog_oldest_age_ms
                    .map(|ms| format!(" oldest {}s", ms / 1000))
                    .unwrap_or_default(),
                if l.degraded { " DEGRADED" } else { "" }
            );
        }
    }
    if !s.replica_sources.is_empty() {
        println!("replicated sources:");
        for r in &s.replica_sources {
            println!(
                "  {} {} from {} applied {}/{} {}{}",
                r.source_id,
                r.name,
                r.node_name,
                r.applied_seq,
                r.reported_head,
                if r.connected { "connected" } else { "offline" },
                if r.resyncing { " RESYNCING" } else { "" }
            );
        }
    }
    let c = &s.counters;
    println!(
        "counters: connections out/in {}/{} duplicates {} batches sent/applied {}/{} rows {}/{} acks {}/{} fences {} resyncs {} repairs {} bytes catalog tx/rx {}/{}",
        c.connections_established_outbound,
        c.connections_established_inbound,
        c.duplicate_sessions_closed,
        c.batches_sent,
        c.batches_applied,
        c.rows_shipped,
        c.rows_applied,
        c.acks_sent,
        c.acks_received,
        c.fences,
        c.full_resyncs,
        c.repairs_applied,
        c.bytes_catalog_sent,
        c.bytes_catalog_received
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Wrap {
        #[command(flatten)]
        fleet: FleetArgs,
    }

    #[test]
    fn policy_reads_api_integer_strings() {
        let sources = vec![serde_json::json!({
            "source": { "id": "42", "name": "reports" }
        })];
        assert_eq!(source_id_by_name(&sources, "reports"), Some(42));
    }

    #[test]
    fn contradictory_mutation_flags_are_rejected() {
        assert!(Wrap::try_parse_from([
            "eidos-fleet",
            "central",
            "--listen",
            "0.0.0.0:7710",
            "--no-listen",
        ])
        .is_err());
        assert!(Wrap::try_parse_from([
            "eidos-fleet",
            "peer",
            "00112233445566778899aabbccddeeff",
            "--forget",
            "--disable",
        ])
        .is_err());
    }
}

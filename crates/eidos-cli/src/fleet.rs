//! `eidos fleet ...`: identity, central role, invitations, enrollment, and
//! sync status.
//!
//! Commands talk to the running service over the loopback API so the
//! service's own runtime picks changes up at once. The ones that make sense
//! without a running service (`identity`, `central`, `enroll`) fall back to
//! working on the data directory directly when the service does not answer.

use anyhow::{anyhow, Context};
use clap::{Args, Subcommand};
use eidos_catalog::Catalog;
use eidos_domain::SyncPolicy;
use eidos_fleet::{FleetConfig, FleetStatus, InviteCode, NodeIdentity};
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
    /// Enable or disable the central role and set the sync listener.
    Central {
        /// Listen address for the dedicated sync endpoint, e.g. 0.0.0.0:7710.
        #[arg(long, conflicts_with = "no_listen")]
        listen: Option<String>,
        /// Stop accepting enrollments and replicas (the listener stays).
        #[arg(long)]
        disable: bool,
        /// Stop listening.
        #[arg(long)]
        no_listen: bool,
    },
    /// Mint a single-use invitation for one node (central only).
    Invite {
        /// How the node reaches this central (host:port); defaults to this
        /// host's name and the listener port.
        #[arg(long)]
        endpoint: Option<String>,
        /// Name to record for the node instead of the one it reports.
        #[arg(long)]
        name: Option<String>,
    },
    /// Redeem an invitation: this node replicates into the central from now on.
    Enroll { code: String },
    /// Stop transferring without forgetting the central (resume later without a resync).
    Pause,
    /// Resume transfer after `pause`.
    Resume,
    /// Leave the fleet: forget the central and drop the ledgers.
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
        /// Forget the node and retire its replicated sources (central only).
        #[arg(
            long,
            conflicts_with_all = ["endpoint", "enable", "disable"]
        )]
        forget: bool,
    },
    /// Keep a source on this host only, or let it follow enrollment again.
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
                    if status.central { "central" } else { "node" }
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
        FleetCommand::Central {
            listen,
            disable,
            no_listen,
        } => {
            if service_reachable(&url) {
                let body = serde_json::json!({
                    "central": if disable { Some(false) } else if listen.is_some() { Some(true) } else { None },
                    "listen": if no_listen { Some(String::new()) } else { listen.clone() },
                });
                let config: FleetConfig = send(&url, "POST", "/api/fleet/central", &body)?;
                println!(
                    "central: {}  listen: {}",
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
                    } else if listen.is_some() {
                        config.central = true;
                    }
                    if no_listen {
                        config.listen = None;
                    } else if let Some(l) = listen {
                        config.listen = Some(l);
                    }
                    Ok(())
                })?;
                println!(
                    "central: {}  listen: {}  (written to {}; takes effect when the service starts or on its next tick)",
                    config.central,
                    config.listen.as_deref().unwrap_or("(none)"),
                    FleetConfig::path(&args.data_dir).display()
                );
            }
        }
        FleetCommand::Invite { endpoint, name } => {
            if service_reachable(&url) {
                let body = serde_json::json!({ "endpoint": endpoint, "name_hint": name });
                let view: serde_json::Value = send(&url, "POST", "/api/fleet/invite", &body)?;
                if args.json {
                    println!("{}", serde_json::to_string_pretty(&view)?);
                } else {
                    println!("{}", view["code"].as_str().unwrap_or_default());
                    eprintln!(
                        "single use, valid 24 hours; run on the node: eidos fleet enroll <code>  (endpoint {})",
                        view["endpoint"].as_str().unwrap_or_default()
                    );
                }
            } else {
                let endpoint = endpoint.ok_or_else(|| {
                    anyhow!("the service is not running; pass --endpoint host:port explicitly")
                })?;
                let catalog = Arc::new(Catalog::open(args.data_dir.join("catalog.db"))?);
                let identity =
                    NodeIdentity::load_or_create(&args.data_dir, &eidos_domain::bench::hostname())?;
                let config = FleetConfig::load(&args.data_dir)?;
                let code = eidos_fleet::enroll::create_invite(
                    &catalog,
                    &identity,
                    &config,
                    &endpoint,
                    name.as_deref(),
                )?;
                println!("{}", code.encode());
                eprintln!("single use, valid 24 hours; the central service must be listening on {endpoint} for the node to redeem it");
            }
        }
        FleetCommand::Enroll { code } => {
            let invite = InviteCode::parse(&code)?;
            if service_reachable(&url) {
                let view: serde_json::Value = send(
                    &url,
                    "POST",
                    "/api/fleet/enroll",
                    &serde_json::json!({ "code": code }),
                )?;
                println!(
                    "enrolled with central {} ({}) at {}",
                    view["central_name"].as_str().unwrap_or_default(),
                    view["central"].as_str().unwrap_or_default(),
                    view["endpoint"].as_str().unwrap_or_default()
                );
            } else {
                let catalog = Arc::new(Catalog::open(args.data_dir.join("catalog.db"))?);
                let identity =
                    NodeIdentity::load_or_create(&args.data_dir, &eidos_domain::bench::hostname())?;
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                let outcome = rt.block_on(eidos_fleet::enroll::enroll(
                    &catalog,
                    &identity,
                    &invite,
                    Duration::from_secs(15),
                ))?;
                println!(
                    "enrolled with central {} ({}) at {}; sync starts when the service runs",
                    outcome.central_name, outcome.central, outcome.endpoint
                );
            }
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
        if s.central { "central" } else { "node" },
        s.fingerprint
    );
    println!(
        "enrolled: {}  sync: {}  listening: {}",
        s.enrolled,
        if s.sync_enabled { "on" } else { "off" },
        s.listening.as_deref().unwrap_or("no")
    );
    if s.pending_invites > 0 {
        println!("pending invitations: {}", s.pending_invites);
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

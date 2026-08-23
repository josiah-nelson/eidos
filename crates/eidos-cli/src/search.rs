//! `eidos search` — runs a query against the running service over HTTP so the
//! CLI and the web UI share one query contract and result schema.
//!
//! Exit status: 0 = complete results; 2 = results returned but at least one
//! in-scope source is incomplete/stale/offline; 1 = error.

use anyhow::Context;
use clap::Args;
use eidos_domain::{ResultMode, SearchResponse, SortField};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Query text in eidos syntax (see `docs/QUERY_SYNTAX.md`).
    pub query: Vec<String>,
    /// Service base URL.
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    pub url: String,
    /// Result mode: files, directories, both.
    #[arg(long, default_value = "files")]
    pub mode: String,
    /// Sort field: relevance, name, path, size, allocated_size, subtree_size, modified, created.
    #[arg(long, default_value = "relevance")]
    pub sort: String,
    #[arg(long)]
    pub desc: bool,
    #[arg(long, default_value_t = 50)]
    pub limit: u32,
    /// Continue from a cursor returned by a previous call.
    #[arg(long)]
    pub cursor: Option<String>,
    /// Emit the full JSON response.
    #[arg(long)]
    pub json: bool,
    /// Include the planner explanation.
    #[arg(long)]
    pub explain: bool,
    /// Comma-separated facets: source,extension,kind,content_state,top_directory,size_bucket,modified_bucket
    #[arg(long)]
    pub facets: Option<String>,
    /// Stream the whole result set through /api/search/export: csv, json, or ndjson.
    #[arg(long, value_name = "FORMAT")]
    pub export: Option<String>,
    /// Destination for --export; `-` or omitted writes to stdout.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
    /// Row cap for --export. The service cap still applies.
    #[arg(long, value_name = "N")]
    pub export_limit: Option<u64>,
    /// Prefix a UTF-8 BOM to a CSV export (for Excel).
    #[arg(long)]
    pub bom: bool,
}

#[derive(Deserialize)]
struct SearchView {
    #[serde(flatten)]
    response: SearchResponse,
    rendered: String,
    #[serde(default)]
    notes: Vec<String>,
}

#[derive(Deserialize)]
struct ApiErr {
    error: String,
    #[serde(default)]
    kind: String,
}

pub fn run(args: SearchArgs) -> anyhow::Result<i32> {
    let q = args.query.join(" ");
    if args.export.is_some() {
        return export(&args, &q);
    }
    let mode: ResultMode = serde_json::from_value(serde_json::Value::String(args.mode.clone()))
        .with_context(|| format!("invalid mode {}", args.mode))?;
    let sort: SortField = serde_json::from_value(serde_json::Value::String(args.sort.clone()))
        .with_context(|| format!("invalid sort {}", args.sort))?;
    let facets: Vec<serde_json::Value> = args
        .facets
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|f| serde_json::json!({"field": f, "limit": 20}))
        .collect();
    let body = serde_json::json!({
        "q": q,
        "mode": mode,
        "sort": {"field": sort, "descending": args.desc},
        "limit": args.limit,
        "cursor": args.cursor,
        "explain": args.explain,
        "facets": facets,
    });
    let agent: ureq::Agent = ureq::config::Config::builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut resp = agent
        .post(format!("{}/api/search", args.url.trim_end_matches('/')))
        .send_json(&body)
        .with_context(|| format!("connecting to {}", args.url))?;
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string()?;
    if status >= 400 {
        let err: ApiErr = serde_json::from_str(&text).unwrap_or(ApiErr {
            error: text.clone(),
            kind: String::new(),
        });
        eprintln!("error ({} {}): {}", status, err.kind, err.error);
        return Ok(1);
    }
    let view: SearchView = serde_json::from_str(&text).context("decoding response")?;
    if args.json {
        println!("{text}");
    } else {
        print_table(&view, &q);
    }
    let needs_content = view.response.hits.iter().any(|h| !h.snippets.is_empty());
    Ok(if view.response.all_sources_complete(needs_content) {
        0
    } else {
        2
    })
}

fn print_table(v: &SearchView, q: &str) {
    let r = &v.response;
    println!("query: {q}");
    println!("interpreted: {}", v.rendered);
    for n in &v.notes {
        println!("  note: {n}");
    }
    for w in &r.warnings {
        println!("  warning: {w}");
    }
    println!(
        "{} result(s){} in {:.1} ms (plan {:.1}, retrieve {:.1}, verify {:.1}, join {:.1})",
        r.total.value,
        if r.total.exact { "" } else { "+" },
        r.timing.total_ms,
        r.timing.plan_ms,
        r.timing.retrieve_ms,
        r.timing.verify_ms,
        r.timing.join_ms
    );
    for h in &r.hits {
        let size = if let Some(d) = &h.directory {
            format!(
                "{:>10}  {:>6}f",
                crate::profile::human_bytes(d.logical_bytes),
                d.file_count
            )
        } else {
            format!("{:>10}  {:>7}", crate::profile::human_bytes(h.size), "")
        };
        let when = h.modified.map(|t| t.to_rfc3339()).unwrap_or_default();
        let when = when.get(0..10).unwrap_or("").to_string();
        let state = if h.kind.is_directory_like() {
            String::new()
        } else {
            format!(" [{}]", h.content.state)
        };
        println!(
            "{size}  {when}  {}{state}",
            h.path.clone().unwrap_or_else(|| h.name.clone())
        );
        for s in &h.snippets {
            let text = s.text.replace(['\r', '\n'], " ");
            println!("      L{:<7} {}", s.line_start + 1, text);
        }
    }
    for f in &r.facets {
        println!("facet {:?}:", f.field);
        for val in &f.values {
            // Range buckets carry the clause that selects them; print it so
            // the bucket can be turned into a query by copying it.
            let clause = val
                .range
                .as_ref()
                .map(|r| format!("   {}", r.clause))
                .unwrap_or_default();
            println!(
                "    {:<28} {:>8}{clause}",
                val.label.clone().unwrap_or_else(|| val.value.clone()),
                val.count
            );
        }
    }
    println!("sources:");
    for c in &r.completeness {
        println!(
            "    {:<12} {:<18} metadata {} · content {} ({} pending) · freshness {:?}{}",
            c.name,
            c.state,
            if c.metadata_complete {
                "complete"
            } else {
                "INCOMPLETE"
            },
            if c.content_complete {
                "complete"
            } else {
                "pending"
            },
            c.content_pending,
            c.freshness,
            c.note
                .as_ref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default()
        );
    }
    if let Some(e) = &r.explanation {
        println!("plan: {}", e.readable);
        for s in &e.steps {
            println!(
                "    {:<10} {}{}{}",
                s.stage,
                s.description,
                s.candidates
                    .map(|c| format!(" · {c} candidates"))
                    .unwrap_or_default(),
                s.verified
                    .map(|c| format!(" · {c} verified"))
                    .unwrap_or_default()
            );
        }
    }
    if let Some(c) = &r.next_cursor {
        println!("next: --cursor {c}");
    }
}

/// Percent-encode a query-string value (RFC 3986 unreserved set kept as-is).
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Stream `/api/search/export` straight to a file (or stdout) without holding
/// the result set in memory.
fn export(args: &SearchArgs, q: &str) -> anyhow::Result<i32> {
    let format = args.export.as_deref().unwrap_or("csv");
    if !matches!(format, "csv" | "json" | "ndjson") {
        anyhow::bail!("invalid --export format {format} (csv, json, ndjson)");
    }
    let mut url = format!(
        "{}/api/search/export?format={}&q={}&mode={}&sort={}&desc={}",
        args.url.trim_end_matches('/'),
        format,
        encode(q),
        encode(&args.mode),
        encode(&args.sort),
        args.desc
    );
    if let Some(n) = args.export_limit {
        url.push_str(&format!("&limit={n}"));
    }
    if args.bom {
        url.push_str("&bom=1");
    }
    let agent: ureq::Agent = ureq::config::Config::builder()
        .http_status_as_error(false)
        .build()
        .new_agent();
    let mut resp = agent
        .get(&url)
        .call()
        .with_context(|| format!("connecting to {}", args.url))?;
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let total = header("x-eidos-export-total");
    let exact = header("x-eidos-export-total-exact");
    let cap = header("x-eidos-export-max-rows");
    if status >= 400 {
        let text = resp.body_mut().read_to_string()?;
        let err: ApiErr = serde_json::from_str(&text).unwrap_or(ApiErr {
            error: text.clone(),
            kind: String::new(),
        });
        eprintln!("error ({} {}): {}", status, err.kind, err.error);
        return Ok(1);
    }
    let mut reader = resp.body_mut().as_reader();
    let written = match args.out.as_deref() {
        Some(p) if p != std::path::Path::new("-") => {
            let mut f =
                std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?;
            let n = std::io::copy(&mut reader, &mut f)?;
            eprintln!("wrote {n} bytes to {}", p.display());
            n
        }
        _ => std::io::copy(&mut reader, &mut std::io::stdout().lock())?,
    };
    let _ = written;
    if let (Some(total), Some(cap)) = (total.as_deref(), cap.as_deref()) {
        let truncated = total
            .parse::<u64>()
            .ok()
            .zip(cap.parse::<u64>().ok())
            .is_some_and(|(t, c)| t > c);
        eprintln!(
            "matched {total}{} result(s); export cap {cap}{}",
            if exact.as_deref() == Some("false") {
                "+"
            } else {
                ""
            },
            if truncated { " (TRUNCATED)" } else { "" }
        );
    }
    Ok(0)
}

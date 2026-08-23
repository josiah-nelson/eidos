//! `eidos archive`: archive manifests from the running service.

use clap::{Args, Subcommand};

#[derive(Args, Debug)]
pub struct ArchiveArgs {
    /// Service base URL.
    #[arg(long, env = "EIDOS_URL", default_value = "http://127.0.0.1:7700")]
    url: String,
    #[command(subcommand)]
    command: ArchiveCommand,
}

#[derive(Subcommand, Debug)]
enum ArchiveCommand {
    /// Show a container's manifest (object id from `eidos search --json`).
    Show {
        object: i64,
        /// List the children of this virtual directory ("" for the root)
        /// instead of every member.
        #[arg(long)]
        parent: Option<String>,
        /// List members whose path starts with this prefix.
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long, default_value_t = 200)]
        limit: u32,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Queue manifest jobs for a source's container files that have none
    /// (files crawled before archive support existed).
    Requeue {
        /// Source id (`eidos source list`).
        source: i64,
    },
}

pub fn run(args: ArchiveArgs) -> anyhow::Result<()> {
    match args.command {
        ArchiveCommand::Show {
            object,
            parent,
            prefix,
            offset,
            limit,
            json,
        } => {
            let mut url = format!(
                "{}/api/objects/{object}/archive?offset={offset}&limit={limit}",
                args.url
            );
            if let Some(p) = parent {
                url.push_str(&format!("&parent={}", urlencode(&p)));
            }
            if let Some(p) = prefix {
                url.push_str(&format!("&prefix={}", urlencode(&p)));
            }
            let value: serde_json::Value = ureq::get(&url).call()?.body_mut().read_json()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                print_manifest(&value);
            }
        }
        ArchiveCommand::Requeue { source } => {
            let value: serde_json::Value =
                ureq::post(format!("{}/api/sources/{source}/archives", args.url))
                    .send_empty()?
                    .body_mut()
                    .read_json()?;
            println!(
                "queued {} manifest job(s)",
                value.get("queued").and_then(|v| v.as_u64()).unwrap_or(0)
            );
        }
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn print_manifest(v: &serde_json::Value) {
    let rec = &v["record"];
    let s = |k: &str| rec[k].as_str().unwrap_or("").to_string();
    let n = |k: &str| rec[k].as_u64().unwrap_or(0);
    println!("{}", v["path"].as_str().unwrap_or("(path unknown)"));
    println!(
        "{} manifest, state {}: {} members, {} directories ({} implicit), declared {} in {} compressed{}{}",
        s("format"),
        s("state"),
        n("member_count"),
        n("dir_count"),
        n("implicit_dir_count"),
        human(n("declared_size")),
        human(n("compressed_size")),
        if rec["zip64"].as_bool().unwrap_or(false) {
            ", zip64"
        } else {
            ""
        },
        if rec["truncated"].as_bool().unwrap_or(false) {
            ", TRUNCATED"
        } else {
            ""
        },
    );
    if n("suspicious_count") > 0 {
        println!(
            "{} member name(s) flagged (traversal/absolute/backslash/duplicate/encoding); shown with '!'",
            n("suspicious_count")
        );
    }
    if let Some(r) = rec["reason"].as_str() {
        println!("{r}");
    }
    if let Some(e) = rec["error"].as_str() {
        println!("error: {e}");
    }
    if let Some(c) = rec["comment"].as_str() {
        println!("comment: {c}");
    }
    let total = v["total"].as_u64().unwrap_or(0);
    let members = v["members"].as_array().cloned().unwrap_or_default();
    println!("--- {} of {total} member(s)", members.len());
    for m in &members {
        let flags = m["flags"].as_u64().unwrap_or(0);
        let is_dir = m["is_dir"].as_bool().unwrap_or(false);
        let implicit = m["implicit"].as_bool().unwrap_or(false);
        println!(
            "{}{} {:>10}  {}{}",
            if flags != 0 { "!" } else { " " },
            if is_dir { "D" } else { "F" },
            if is_dir {
                String::new()
            } else {
                human(m["size"].as_u64().unwrap_or(0))
            },
            m["path"].as_str().unwrap_or(""),
            if implicit {
                "/ (implicit)"
            } else if is_dir {
                "/"
            } else {
                ""
            },
        );
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

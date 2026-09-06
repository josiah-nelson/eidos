//! `eidos updates`: advisory release checks and verified installer staging.

use anyhow::{anyhow, Context};
use clap::{Args, Subcommand};
use eidos_service::updates::{StagePhase, UpdateSettings, UpdateState};
use serde::de::DeserializeOwned;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct UpdateArgs {
    #[arg(
        long,
        env = "EIDOS_URL",
        default_value = "http://127.0.0.1:7700",
        global = true
    )]
    url: String,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: UpdateCommand,
}

#[derive(Subcommand, Debug)]
enum UpdateCommand {
    /// Show the last release check and staging result.
    Status,
    /// Check the canonical GitHub release now.
    Check,
    /// Download and verify the discovered canonical setup artifact. Does not install it.
    Stage,
    /// Configure advisory checks and the required Windows signer identity.
    Configure {
        #[arg(long)]
        expected_publisher: Option<String>,
        #[arg(long)]
        expected_product: Option<String>,
        #[arg(long, conflicts_with = "disable_automatic_checks")]
        automatic_checks: bool,
        #[arg(long)]
        disable_automatic_checks: bool,
        #[arg(long)]
        max_artifact_mib: Option<u64>,
    },
}

fn request<T: DeserializeOwned>(
    url: &str,
    method: &str,
    path: &str,
    body: Option<&impl serde::Serialize>,
) -> anyhow::Result<T> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(180)))
        .http_status_as_error(false)
        .build()
        .into();
    let full = format!("{}{path}", url.trim_end_matches('/'));
    let mut response = match (method, body) {
        ("GET", _) => agent.get(&full).call(),
        (_, Some(body)) => agent.post(&full).send_json(body),
        _ => agent.post(&full).send_empty(),
    }
    .with_context(|| format!("connecting to {url}"))?;
    let status = response.status().as_u16();
    let text = response.body_mut().read_to_string()?;
    if status >= 300 {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_owned))
            .unwrap_or(text);
        return Err(anyhow!("service answered {status}: {message}"));
    }
    Ok(serde_json::from_str(&text)?)
}

pub fn run(args: UpdateArgs) -> anyhow::Result<()> {
    let state = match args.command {
        UpdateCommand::Status => {
            request::<UpdateState>(&args.url, "GET", "/api/updates", None::<&&str>)?
        }
        UpdateCommand::Check => {
            request::<UpdateState>(&args.url, "POST", "/api/updates/check", None::<&&str>)?
        }
        UpdateCommand::Stage => {
            request::<UpdateState>(&args.url, "POST", "/api/updates/stage", None::<&&str>)?
        }
        UpdateCommand::Configure {
            expected_publisher,
            expected_product,
            automatic_checks,
            disable_automatic_checks,
            max_artifact_mib,
        } => {
            let mut settings = request::<UpdateSettings>(
                &args.url,
                "GET",
                "/api/updates/settings",
                None::<&&str>,
            )?;
            if let Some(value) = expected_publisher {
                settings.expected_publisher = Some(value);
            }
            if let Some(value) = expected_product {
                settings.expected_product = value;
            }
            if automatic_checks {
                settings.automatic_checks = true;
            }
            if disable_automatic_checks {
                settings.automatic_checks = false;
            }
            if let Some(value) = max_artifact_mib {
                settings.max_artifact_bytes = value.saturating_mul(1024 * 1024);
            }
            request(&args.url, "POST", "/api/updates/settings", Some(&settings))?
        }
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&state)?);
    } else {
        print_state(&state);
    }
    Ok(())
}

fn print_state(state: &UpdateState) {
    println!(
        "running {}  automatic checks: {}",
        state.current_version,
        if state.checks_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if let Some(release) = &state.available {
        println!(
            "available {}  {} bytes  sha256 {}",
            release.version, release.size, release.sha256
        );
    } else {
        println!("available: none");
    }
    println!(
        "staging: {}",
        match state.stage_phase {
            StagePhase::Idle => "idle",
            StagePhase::Downloading => "downloading",
            StagePhase::Verifying => "verifying",
            StagePhase::Staged => "verified and staged",
            StagePhase::Failed => "failed",
        }
    );
    if let Some(error) = &state.check_error {
        println!("check error: {error}");
    }
    if let Some(error) = &state.stage_error {
        println!("stage error: {error}");
    }
    if let Some(staged) = &state.staged {
        println!("staged {} at {}", staged.version, staged.path);
    }
}

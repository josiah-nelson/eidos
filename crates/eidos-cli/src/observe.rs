use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct ObserveArgs {
    #[command(subcommand)]
    command: ObserveCommand,
}

#[derive(Debug, Subcommand)]
enum ObserveCommand {
    /// Create the study key in the current user's login keychain.
    Init {
        /// Replace an existing study key, making prior object tokens unlinkable.
        #[arg(long)]
        force: bool,
    },
    /// Maintain the user-session key handoff to the privileged collector.
    Run {
        #[arg(long, default_value = default_socket())]
        socket: PathBuf,
    },
    /// Show collector capabilities, feed health, and ring usage.
    Status {
        #[arg(long, default_value = default_socket())]
        socket: PathBuf,
    },
    /// Add a keyed phase marker; the supplied label is never persisted.
    Mark {
        label: String,
        #[arg(long, default_value = default_socket())]
        socket: PathBuf,
    },
    /// Ask the daemon for a versioned study bundle and copy it locally.
    Export {
        #[arg(long, short, default_value = "observation.eidos-observation.zst")]
        output: PathBuf,
        #[arg(long, default_value = default_socket())]
        socket: PathBuf,
    },
    /// List exactly the fields and record count in a study bundle.
    Inspect { bundle: PathBuf },
}

const fn default_socket() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        eidos_macos_collector::DEFAULT_SOCKET
    }
    #[cfg(not(target_os = "macos"))]
    {
        "/var/run/eidos-collector.sock"
    }
}

pub fn run(args: ObserveArgs) -> anyhow::Result<()> {
    match args.command {
        ObserveCommand::Inspect { bundle } => inspect(&bundle),
        #[cfg(target_os = "macos")]
        ObserveCommand::Init { force } => {
            if eidos_macos_collector::client::init_key(force)? {
                println!("study key created in the login keychain");
            } else {
                println!("study key already exists in the login keychain");
            }
            Ok(())
        }
        #[cfg(target_os = "macos")]
        ObserveCommand::Run { socket } => loop {
            match eidos_macos_collector::client::load_session_key(&socket) {
                Ok(()) => std::thread::sleep(std::time::Duration::from_secs(60)),
                Err(error) => {
                    tracing::warn!(error = %error, "collector session handoff failed");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        },
        #[cfg(target_os = "macos")]
        ObserveCommand::Status { socket } => {
            use eidos_macos_collector::protocol::{Request, Response};
            let response = eidos_macos_collector::client::request(&socket, &Request::Status)?;
            match response {
                Response::Status { status } => {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                    Ok(())
                }
                Response::Error { message } => anyhow::bail!(message),
                _ => anyhow::bail!("unexpected collector response"),
            }
        }
        #[cfg(target_os = "macos")]
        ObserveCommand::Mark { label, socket } => {
            use eidos_macos_collector::protocol::{Request, Response};
            eidos_macos_collector::client::load_session_key(&socket)?;
            match eidos_macos_collector::client::request(&socket, &Request::Mark { label })? {
                Response::Accepted => Ok(()),
                Response::Error { message } => anyhow::bail!(message),
                _ => anyhow::bail!("unexpected collector response"),
            }
        }
        #[cfg(target_os = "macos")]
        ObserveCommand::Export { output, socket } => {
            use eidos_macos_collector::protocol::{Request, Response};
            let response = eidos_macos_collector::client::request(&socket, &Request::Export)?;
            let Response::Exported { staged_file } = response else {
                if let Response::Error { message } = response {
                    anyhow::bail!(message);
                }
                anyhow::bail!("unexpected collector response");
            };
            let mut source = std::fs::File::open(staged_file)?;
            let mut destination = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)?;
            std::io::copy(&mut source, &mut destination)?;
            println!("{}", output.display());
            Ok(())
        }
        #[cfg(not(target_os = "macos"))]
        _ => anyhow::bail!("this observe command requires macOS"),
    }
}

fn inspect(bundle: &std::path::Path) -> anyhow::Result<()> {
    let inspection = eidos_observe::inspect_bundle(bundle)?;
    println!("schema: {}", inspection.schema);
    println!("records: {}", inspection.records);
    println!("fields:");
    for field in inspection.fields {
        println!("  {field}");
    }
    Ok(())
}

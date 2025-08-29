use anyhow::Result;
use base64::Engine as _;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mee-dev", version, about = "Mee PDN developer CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // HTTP transport utilities
    Http {
        #[command(subcommand)]
        cmd: HttpCmd,
    },
}

#[derive(Subcommand)]
enum HttpCmd {
    // Fetch server ticket from base URL
    Ticket {
        #[arg(long = "url")]
        url: String,
    },
    // Ask a running node to send a ping
    SendPing {
        #[arg(long = "url")]
        url: String,
        #[arg(long = "to-ticket")]
        to_ticket: String,
        #[arg(long)]
        body: Option<String>,
    },
    // Show inbox items from a node
    Inbox {
        #[arg(long = "url")]
        url: String,
    },
    // Create a connection to a ticket
    Connect {
        #[arg(long = "url")]
        url: String,
        #[arg(long = "to-ticket")]
        to_ticket: String,
    },
    // List connections
    Connections {
        #[arg(long = "url")]
        url: String,
    },
    // Send via connection
    Send {
        #[arg(long = "url")]
        url: String,
        #[arg(long = "conn")]
        conn: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        body: Option<String>,
    },
    // Close connection
    Close {
        #[arg(long = "url")]
        url: String,
        #[arg(long = "conn")]
        conn: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Http { cmd } => match cmd {
            HttpCmd::Ticket { url } => {
                let u = format!("{}/demo/ticket", url.trim_end_matches('/'));
                let resp: serde_json::Value = ureq::get(&u).call()?.into_json()?;
                println!("{}", resp["ticket"].as_str().unwrap_or_default());
            }
            HttpCmd::SendPing {
                url,
                to_ticket,
                body,
            } => {
                let u = format!("{}/demo/send/ping", url.trim_end_matches('/'));
                let mut payload = serde_json::json!({ "to_ticket": to_ticket });
                if let Some(b) = body {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(b.as_bytes());
                    payload["body_b64"] = serde_json::Value::String(b64);
                }
                let _ = ureq::post(&u).send_json(payload)?.into_string();
                println!("sent");
            }
            HttpCmd::Inbox { url } => {
                let u = format!("{}/demo/inbox", url.trim_end_matches('/'));
                let body = ureq::get(&u).call()?.into_string()?;
                println!("{}", body);
            }
            HttpCmd::Connect { url, to_ticket } => {
                let u = format!("{}/demo/connections", url.trim_end_matches('/'));
                let resp: serde_json::Value = ureq::post(&u)
                    .send_json(serde_json::json!({"to_ticket": to_ticket}))?
                    .into_json()?;
                println!("{}", resp["connection_id"].as_str().unwrap_or_default());
            }
            HttpCmd::Connections { url } => {
                let u = format!("{}/demo/connections", url.trim_end_matches('/'));
                let txt = ureq::get(&u).call()?.into_string()?;
                println!("{}", txt);
            }
            HttpCmd::Send {
                url,
                conn,
                kind,
                body,
            } => {
                let u = format!(
                    "{}/demo/connections/{}/send",
                    url.trim_end_matches('/'),
                    conn
                );
                let mut payload = serde_json::json!({ "kind": kind });
                if let Some(b) = body {
                    let b64 = base64::engine::general_purpose::STANDARD.encode(b.as_bytes());
                    payload["body_b64"] = serde_json::Value::String(b64);
                }
                let _ = ureq::post(&u).send_json(payload)?.into_string();
                println!("sent");
            }
            HttpCmd::Close { url, conn } => {
                let u = format!("{}/demo/connections/{}", url.trim_end_matches('/'), conn);
                let _ = ureq::delete(&u).call()?;
                println!("ok");
            }
        },
    }
    Ok(())
}

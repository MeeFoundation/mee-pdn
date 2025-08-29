use mee_node_axum::serve;
use mee_transport_api::ProfileName;
use std::env;
use std::net::SocketAddr;

fn print_help() {
    eprintln!("mee-node-axum --profile <name> --addr <host:port> --base-url <http://host:port>");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().collect();
    let mut profile: Option<String> = None;
    let mut addr: Option<String> = None;
    let mut base_url: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                profile = args.get(i + 1).cloned();
                i += 2;
            }
            "--addr" => {
                addr = args.get(i + 1).cloned();
                i += 2;
            }
            "--base-url" => {
                base_url = args.get(i + 1).cloned();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }
    let (Some(profile), Some(addr), Some(base_url)) = (profile, addr, base_url) else {
        print_help();
        std::process::exit(2);
    };
    let addr: SocketAddr = addr.parse()?;
    serve(ProfileName::from(profile), addr, base_url).await
}

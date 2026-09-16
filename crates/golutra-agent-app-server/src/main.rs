use std::net::SocketAddr;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "golutra-agent-app-server")]
#[command(version)]
#[command(about = "Golutra user-level runtime daemon and HTTP app server")]
struct Args {
    #[arg(
        long,
        env = "GOLUTRA_AGENT_APP_ADDR",
        default_value = "127.0.0.1:47831"
    )]
    addr: SocketAddr,
    #[arg(long, conflicts_with = "addr")]
    stdio: bool,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    if args.stdio {
        golutra_agent_app_server::run_stdio().await
    } else {
        golutra_agent_app_server::run(args.addr).await
    }
}

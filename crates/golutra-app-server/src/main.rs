use std::net::SocketAddr;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "golutra-app-server")]
#[command(about = "Golutra workspace runtime daemon and HTTP app server")]
struct Args {
    #[arg(long, env = "GOLUTRA_APP_ADDR", default_value = "127.0.0.1:47831")]
    addr: SocketAddr,
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    match args.workspace {
        Some(workspace) => golutra_app_server::run_workspace(args.addr, workspace).await,
        None => golutra_app_server::run(args.addr).await,
    }
}

use std::{net::SocketAddr, str::FromStr};

#[tokio::main]
async fn main() -> miette::Result<()> {
    let addr = std::env::var("GOLUTRA_APP_ADDR")
        .ok()
        .and_then(|value| SocketAddr::from_str(&value).ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 47831)));
    golutra_app_server::run(addr).await
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let ok = golutra_test_client::transport_smoke().await?;
    println!(
        "{}",
        if ok {
            "transport smoke passed"
        } else {
            "transport smoke failed"
        }
    );
    Ok(())
}

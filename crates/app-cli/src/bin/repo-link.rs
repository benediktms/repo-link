#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app_cli::run().await
}

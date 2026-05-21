#[tokio::main]
async fn main() -> anyhow::Result<()> {
    app_daemon::run_cli().await
}

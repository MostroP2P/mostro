use anyhow::Result;
use dotenvy::dotenv;
use mostro_client::cli::run;
use std::process;

#[tokio::main]
async fn main() -> Result<()> {
    dotenv().ok();

    if let Err(e) = run().await {
        eprintln!("{e}");
        process::exit(1);
    }

    process::exit(0);
}

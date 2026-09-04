use xbar_ai_usage::runtime::{run, RuntimeConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let debug = std::env::args().any(|argument| argument == "--debug");
    run(RuntimeConfig::from_environment(debug)?).await?;
    Ok(())
}

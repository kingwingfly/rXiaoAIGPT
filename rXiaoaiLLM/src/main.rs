mod agent;
mod command;
mod config;
mod music;
// The `MusicSource` implementations and the `Tool`s built on them exist ahead
// of the intent layer that will register them, so nothing in the binary reaches
// them yet.
#[allow(dead_code, unused_imports)]
mod source;
#[allow(dead_code)]
mod tools;

use agent::Agent;
use anyhow::Result;
use config::Config;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    Agent::new(Config::from_env()?).await?.run().await
}

/// Log at `info` for our own crates and `warn` for everything else (reqwest and
/// hyper are extremely chatty at `info`). Override wholesale with `RUST_LOG`,
/// e.g. `RUST_LOG=xiaoai=debug` to see the raw Xiaomi login exchanges.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn,xiaoai_llm=info,xiaoai=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

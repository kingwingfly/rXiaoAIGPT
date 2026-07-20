mod agent;
mod command;
mod config;
mod music;

use agent::Agent;
use anyhow::Result;
use config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    Agent::new(Config::from_env()?).await?.run().await
}

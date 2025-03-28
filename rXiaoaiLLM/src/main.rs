mod agent;

use agent::Agent;

#[tokio::main]
async fn main() {
    Agent::new("哈哈").await.unwrap().run().await.unwrap();
}

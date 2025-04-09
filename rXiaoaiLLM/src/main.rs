mod agent;

use agent::Agent;

#[tokio::main]
async fn main() {
    Agent::new("哈哈")
        .await
        .unwrap()
        .run("192.168.1.20", 3000)
        .await
        .unwrap();
}

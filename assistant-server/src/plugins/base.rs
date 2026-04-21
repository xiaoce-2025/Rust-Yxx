use async_trait::async_trait;

#[async_trait]
pub trait MessageModule: Send + Sync {
    async fn handle(&self, text: &str) -> String;
}

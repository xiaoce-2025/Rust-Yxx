use async_trait::async_trait;

use crate::plugins::base::MessageModule;

pub struct YxxModule;

#[async_trait]
impl MessageModule for YxxModule {
    async fn handle(&self, _text: &str) -> String {
        "你好呀，我是严小希~".to_string()
    }
}

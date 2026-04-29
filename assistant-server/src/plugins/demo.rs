use async_trait::async_trait;

use crate::plugins::base::MessageModule;

pub struct DemoModule;

#[async_trait]
impl MessageModule for DemoModule {
    async fn handle(&self, _text: &str) -> String {
        "成功分类".to_string()
    }
}

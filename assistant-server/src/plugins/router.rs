use std::{collections::HashMap, sync::Arc};

use crate::plugins::{
    base::MessageModule, classifier::MessageClassifier, demo::DemoModule, weather::WeatherModule,
    yxx::YxxModule,
};

pub struct PluginRouter {
    modules: HashMap<&'static str, Arc<dyn MessageModule>>,
}

impl PluginRouter {
    pub fn new() -> Self {
        let mut modules: HashMap<&'static str, Arc<dyn MessageModule>> = HashMap::new();
        modules.insert("demo", Arc::new(DemoModule));
        modules.insert("empty", Arc::new(DemoModule));
        modules.insert("unknown", Arc::new(DemoModule));
        modules.insert("weather", Arc::new(WeatherModule));
        modules.insert("yxx", Arc::new(YxxModule));
        Self { modules }
    }

    pub async fn reply(&self, text: &str) -> String {
        let category = MessageClassifier::classify(text);
        let module = self
            .modules
            .get(category)
            .or_else(|| self.modules.get("unknown"));
        if let Some(module) = module {
            module.handle(text).await
        } else {
            "成功分类".to_string()
        }
    }

    pub fn list_modules(&self) -> Vec<String> {
        let mut names: Vec<String> = self.modules.keys().map(|s| (*s).to_string()).collect();
        names.sort();
        names
    }
}

pub struct MessageClassifier;

impl MessageClassifier {
    pub fn classify(text: &str) -> &'static str {
        let content = text.trim();
        if content.is_empty() {
            return "empty";
        }
        if content.contains("天气") {
            return "weather";
        }
        if content.contains("严小希") {
            return "yxx";
        }
        "demo"
    }
}

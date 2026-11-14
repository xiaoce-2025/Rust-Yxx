//! Keep the existing clients on Rustls when Blackboard enables reqwest's native TLS feature.
//! Reqwest otherwise changes its default backend globally through Cargo feature unification.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_rustls_tls()
}

pub fn client() -> reqwest::Client {
    builder()
        .build()
        .expect("HTTP client initialization failed")
}

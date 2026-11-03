//! Container healthcheck: exits 0 when /health answers 200.

#[tokio::main]
async fn main() {
    let host = std::env::var("APP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("APP_PORT").unwrap_or_else(|_| "8080".to_string());
    let url = format!("http://{host}:{port}/health");
    match reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => std::process::exit(0),
        Ok(response) => {
            eprintln!("healthcheck failed: {}", response.status());
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("healthcheck failed: {error}");
            std::process::exit(1);
        }
    }
}

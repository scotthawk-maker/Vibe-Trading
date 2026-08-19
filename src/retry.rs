//! Shared HTTP retry-with-backoff helper.
//!
//! Used by the DexScreener and CoinGecko clients (M1) to recover from HTTP 429
//! rate-limit responses with exponential backoff and `Retry-After` header
//! handling.  Jupiter's client already has its own inline retry and is left
//! unchanged.

use std::error::Error;
use std::time::Duration;

/// Maximum number of retry attempts after an initial request.
const MAX_RETRIES: u32 = 5;

/// Issue a `GET` request with optional headers, retrying on HTTP 429.
///
/// On a 429 response the helper honours the `Retry-After` header (seconds)
/// when present, otherwise it falls back to exponential backoff
/// (`2^(attempt-1)` seconds, capped at 32s).  Non-429 errors are returned
/// immediately as `Ok(resp)` so the caller can inspect the status/body.
pub async fn get_with_retry(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<reqwest::Response, Box<dyn Error + Send + Sync>> {
    let mut attempt: u32 = 0;
    loop {
        let mut req = client.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req.send().await?;
        if resp.status().as_u16() == 429 && attempt < MAX_RETRIES {
            attempt += 1;
            let retry_after = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            // Exponential backoff: 2^(attempt-1) seconds, capped at 32s.
            let backoff_secs = if retry_after > 0 {
                retry_after
            } else {
                1u64 << (attempt - 1).min(5)
            };
            println!(
                "  [HTTP] 429 rate-limited on {}. Backing off {}s (attempt {}/{}).",
                url, backoff_secs, attempt, MAX_RETRIES
            );
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
            continue;
        }
        return Ok(resp);
    }
}
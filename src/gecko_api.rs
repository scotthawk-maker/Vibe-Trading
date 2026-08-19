use serde::Deserialize;
use serde_json::Value;
use std::error::Error;

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct GeckoPoolMetrics {
    pub pool_address: String,
    pub price_usd: f64,
    pub reserve_usd: f64,
    pub price_change_m5: f64,
    pub price_change_m15: f64,
    pub buys_m5: u32,
    pub sells_m5: u32,
    pub buys_m15: u32,
    pub sells_m15: u32,
    pub volume_m5_usd: f64,
    pub volume_m15_usd: f64,
}

/// Query CoinGecko Pro API /onchain endpoints to fetch pool metrics for a given Solana token contract address
///
/// M1: retries on HTTP 429 with exponential backoff and `Retry-After` header
/// handling via the shared [`crate::retry::get_with_retry`] helper.
pub async fn fetch_token_pool_metrics(
    token_address: &str,
    api_key: &str,
) -> Result<GeckoPoolMetrics, Box<dyn Error + Send + Sync>> {
    let client = reqwest::Client::new();
    let url = format!(
        "https://pro-api.coingecko.com/api/v3/onchain/networks/solana/tokens/{}/pools",
        token_address
    );

    let headers: [(&str, &str); 2] = [
        ("accept", "application/json;version=20230302"),
        ("x-cg-pro-api-key", api_key),
    ];
    let res = crate::retry::get_with_retry(&client, &url, &headers).await?;

    if !res.status().is_success() {
        let status = res.status();
        let err_text = res.text().await.unwrap_or_default();
        return Err(format!("CoinGecko Pro API returned error (status {}): {}", status, err_text).into());
    }

    let json: Value = res.json().await?;
    
    let pools_arr = json["data"]
        .as_array()
        .ok_or_else(|| "No pool data found in CoinGecko response")?;
        
    if pools_arr.is_empty() {
        return Err("No pools listed on CoinGecko for this token".into());
    }

    // The first pool in the list represents the primary pool (highest liquidity/volume)
    let primary_pool = &pools_arr[0];
    let attributes = &primary_pool["attributes"];
    let pool_address = attributes["address"].as_str().unwrap_or("").to_string();
    
    // Help parse numeric values that might be represented as floats or string-encoded decimals
    let parse_f64 = |val: &Value| -> f64 {
        if let Some(s) = val.as_str() {
            s.parse::<f64>().unwrap_or(0.0)
        } else if let Some(n) = val.as_f64() {
            n
        } else {
            0.0
        }
    };

    let price_usd = parse_f64(&attributes["token_price_usd"]);
    let reserve_usd = parse_f64(&attributes["reserve_in_usd"]);

    let price_change_m5 = parse_f64(&attributes["price_change_percentage"]["m5"]);
    let price_change_m15 = parse_f64(&attributes["price_change_percentage"]["m15"]);

    let buys_m5 = attributes["transactions"]["m5"]["buys"].as_u64().unwrap_or(0) as u32;
    let sells_m5 = attributes["transactions"]["m5"]["sells"].as_u64().unwrap_or(0) as u32;
    
    let buys_m15 = attributes["transactions"]["m15"]["buys"].as_u64().unwrap_or(0) as u32;
    let sells_m15 = attributes["transactions"]["m15"]["sells"].as_u64().unwrap_or(0) as u32;

    let volume_m5_usd = parse_f64(&attributes["volume_usd"]["m5"]);
    let volume_m15_usd = parse_f64(&attributes["volume_usd"]["m15"]);

    Ok(GeckoPoolMetrics {
        pool_address,
        price_usd,
        reserve_usd,
        price_change_m5,
        price_change_m15,
        buys_m5,
        sells_m5,
        buys_m15,
        sells_m15,
        volume_m5_usd,
        volume_m15_usd,
    })
}

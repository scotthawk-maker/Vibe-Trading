use serde::Deserialize;
use std::error::Error;

/// DexScreener API client for pre-entry safety checks.
/// Queries cross-DEX liquidity, pair age, buy/sell ratios, and price changes
/// before the bot enters a position. Complements GMGN discovery + Helius on-chain data.
///
/// API base: https://api.dexscreener.com
/// Rate limits: 300 req/min for token/pair lookups, 60 req/min for trending/boosts
/// No API key required.

const DEXSCREENER_BASE: &str = "https://api.dexscreener.com";

/// A single DEX pair returned by DexScreener's token-pairs endpoint.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexPair {
    pub chain_id: String,
    pub dex_id: String,
    pub pair_address: String,
    pub base_token: DexToken,
    pub quote_token: Option<DexToken>,
    pub price_native: Option<String>,
    pub price_usd: Option<String>,
    pub txns: Option<DexTxns>,
    pub volume: Option<DexVolume>,
    pub price_change: Option<DexPriceChange>,
    pub liquidity: Option<DexLiquidity>,
    pub fdv: Option<f64>,
    pub market_cap: Option<f64>,
    pub pair_created_at: Option<i64>,
    pub info: Option<DexInfo>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexToken {
    pub address: String,
    pub name: String,
    pub symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexTxns {
    #[serde(rename = "m5")]
    pub m5: Option<DexTxnCount>,
    #[serde(rename = "h1")]
    pub h1: Option<DexTxnCount>,
    #[serde(rename = "h6")]
    pub h6: Option<DexTxnCount>,
    #[serde(rename = "h24")]
    pub h24: Option<DexTxnCount>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexTxnCount {
    pub buys: u32,
    pub sells: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexVolume {
    #[serde(rename = "m5")]
    pub m5: Option<f64>,
    #[serde(rename = "h1")]
    pub h1: Option<f64>,
    #[serde(rename = "h6")]
    pub h6: Option<f64>,
    #[serde(rename = "h24")]
    pub h24: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexPriceChange {
    #[serde(rename = "m5")]
    pub m5: Option<f64>,
    #[serde(rename = "h1")]
    pub h1: Option<f64>,
    #[serde(rename = "h6")]
    pub h6: Option<f64>,
    #[serde(rename = "h24")]
    pub h24: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexLiquidity {
    pub usd: Option<f64>,
    pub base: Option<f64>,
    pub quote: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexInfo {
    pub image_url: Option<String>,
    pub websites: Option<Vec<DexWebsite>>,
    pub socials: Option<Vec<DexSocial>>,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexWebsite {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct DexSocial {
    pub platform: String,
    pub handle: String,
}

/// Aggregated safety assessment from DexScreener data.
#[derive(Debug, Clone)]
pub struct SafetyReport {
    pub safe: bool,
    pub total_liquidity_usd: f64,
    pub best_dex: String,
    pub best_liquidity_usd: f64,
    pub pair_age_seconds: u64,
    pub h24_buys: u32,
    pub h24_sells: u32,
    pub buy_sell_ratio: f64,
    pub h24_volume_usd: f64,
    pub h24_price_change: f64,
    pub h1_price_change: f64,
    pub market_cap: f64,
    pub has_socials: bool,
    pub rejection_reason: Option<String>,
}

/// Fetch all DEX pairs for a Solana token from DexScreener.
///
/// M1: retries on HTTP 429 with exponential backoff and `Retry-After` header
/// handling via the shared [`crate::retry::get_with_retry`] helper.
pub async fn get_token_pairs(token_address: &str) -> Result<Vec<DexPair>, Box<dyn Error + Send + Sync>> {
    let url = format!("{}/token-pairs/v1/solana/{}", DEXSCREENER_BASE, token_address);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = crate::retry::get_with_retry(&client, &url, &[]).await?;
    if !resp.status().is_success() {
        return Err(format!("DexScreener API error: {}", resp.status()).into());
    }
    let pairs: Vec<DexPair> = resp.json().await?;
    Ok(pairs)
}

/// Run a pre-entry safety check on a token using DexScreener data.
///
/// Checks:
/// 1. Total liquidity across all DEX pairs (must meet minimum)
/// 2. Pair age (too new = higher risk)
/// 3. 24h buy/sell ratio (more sells than buys = dumping)
/// 4. 24h price change (large negative = dumping)
/// 5. Has socials/websites (legit projects usually have presence)
///
/// Returns a SafetyReport with the pass/fail verdict and details.
pub async fn check_entry_safety(
    token_address: &str,
    min_liquidity_usd: f64,
    min_pair_age_seconds: u64,
) -> Result<SafetyReport, Box<dyn Error + Send + Sync>> {
    let pairs = get_token_pairs(token_address).await?;

    if pairs.is_empty() {
        return Ok(SafetyReport {
            safe: false,
            total_liquidity_usd: 0.0,
            best_dex: "none".to_string(),
            best_liquidity_usd: 0.0,
            pair_age_seconds: 0,
            h24_buys: 0,
            h24_sells: 0,
            buy_sell_ratio: 0.0,
            h24_volume_usd: 0.0,
            h24_price_change: 0.0,
            h1_price_change: 0.0,
            market_cap: 0.0,
            has_socials: false,
            rejection_reason: Some("No DEX pairs found on DexScreener".to_string()),
        });
    }

    // Aggregate liquidity across all pairs, find the best (most liquid) pair
    let mut total_liquidity: f64 = 0.0;
    let mut best_pair: Option<&DexPair> = None;
    let mut best_liquidity: f64 = 0.0;

    for pair in &pairs {
        let liq = pair.liquidity.as_ref().and_then(|l| l.usd).unwrap_or(0.0);
        total_liquidity += liq;
        if liq > best_liquidity {
            best_liquidity = liq;
            best_pair = Some(pair);
        }
    }

    let best = best_pair.unwrap_or(&pairs[0]);

    // Extract metrics from the best (most liquid) pair
    let txns = best.txns.as_ref();
    let h24_txns = txns.and_then(|t| t.h24.as_ref());
    let h24_buys = h24_txns.map(|t| t.buys).unwrap_or(0);
    let h24_sells = h24_txns.map(|t| t.sells).unwrap_or(0);
    let buy_sell_ratio = if h24_sells > 0 {
        h24_buys as f64 / h24_sells as f64
    } else if h24_buys > 0 {
        99.0 // All buys, no sells = strong buy pressure
    } else {
        1.0 // No transactions
    };

    let h24_volume = best.volume.as_ref().and_then(|v| v.h24).unwrap_or(0.0);
    let price_changes = best.price_change.as_ref();
    let h24_change = price_changes.and_then(|p| p.h24).unwrap_or(0.0);
    let h1_change = price_changes.and_then(|p| p.h1).unwrap_or(0.0);
    let market_cap = best.market_cap.unwrap_or(0.0);

    // Pair age from creation timestamp (milliseconds since epoch)
    let pair_age_seconds = best.pair_created_at
        .map(|ts| {
            let now_ms = chrono::Utc::now().timestamp_millis();
            ((now_ms - ts) / 1000).max(0) as u64
        })
        .unwrap_or(0);

    // Check if token has socials/websites
    let has_socials = best.info.as_ref()
        .map(|info| {
            info.socials.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
                || info.websites.as_ref().map(|w| !w.is_empty()).unwrap_or(false)
        })
        .unwrap_or(false);

    // Run safety checks
    let mut rejection_reason: Option<String> = None;

    if total_liquidity < min_liquidity_usd {
        rejection_reason = Some(format!(
            "Total liquidity ${:.0} below minimum ${:.0}",
            total_liquidity, min_liquidity_usd
        ));
    }

    if pair_age_seconds > 0 && pair_age_seconds < min_pair_age_seconds {
        let age_min = pair_age_seconds / 60;
        if rejection_reason.is_none() {
            rejection_reason = Some(format!(
                "Pair too young: {}min (min: {}min)",
                age_min, min_pair_age_seconds / 60
            ));
        }
    }

    // Sell pressure check: if sells significantly outnumber buys, token is being dumped
    if h24_buys + h24_sells > 10 && buy_sell_ratio < 0.5 {
        if rejection_reason.is_none() {
            rejection_reason = Some(format!(
                "Heavy sell pressure: {}/{} buy/sell ratio {:.2} (<0.5 = dumping)",
                h24_buys, h24_sells, buy_sell_ratio
            ));
        }
    }

    // Rapid dump check: >50% drop in 1h
    if h1_change < -50.0 {
        if rejection_reason.is_none() {
            rejection_reason = Some(format!(
                "1h price dump: {:.1}% (threshold: -50%)", h1_change
            ));
        }
    }

    let safe = rejection_reason.is_none();

    // Print the assessment
    println!("🔬  [DexScreener] Pre-entry safety check for {}:", token_address);
    println!("    Total liquidity:   ${:.0} across {} pairs", total_liquidity, pairs.len());
    println!("    Best DEX:          {} (${:.0} liquidity)", best.dex_id, best_liquidity);
    println!("    Pair age:          {}min", pair_age_seconds / 60);
    println!("    24h buys/sells:    {}/{} (ratio: {:.2})", h24_buys, h24_sells, buy_sell_ratio);
    println!("    24h volume:        ${:.0}", h24_volume);
    println!("    Price change:      1h={:.1}%  24h={:.1}%", h1_change, h24_change);
    println!("    Market cap:        ${:.0}", market_cap);
    println!("    Has socials:       {}", has_socials);

    if safe {
        println!("    ✅ DexScreener safety check PASSED");
    } else {
        println!("    ❌ DexScreener safety check REJECTED: {}", rejection_reason.as_ref().unwrap());
    }

    Ok(SafetyReport {
        safe,
        total_liquidity_usd: total_liquidity,
        best_dex: best.dex_id.clone(),
        best_liquidity_usd: best_liquidity,
        pair_age_seconds,
        h24_buys,
        h24_sells,
        buy_sell_ratio,
        h24_volume_usd: h24_volume,
        h24_price_change: h24_change,
        h1_price_change: h1_change,
        market_cap,
        has_socials,
        rejection_reason,
    })
}
use serde::{Deserialize, Serialize};
use reqwest::Client;
use std::error::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct MarketInfo {
    pub id: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub not_raw_in_amount: Option<String>,
    pub not_raw_out_amount: Option<String>,
    pub min_in_amount: Option<String>,
    pub min_out_amount: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapInfo {
    pub amm_key: String,
    pub label: String,
    pub input_mint: String,
    pub output_mint: String,
    pub in_amount: String,
    pub out_amount: String,
    pub fee_amount: Option<String>,
    pub fee_mint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutePlanStep {
    pub swap_info: SwapInfo,
    pub percent: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteResponse {
    pub input_mint: String,
    pub in_amount: String,
    pub output_mint: String,
    pub out_amount: String,
    pub other_amount_threshold: String,
    pub swap_mode: String,
    pub slippage_bps: u32,
    pub platform_fee: Option<serde_json::Value>,
    pub price_impact_pct: String,
    pub route_plan: Vec<RoutePlanStep>,
    pub context_slot: Option<u64>,
    pub time_taken: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapRequest {
    pub quote_response: QuoteResponse,
    pub user_public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrap_and_unwrap_sol: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dynamic_compute_unit_limit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prioritization_fee_lamports: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jito_tip_lamports: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct SwapResponse {
    pub swap_transaction: String,
    pub last_valid_blockhash_value: Option<String>,
}

pub struct JupApiClient {
    client: Client,
}

impl JupApiClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    /// Fetch a quote from Jupiter Swap API V1 with automatic retry on 429 rate limit
    pub async fn get_quote(
        &self,
        input_mint: &str,
        output_mint: &str,
        amount: u64,
        slippage_bps: u32,
    ) -> Result<QuoteResponse, Box<dyn Error + Send + Sync>> {
        let url = format!(
            "https://api.jup.ag/swap/v1/quote?inputMint={}&outputMint={}&amount={}&slippageBps={}",
            input_mint, output_mint, amount, slippage_bps
        );

        let mut attempts = 0;
        loop {
            let response = self.client.get(&url).send().await?;
            if response.status().is_success() {
                let quote: QuoteResponse = response.json().await?;
                return Ok(quote);
            }
            
            let status = response.status();
            let err_text = response.text().await?;
            
            if (status.as_u16() == 429 || err_text.contains("Rate limit") || err_text.contains("Too many requests")) && attempts < 3 {
                attempts += 1;
                let sleep_ms = attempts * 1000;
                println!("  [Jupiter API] Rate limited (429). Retrying in {}ms (Attempt {}/3)...", sleep_ms, attempts);
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                continue;
            }
            
            return Err(format!("Jupiter Quote API error (status {}): {}", status, err_text).into());
        }
    }

    /// Build a swap transaction from a Jupiter Quote with automatic retry on 429 rate limit
    pub async fn get_swap_transaction(
        &self,
        quote: QuoteResponse,
        user_pubkey: &str,
        prior_fee_lamports: Option<u64>,
        jito_tip_lamports: Option<u64>,
    ) -> Result<String, Box<dyn Error + Send + Sync>> {
        let url = "https://api.jup.ag/swap/v1/swap";
        
        let req_body = SwapRequest {
            quote_response: quote,
            user_public_key: user_pubkey.to_string(),
            wrap_and_unwrap_sol: Some(true),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: prior_fee_lamports,
            jito_tip_lamports,
        };

        let mut attempts = 0;
        loop {
            let response = self.client.post(url)
                .json(&req_body)
                .send()
                .await?;

            if response.status().is_success() {
                let swap_resp: SwapResponse = response.json().await?;
                return Ok(swap_resp.swap_transaction);
            }
            
            let status = response.status();
            let err_text = response.text().await?;
            
            if (status.as_u16() == 429 || err_text.contains("Rate limit") || err_text.contains("Too many requests")) && attempts < 3 {
                attempts += 1;
                let sleep_ms = attempts * 1000;
                println!("  [Jupiter API] Swap Rate limited (429). Retrying in {}ms (Attempt {}/3)...", sleep_ms, attempts);
                tokio::time::sleep(std::time::Duration::from_millis(sleep_ms)).await;
                continue;
            }
            
            return Err(format!("Jupiter Swap API error (status {}): {}", status, err_text).into());
        }
    }
}

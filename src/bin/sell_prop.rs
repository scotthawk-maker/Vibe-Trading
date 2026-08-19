use std::env;
use std::error::Error;
use solana_sdk::{
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SwapRequest {
    quote_response: serde_json::Value,
    user_public_key: String,
    wrap_and_unwrap_sol: Option<bool>,
    dynamic_compute_unit_limit: Option<bool>,
    prioritization_fee_lamports: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwapResponse {
    swap_transaction: String,
}

fn parse_keypair(s: &str) -> Option<Keypair> {
    if s.trim().starts_with('[') && s.trim().ends_with(']') {
        if let Ok(bytes) = serde_json::from_str::<Vec<u8>>(s) {
            if let Ok(keypair) = Keypair::try_from(bytes.as_slice()) {
                return Some(keypair);
            }
        }
    }
    if let Ok(bytes) = bs58::decode(s).into_vec() {
        if let Ok(keypair) = Keypair::try_from(bytes.as_slice()) {
            return Some(keypair);
        }
    }
    None
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    
    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    let wallet_private_key = env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY missing");
    
    let keypair = parse_keypair(&wallet_private_key).expect("Failed to parse private key");
    let wallet_pubkey_str = keypair.pubkey().to_string();
    
    println!("Loading Wallet Public Key: {}", wallet_pubkey_str);

    let client = reqwest::Client::new();

    // L1: parameterize the previously hardcoded sell target via env vars.
    let sell_mint = env::var("SELL_MINT").unwrap_or_else(|_| "GyzmAHqaujH5nwngLcTucAB8JAyNaL55e16kHyvEpump".to_string());
    let sell_amount: u64 = env::var("SELL_AMOUNT").unwrap_or_else(|_| "7366694314595".to_string()).parse().expect("Invalid SELL_AMOUNT integer");
    let sell_slippage_bps: u32 = env::var("SELL_SLIPPAGE_BPS").unwrap_or_else(|_| "300".to_string()).parse().expect("Invalid SELL_SLIPPAGE_BPS");

    // Fetch dynamic priority fee from Helius (Tier 3 grounded: getPriorityFeeEstimate > hardcoded)
    // H6: reqwest POST uses a clean URL (no api-key in query string) with an
    // Authorization: Bearer header so the key never appears in the URL/log.
    let rpc_url_clean = "https://mainnet.helius-rpc.com";
    let fee_payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": "1",
        "method": "getPriorityFeeEstimate",
        "params": [{
            "accountKeys": ["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4"],
            "options": { "priorityLevel": "VeryHigh" }
        }]
    });
    let priority_fee_lamports: u64 = match client
        .post(rpc_url_clean)
        .header("Authorization", format!("Bearer {}", helius_api_key))
        .json(&fee_payload)
        .send()
        .await
    {
        Ok(resp) => {
            let fee_json: serde_json::Value = resp.json().await.unwrap_or_default();
            let estimate = fee_json["result"]["priorityFeeEstimate"].as_f64().unwrap_or(5000.0);
            // Convert microlamports to lamports: assume ~300k CU per swap
            let lamports = ((300_000.0 * estimate) / 1_000_000.0).round() as u64;
            std::cmp::max(1_000, std::cmp::min(lamports, 200_000))
        }
        Err(_) => 5_000, // fallback to conservative 5000 lamports
    };
    println!("Dynamic priority fee: {} lamports (VeryHigh)", priority_fee_lamports);

    // Query Jupiter Quote
    println!("Fetching quote from Jupiter for PROP -> SOL...");
    let url = format!("https://lite-api.jup.ag/swap/v1/quote?inputMint={}&outputMint=So11111111111111111111111111111111111111112&amount={}&slippageBps={}", sell_mint, sell_amount, sell_slippage_bps);
    let response = client.get(url).send().await?;
    if !response.status().is_success() {
        let err = response.text().await?;
        return Err(format!("Quote failed: {}", err).into());
    }
    let quote: serde_json::Value = response.json().await?;
    let out_amount_str = quote["outAmount"].as_str().unwrap_or("0");
    println!("Quote retrieved! Expected SOL return: {} SOL", out_amount_str.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0);
    
    // Build Swap Request
    println!("Fetching swap transaction from Jupiter...");
    let swap_url = "https://lite-api.jup.ag/swap/v1/swap";
    let req_body = SwapRequest {
        quote_response: quote,
        user_public_key: wallet_pubkey_str,
        wrap_and_unwrap_sol: Some(true),
        dynamic_compute_unit_limit: Some(true),
        prioritization_fee_lamports: Some(priority_fee_lamports), // Dynamic VeryHigh fee from Helius getPriorityFeeEstimate
    };
    let response = client.post(swap_url).json(&req_body).send().await?;
    if !response.status().is_success() {
        let err = response.text().await?;
        return Err(format!("Swap failed: {}", err).into());
    }
    let swap_resp: SwapResponse = response.json().await?;
    
    // Deserialize & Sign
    let tx_bytes = BASE64_STANDARD.decode(&swap_resp.swap_transaction)?;
    let unsigned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
    let signed_tx = VersionedTransaction::try_new(unsigned_tx.message, &[&keypair])?;
    
    // Send Transaction
    println!("Submitting live sell transaction on Mainnet via Helius...");
    // H6: RpcClient (solana_client) does not support custom headers, so the
    // key remains in the query string for this library call only.  The reqwest
    // POST above uses the Authorization header.  This URL is never logged.
    let rpc_client_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_client_url, CommitmentConfig::confirmed());
    
    match rpc_client.send_and_confirm_transaction(&signed_tx) {
        Ok(sig) => {
            println!("🎉 SELL ORDER EXECUTED SUCCESSFULLY!");
            println!("   Signature: https://orbmarkets.io/tx/{}", sig);
        }
        Err(e) => {
            return Err(format!("Live transaction submission failed: {:?}", e).into());
        }
    }
    
    Ok(())
}

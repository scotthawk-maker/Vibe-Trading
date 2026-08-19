use std::env;
use std::error::Error;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::VersionedTransaction,
    instruction::{Instruction, AccountMeta},
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde::Serialize;
use rusqlite::Connection;

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

async fn nuke_and_close(
    mint_pubkey: Pubkey,
    ata: Pubkey,
    prog_id: Pubkey,
    keypair: &Keypair,
    rpc_client: &RpcClient,
) {
    let wallet_pubkey = keypair.pubkey();
    
    // Check balance
    if let Ok(balance_resp) = rpc_client.get_token_account_balance(&ata) {
        let balance_amount = balance_resp.amount.parse::<u64>().unwrap_or(0);
        let mut ixs = Vec::new();
        
        let is_2022 = prog_id == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse::<Pubkey>().unwrap();
        
        if balance_amount > 0 {
            println!("🧹  [Nuke] Found dust of {} base units on {}. Building Burn...", balance_amount, ata);
            let mut burn_data = vec![if is_2022 { 8 } else { 7 }];
            burn_data.extend_from_slice(&balance_amount.to_le_bytes());
            
            ixs.push(Instruction {
                program_id: prog_id,
                accounts: vec![
                    AccountMeta::new(ata, false),
                    AccountMeta::new(mint_pubkey, false),
                    AccountMeta::new_readonly(wallet_pubkey, true),
                ],
                data: burn_data,
            });
        }
        
        println!("🧹  [Nuke] Building CloseAccount instruction for {}...", ata);
        ixs.push(Instruction {
            program_id: prog_id,
            accounts: vec![
                AccountMeta::new(ata, false),
                AccountMeta::new(wallet_pubkey, false),
                AccountMeta::new_readonly(wallet_pubkey, true),
            ],
            data: vec![9],
        });
        
        if let Ok(recent_blockhash) = rpc_client.get_latest_blockhash() {
            let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                &ixs,
                Some(&wallet_pubkey),
                &[&keypair],
                recent_blockhash,
            );
            if let Ok(sig) = rpc_client.send_and_confirm_transaction(&tx) {
                println!("🎉  [Nuke Success] Closed account {} and reclaimed rent!", ata);
                println!("   Signature: https://orbmarkets.io/tx/{}", sig);
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    dotenvy::dotenv().ok();
    
    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    let wallet_private_key = env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY missing");
    
    let keypair = parse_keypair(&wallet_private_key).expect("Failed to parse private key");
    let wallet_pubkey_str = keypair.pubkey().to_string();
    let wallet_pubkey = keypair.pubkey();
    
    // H6: RpcClient (solana_client) does not support custom headers, so the
    // key remains in the query string for this library call only.  The reqwest
    // POST below uses a clean URL with an Authorization: Bearer header so the
    // key never appears in a URL or log.  This URL is never logged.
    let rpc_client_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_client_url.clone(), CommitmentConfig::confirmed());
    let client = reqwest::Client::new();

    // Fetch dynamic priority fee from Helius (Tier 3 grounded: getPriorityFeeEstimate > hardcoded)
    // H6: reqwest POST uses a clean URL (no api-key in query string) with an
    // Authorization: Bearer header.
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
            let lamports = ((300_000.0 * estimate) / 1_000_000.0).round() as u64;
            std::cmp::max(1_000, std::cmp::min(lamports, 200_000))
        }
        Err(_) => 5_000,
    };
    println!("Dynamic priority fee: {} lamports (VeryHigh)", priority_fee_lamports);

    // Open DB connection to clean up state
    let db_conn = Connection::open("trades.db")?;
    
    // Query active sessions from grid_sessions table dynamically
    let mut stmt = db_conn.prepare("SELECT token_address, token_name FROM grid_sessions WHERE status = 'ACTIVE'")?;
    let targets: Vec<(String, String)> = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?
    .filter_map(|r| r.ok())
    .collect();
    
    println!("=== INITIATING DYNAMIC EMERGENCY LIQUIDATION PORTFOLIO EXIT ===");
    println!("Found {} active sessions to close.", targets.len());
    
    for (mint_str, name) in targets {
        let mint_pubkey = match mint_str.parse::<Pubkey>() {
            Ok(pk) => pk,
            Err(_) => {
                println!("⚠️   Invalid mint address format: {}. Skipping.", mint_str);
                continue;
            }
        };

        // Query Mint Account program ID directly from RPC
        let prog_id = match rpc_client.get_account(&mint_pubkey) {
            Ok(acc) => acc.owner,
            Err(e) => {
                println!("⚠️   Could not fetch mint account details for {} ({}): {:?}", name, mint_str, e);
                continue;
            }
        };

        let ata_program_id = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse::<Pubkey>()?;
        
        // Derive ATA
        let (ata_pubkey, _) = Pubkey::find_program_address(
            &[wallet_pubkey.as_ref(), prog_id.as_ref(), mint_pubkey.as_ref()],
            &ata_program_id,
        );
        
        // Fetch balance
        let balance = match rpc_client.get_token_account_balance(&ata_pubkey) {
            Ok(b) => b.amount.parse::<u64>().unwrap_or(0),
            Err(_) => 0,
        };
        
        if balance == 0 {
            println!("ℹ️   Balance of {} ({}) is 0. Cleaning DB state...", name, mint_str);
            db_conn.execute(
                "UPDATE trades SET status = 'CLOSED', sell_signature = 'EMERGENCY_EXIT_ZERO_BALANCE' WHERE token_address = ?1 AND status = 'OPEN'",
                [&mint_str],
            )?;
            db_conn.execute(
                "UPDATE grid_sessions SET status = 'COMPLETED' WHERE token_address = ?1",
                [&mint_str],
            )?;
            continue;
        }
        
        println!("\n🔥  Selling balance of {} ({}) | Balance: {} base units", name, mint_str, balance);
        
        // Get Jupiter quote to swap back to SOL
        let url = format!(
            "https://lite-api.jup.ag/swap/v1/quote?inputMint={}&outputMint=So11111111111111111111111111111111111111112&amount={}&slippageBps=500", // 5.0% slippage to guarantee it clears!
            mint_str, balance
        );
        
        let response = client.get(&url).send().await?;
        if !response.status().is_success() {
            println!("⚠️   Failed to get Jupiter quote for {}. Skipping.", name);
            continue;
        }
        
        let quote: serde_json::Value = response.json().await?;
        
        // Fetch Swap transaction
        let swap_url = "https://lite-api.jup.ag/swap/v1/swap";
        let req_body = SwapRequest {
            quote_response: quote,
            user_public_key: wallet_pubkey_str.clone(),
            wrap_and_unwrap_sol: Some(true),
            dynamic_compute_unit_limit: Some(true),
            prioritization_fee_lamports: Some(priority_fee_lamports), // Dynamic VeryHigh fee from Helius getPriorityFeeEstimate
        };
        
        let response = client.post(swap_url).json(&req_body).send().await?;
        if !response.status().is_success() {
            println!("⚠️   Failed to build swap transaction for {}. Skipping.", name);
            continue;
        }
        
        let swap_resp: SwapResponse = response.json().await?;
        
        // Deserialize & Sign
        let tx_bytes = BASE64_STANDARD.decode(&swap_resp.swap_transaction)?;
        let unsigned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
        let signed_tx = VersionedTransaction::try_new(unsigned_tx.message, &[&keypair])?;
        
        // Send Transaction
        println!("🚀  Submitting live sell transaction for {}...", name);
        match rpc_client.send_and_confirm_transaction(&signed_tx) {
            Ok(sig) => {
                println!("🎉  SELL SUCCESSFUL for {}!", name);
                println!("   Signature: https://orbmarkets.io/tx/{}", sig);
                
                // Mark closed in database
                db_conn.execute(
                    "UPDATE trades SET status = 'CLOSED', sell_signature = ?1 WHERE token_address = ?2 AND status = 'OPEN'",
                    [sig.to_string(), mint_str.to_string()],
                )?;
                db_conn.execute(
                    "UPDATE grid_sessions SET status = 'COMPLETED' WHERE token_address = ?1",
                    [mint_str.to_string()],
                )?;
                
                // Run Auto-Nuke Recycler
                nuke_and_close(mint_pubkey, ata_pubkey, prog_id, &keypair, &rpc_client).await;
            }
            Err(e) => {
                println!("⚠️   Failed to submit sell transaction for {}: {:?}", name, e);
            }
        }
    }
    
    println!("\n=== EMERGENCY LIQUIDATION COMPLETE ===");
    Ok(())
}

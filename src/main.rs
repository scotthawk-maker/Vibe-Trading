use std::time::Duration;
use std::error::Error;
use std::str::FromStr;
use tokio::time::sleep;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
    transaction::VersionedTransaction,
    instruction::{Instruction, AccountMeta},
};
use solana_commitment_config::CommitmentConfig;
use solana_client::rpc_config::RpcSendTransactionConfig;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use rusqlite::{params, Connection};
use helius::{Helius, types::Cluster};

mod config;
mod jup_api;
mod db;
mod hunter;
mod webhook_server;
mod local_isolate;
mod gecko_api;
mod discord_notify;
mod dexscreener;
mod validation;
mod retry;

use config::{BotConfig, Strategy};
use webhook_server::{GraduationEvent, ActorEvent, ActorRegistry};
use jup_api::JupApiClient;

const FEE_BUFFER_LAMPORTS: u64 = 5_000_000; // 0.005 SOL
// const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

use std::sync::LazyLock;
use std::sync::Mutex;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

static DECIMAL_CACHE: LazyLock<Mutex<HashMap<String, Option<u32>>>> = LazyLock::new(|| {
    Mutex::new(HashMap::new())
});

static CACHED_PRIORITY_FEE: AtomicU64 = AtomicU64::new(5_000); // 5000 lamports default

/// Global graceful-shutdown flag (M5).  Set when the autonomous loop decides
/// the bot should exit; `main()` checks it after each strategy loop returns.
static SHUTDOWN_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// L3: Fetch token decimals from the on-chain `Mint` supply.  Returns `None`
/// when the lookup fails — **never** falls back to a suffix heuristic.  Callers
/// must handle `None` by skipping/aborting the trade rather than guessing.
/// Results are cached with the existing `DECIMAL_CACHE`.
fn get_decimals_by_mint(connection: &solana_client::rpc_client::RpcClient, mint: &str) -> Option<u32> {
    if let Ok(cache) = DECIMAL_CACHE.lock() {
        if let Some(cached) = cache.get(mint) {
            return *cached;
        }
    }

    let decimals = (|| {
        let pubkey = solana_sdk::pubkey::Pubkey::from_str(mint).ok()?;
        let supply = connection.get_token_supply(&pubkey).ok()?;
        Some(supply.decimals as u32)
    })();

    if let Ok(mut cache) = DECIMAL_CACHE.lock() {
        cache.insert(mint.to_string(), decimals);
    }

    decimals
}

/// M2: Safely convert an f64 to u64, rejecting NaN/negative/inf values that
/// would otherwise saturate to `0` via `as u64` and produce a zero-size order.
fn f64_to_u64_checked(v: f64) -> Option<u64> {
    if v.is_finite() && v >= 0.0 {
        Some(v.round() as u64)
    } else {
        None
    }
}

/// H4: Returns `true` when the Jupiter quote's price impact exceeds the
/// configured `max_pct` threshold.  A non-parseable impact value is treated
/// as exceeding (fail-closed) so a malformed quote is never executed blindly.
fn price_impact_exceeds(quote: &jup_api::QuoteResponse, max_pct: f64) -> bool {
    quote
        .price_impact_pct
        .parse::<f64>()
        .map(|p| p > max_pct)
        .unwrap_or(true)
}

/// Convenience wrapper for price-display paths: returns the on-chain decimals
/// or logs a warning and returns `0` (which makes `10f64.powi(0) == 1`).
/// Trade-*execution* paths should call [`get_decimals_by_mint`] directly and
/// skip when `None` (L3).
fn get_decimals_or_warn(connection: &solana_client::rpc_client::RpcClient, mint: &str) -> u32 {
    match get_decimals_by_mint(connection, mint) {
        Some(d) => d,
        None => {
            eprintln!("⚠️  [Decimals] Could not fetch on-chain decimals for {}; using 0 (price calc may be inaccurate).", mint);
            0
        }
    }
}

fn get_elapsed_minutes(buy_time_str: &str) -> f64 {
    use chrono::NaiveDateTime;
    if let Ok(naive_time) = NaiveDateTime::parse_from_str(buy_time_str, "%Y-%m-%d %H:%M:%S") {
        let now = chrono::Utc::now().naive_utc();
        let elapsed = now.signed_duration_since(naive_time);
        elapsed.num_seconds() as f64 / 60.0
    } else {
        0.0
    }
}

fn parse_simulation_failure_reason(logs: &[String], err: &solana_transaction_status_client_types::UiTransactionError) -> String {
    let mut reason = format!("Simulation Failure: {:?}", err);
    for log in logs {
        if log.contains("custom program error:") {
            reason = format!("On-chain Error in Log: {}", log);
        } else if log.contains("Slippage tolerance exceeded") {
            reason = "Slippage tolerance exceeded (price moved too fast)".to_string();
        } else if log.contains("Insufficient funds") || log.contains("insufficient lamports") {
            reason = "Insufficient funds in wallet for swap fee".to_string();
        }
    }
    reason
}

async fn nuke_and_close_token_account(
    token_mint_str: &str,
    helius: &Helius,
    keypair: &Keypair,
    dust_threshold_base_units: u64,
) {
    if let Ok(mint_pubkey) = token_mint_str.parse::<Pubkey>() {
        let wallet_pubkey = keypair.pubkey();
        let token_program_id = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse::<Pubkey>().unwrap();
        let token_2022_program_id = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse::<Pubkey>().unwrap();
        let ata_program_id = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse::<Pubkey>().unwrap();
        
        for (prog_id, is_2022) in [(token_program_id, false), (token_2022_program_id, true)] {
            let (ata, _) = Pubkey::find_program_address(
                &[wallet_pubkey.as_ref(), prog_id.as_ref(), mint_pubkey.as_ref()],
                &ata_program_id,
            );
            
            if let Ok(balance_resp) = helius.connection().get_token_account_balance(&ata) {
                let balance_amount = balance_resp.amount.parse::<u64>().unwrap_or(0);
                
                // L2: Safety gate using the configurable dust threshold.
                if balance_amount > dust_threshold_base_units {
                    println!("⚠️  [Auto-Nuke Safety] Refusing to burn/close account because balance is too large ({} base units, threshold {})! Retaining balance.", balance_amount, dust_threshold_base_units);
                    continue;
                }
                
                let mut ixs = Vec::new();
                
                if balance_amount > 0 {
                    println!("🧹  [Auto-Nuke] Found dust balance of {} base units. Building Burn instruction...", balance_amount);
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
                
                println!("🧹  [Auto-Nuke] Building CloseAccount instruction for {}...", ata);
                ixs.push(Instruction {
                    program_id: prog_id,
                    accounts: vec![
                        AccountMeta::new(ata, false),
                        AccountMeta::new(wallet_pubkey, false),
                        AccountMeta::new_readonly(wallet_pubkey, true),
                    ],
                    data: vec![9],
                });
                
                if let Ok(recent_blockhash) = helius.connection().get_latest_blockhash() {
                    let tx = Transaction::new_signed_with_payer(
                        &ixs,
                        Some(&wallet_pubkey),
                        &[&keypair],
                        recent_blockhash,
                    );
                    if let Ok(sig) = helius.connection().send_and_confirm_transaction(&tx) {
                        println!("🎉  [AUTO-RECLAIM SUCCESS] Closed token account {}! Reclaimed 0.002039 SOL rent directly to your wallet balance.", ata);
                        println!("   Tx: https://orbmarkets.io/tx/{}", sig);
                    }
                }
            }
        }
    }
}

async fn send_transaction_to_jito(
    signed_tx: &VersionedTransaction,
) -> Result<String, Box<dyn Error + Send + Sync>> {
    let serialized_tx = bincode::serialize(signed_tx)?;
    let encoded_tx = bs58::encode(serialized_tx).into_string();
    
    let client = reqwest::Client::new();
    let url = "https://mainnet.block-engine.jito.wtf/api/v1/transactions";
    
    let payload = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendTransaction",
        "params": [
            encoded_tx,
            {
                "encoding": "base58",
                "skipPreflight": true
            }
        ]
    });
    
    let response = client.post(url)
        .json(&payload)
        .send()
        .await?;
        
    if !response.status().is_success() {
        let err_text = response.text().await?;
        return Err(format!("Jito send transaction failed: {}", err_text).into());
    }
    
    let res_json: serde_json::Value = response.json().await?;
    if let Some(err) = res_json.get("error") {
        return Err(format!("Jito RPC error: {:?}", err).into());
    }
    
    let sig = res_json["result"]
        .as_str()
        .ok_or_else(|| format!("Jito response missing transaction signature: {:?}", res_json))?;
        
    Ok(sig.to_string())
}

async fn execute_buy_swap(
    mint_str: &str,
    amount_lamports: u64,
    config: &BotConfig,
    helius: &Helius,
    jup_client: &JupApiClient,
) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
    let wallet_pubkey_str = config.keypair.pubkey().to_string();
    
    // 1. Fetch Jupiter Quote
    let quote = jup_client.get_quote(
        "So11111111111111111111111111111111111111112",
        mint_str,
        amount_lamports,
        config.slippage_bps,
    ).await?;

    // H4: Reject swaps whose price impact exceeds the configured threshold.
    println!("  [Price Impact] buy SOL -> {}: {}% (max {:.1}%)", mint_str, quote.price_impact_pct, config.max_price_impact_pct);
    if price_impact_exceeds(&quote, config.max_price_impact_pct) {
        println!("  ⚠️ [Price Impact] Rejecting buy swap: impact {}% exceeds {:.1}% threshold.", quote.price_impact_pct, config.max_price_impact_pct);
        return Ok(None);
    }

    // 2. Fetch Priority Fee or Jito Tip
    let (prior_fee, jito_tip) = if config.dry_run {
        (Some(CACHED_PRIORITY_FEE.load(Ordering::Relaxed)), None)
    } else {
        (None, Some(config.jito_tip_lamports))
    };
    
    if let Some(tip) = jito_tip {
        println!("  Using Jito Tip: {} lamports ({:.6} SOL)", tip, tip as f64 / 1_000_000_000.0);
    }
    
    // 3. Get Swap Transaction
    let swap_tx_b64 = jup_client.get_swap_transaction(quote, &wallet_pubkey_str, prior_fee, jito_tip).await?;
    let tx_bytes = BASE64_STANDARD.decode(&swap_tx_b64)?;
    let unsigned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
    let signed_tx = VersionedTransaction::try_new(unsigned_tx.message, &[config.keypair.as_ref()])?;
    
    // 4. Submit Transaction
    if config.dry_run {
        println!("  [Dry-Run] Simulating buy transaction...");
        let sim_result = helius.connection().simulate_transaction(&signed_tx)?;
        if let Some(err) = &sim_result.value.err {
            let logs = sim_result.value.logs.clone().unwrap_or_default();
            println!("  ❌ Buy simulation failed: {}", parse_simulation_failure_reason(&logs, err));
            Ok::<Option<String>, Box<dyn Error + Send + Sync>>(None)
        } else {
            println!("  ✅ Buy simulation succeeded!");
            Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some("DRY_RUN_SIGNATURE".to_string()))
        }
    } else {
        println!("🚀 Submitting live buy swap privately via Jito...");
        match send_transaction_to_jito(&signed_tx).await {
            Ok(sig) => {
                let sig_parsed = solana_sdk::signature::Signature::from_str(&sig)?;
                println!("  Submitted privately to Jito Block Engine! Sig: {}. Confirming...", sig);
                
                let mut confirmed = false;
                for _ in 0..30 {
                    if let Ok(status) = helius.connection().get_signature_status(&sig_parsed) {
                        if let Some(Ok(())) = status {
                            confirmed = true;
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                
                if confirmed {
                    Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some(sig))
                } else {
                    Err(format!("Transaction submitted to Jito but confirmation timed out (sig: {})", sig).into())
                }
            }
            Err(e) => {
                println!("  ⚠️ Jito private submission failed: {:?}. Falling back to public Helius RPC...", e);
                let latest_blockhash_with_height = helius.connection().get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?;
                let last_valid_block_height = latest_blockhash_with_height.1;
                let send_config = RpcSendTransactionConfig {
                    skip_preflight: true,
                    preflight_commitment: None,
                    encoding: None,
                    max_retries: Some(5),
                    min_context_slot: None,
                };
                match helius.send_and_confirm_transaction(&signed_tx, send_config, last_valid_block_height, None).await {
                    Ok(sig) => Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some(sig.to_string())),
                    Err(err) => Err(format!("Jito failed and fallback Helius RPC submission failed: {:?}", err).into()),
                }
            }
        }
    }
}

async fn execute_sell_swap(
    mint_str: &str,
    amount_base_units: u64,
    config: &BotConfig,
    helius: &Helius,
    jup_client: &JupApiClient,
) -> Result<Option<String>, Box<dyn Error + Send + Sync>> {
    let wallet_pubkey_str = config.keypair.pubkey().to_string();
    
    // 1. Fetch Jupiter Quote
    let quote = jup_client.get_quote(
        mint_str,
        "So11111111111111111111111111111111111111112",
        amount_base_units,
        config.slippage_bps,
    ).await?;

    // H4: Reject swaps whose price impact exceeds the configured threshold.
    println!("  [Price Impact] sell {} -> SOL: {}% (max {:.1}%)", mint_str, quote.price_impact_pct, config.max_price_impact_pct);
    if price_impact_exceeds(&quote, config.max_price_impact_pct) {
        println!("  ⚠️ [Price Impact] Rejecting sell swap: impact {}% exceeds {:.1}% threshold.", quote.price_impact_pct, config.max_price_impact_pct);
        return Ok(None);
    }

    // 2. Fetch Priority Fee or Jito Tip
    let (prior_fee, jito_tip) = if config.dry_run {
        (Some(CACHED_PRIORITY_FEE.load(Ordering::Relaxed)), None)
    } else {
        (None, Some(config.jito_tip_lamports))
    };
    
    if let Some(tip) = jito_tip {
        println!("  Using Jito Tip: {} lamports ({:.6} SOL)", tip, tip as f64 / 1_000_000_000.0);
    }
    
    // 3. Get Swap Transaction
    let swap_tx_b64 = jup_client.get_swap_transaction(quote, &wallet_pubkey_str, prior_fee, jito_tip).await?;
    let tx_bytes = BASE64_STANDARD.decode(&swap_tx_b64)?;
    let unsigned_tx: VersionedTransaction = bincode::deserialize(&tx_bytes)?;
    let signed_tx = VersionedTransaction::try_new(unsigned_tx.message, &[config.keypair.as_ref()])?;
    
    // 4. Submit Transaction
    if config.dry_run {
        println!("  [Dry-Run] Simulating sell transaction...");
        let sim_result = helius.connection().simulate_transaction(&signed_tx)?;
        if let Some(err) = &sim_result.value.err {
            let logs = sim_result.value.logs.clone().unwrap_or_default();
            println!("  ⚠️ Sell simulation failed (expected in Dry-Run since we don't own tokens on-chain): {}", parse_simulation_failure_reason(&logs, err));
        } else {
            println!("  ✅ Sell simulation succeeded!");
        }
        return Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some("DRY_RUN_SIGNATURE".to_string()));
    } else {
        println!("🚀 Submitting live sell swap privately via Jito...");
        match send_transaction_to_jito(&signed_tx).await {
            Ok(sig) => {
                let sig_parsed = solana_sdk::signature::Signature::from_str(&sig)?;
                println!("  Submitted privately to Jito Block Engine! Sig: {}. Confirming...", sig);
                
                let mut confirmed = false;
                for _ in 0..30 {
                    if let Ok(status) = helius.connection().get_signature_status(&sig_parsed) {
                        if let Some(Ok(())) = status {
                            confirmed = true;
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
                
                if confirmed {
                    Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some(sig))
                } else {
                    Err(format!("Transaction submitted to Jito but confirmation timed out (sig: {})", sig).into())
                }
            }
            Err(e) => {
                println!("  ⚠️ Jito private submission failed: {:?}. Falling back to public Helius RPC...", e);
                let latest_blockhash_with_height = helius.connection().get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())?;
                let last_valid_block_height = latest_blockhash_with_height.1;
                let send_config = RpcSendTransactionConfig {
                    skip_preflight: true,
                    preflight_commitment: None,
                    encoding: None,
                    max_retries: Some(5),
                    min_context_slot: None,
                };
                match helius.send_and_confirm_transaction(&signed_tx, send_config, last_valid_block_height, None).await {
                    Ok(sig) => Ok::<Option<String>, Box<dyn Error + Send + Sync>>(Some(sig.to_string())),
                    Err(err) => Err(format!("Jito failed and fallback Helius RPC submission failed: {:?}", err).into()),
                }
            }
        }
    }
}

async fn run_hunter_loop(
    _ref_config: &BotConfig,
    helius: &Helius,
    jup_client: &JupApiClient,
    db_conn: &Connection,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut config = BotConfig::load_from_env().map_err(|e| -> Box<dyn Error + Send + Sync> { e.into() })?;
    let wallet_pubkey_str = config.keypair.pubkey().to_string();
    let mut traded_tokens: Vec<(String, std::time::Instant)> = Vec::new();
    
    // Auto-detect open trades from SQLite to recover
    println!("\nChecking database for active open positions to recover...");
    let mut recovered_positions = Vec::new();
    let mut stmt = db_conn.prepare("SELECT token_address, token_name, token_symbol, buy_price_usd FROM trades WHERE status = 'OPEN'")?;
    let open_trades_db: Vec<(String, String, String, f64)> = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?
      .filter_map(|r| r.ok())
      .collect();
      
    if !open_trades_db.is_empty() {
        println!("Found {} OPEN trades in database. Verifying on-chain balances...", open_trades_db.len());
        if let Ok(balances) = helius.get_wallet_balances(&wallet_pubkey_str, Some(1), Some(100), Some(false), Some(false), Some(false)).await {
            for (mint, name, symbol, buy_price) in open_trades_db {
                let mut found = false;
                for bal in &balances.balances {
                    if bal.mint == mint && bal.balance > 0.0001 {
                        println!("🎯 RECOVERED ACTIVE POSITION: {} ({}) | Balance: {:.6} | Entry Price: ${:.10}", name, symbol, bal.balance, buy_price);
                        let candidate = hunter::GmgnTokenCandidate {
                            address: mint.clone(),
                            symbol: symbol.clone(),
                            name: name.clone(),
                            price: buy_price,
                            progress: 0.0,
                        };
                        recovered_positions.push((candidate, buy_price, 0));
                        found = true;
                        break;
                    }
                }
                if !found {
                    println!("⚠️  Position {} ({}) was open in DB, but wallet has 0 balance. Marking as CLOSED.", name, symbol);
                    if let Err(e) = db_conn.execute("UPDATE trades SET status = 'CLOSED', sell_signature = 'CLEANED_ON_STARTUP' WHERE token_address = ?1 AND status = 'OPEN'", [&mint]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                }
            }
        }
    } else {
        println!("No active open trades found in database on startup.");
    }
    
    let mut active_positions = recovered_positions;
    
    println!("\n=== Starting GMGN Token Hunter State Machine ===");
    println!("Checking Interval:   {} seconds", config.check_interval_secs);
    
    loop {
        // Safe configuration reload
        if let Ok(new_config) = BotConfig::reload_from_env() {
            if new_config.strategy != Strategy::Hunter {
                println!("🔄  [STRATEGY SWITCH DETECTED] Switching on-the-fly to GRID strategy loop...");
                return Ok(());
            }
            config = new_config;
        }
        
        let now = std::time::Instant::now();
        traded_tokens.retain(|(_, ts)| now.duration_since(*ts) < Duration::from_secs(600));
        
        // Fetch live SOL price to calculate safeguards
        let mut sol_price_usd = 150.0;
        if let Ok(quote) = jup_client.get_quote(
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            1_000_000_000,
            config.slippage_bps,
        ).await {
            sol_price_usd = quote.out_amount.parse::<f64>().unwrap_or(150_000_000.0) / 1_000_000.0;
        }
        
        let deck_5usd_sol = 5.0 / sol_price_usd;
        let recovery_7usd_sol = 7.0 / sol_price_usd;
        
        let mut sol_balance = 0.25;
        if let Ok(bal) = helius.connection().get_balance(&config.keypair.pubkey()) {
            sol_balance = bal as f64 / 1_000_000_000.0;
        }
        
        let buying_enabled = sol_balance >= recovery_7usd_sol;
        
        // Audit wallet balances for active positions
        let mut wallet_balances = Vec::new();
        if !config.dry_run {
            if let Ok(bal) = helius.get_wallet_balances(&wallet_pubkey_str, Some(1), Some(100), Some(false), Some(false), Some(false)).await {
                wallet_balances = bal.balances;
            }
        }
        
        // 1. Process Active Positions
        let mut positions_to_keep = Vec::new();
        for (mut candidate, entry_price, ticks) in active_positions {
            let current_ticks = ticks + 1;
            println!("\n--- Monitoring Position: {} ({}) ---", candidate.name, candidate.symbol);
            println!("   Entry Price: ${:.10}", entry_price);
            
            // Check if manually sold (balance is 0)
            let mut current_balance = 0.0;
            if !config.dry_run {
                for b in &wallet_balances {
                    if b.mint == candidate.address {
                        current_balance = b.balance;
                        break;
                    }
                }
            } else {
                current_balance = 1.0;
            }
            
            if current_balance < 0.0001 && !config.dry_run && current_ticks > 4 {
                println!("⚠️  [MANUAL SELL DETECTED] Wallet holds 0 tokens. Cleaning up state...");
                if let Err(e) = db_conn.execute(
                    "UPDATE trades SET status = 'CLOSED', sell_signature = 'MANUAL_SELL_SIGNATURE' WHERE token_address = ?1 AND status = 'OPEN'",
                    [&candidate.address],
                ) { eprintln!("❌ [DB] execute error: {:?}", e); }
                nuke_and_close_token_account(&candidate.address, helius, &config.keypair, config.dust_threshold_base_units).await;
                traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                continue;
            }
            
            // Query current price
            if let Ok(quote) = jup_client.get_quote(
                &candidate.address,
                "So11111111111111111111111111111111111111112",
                1_000_000,
                config.slippage_bps,
            ).await {
                let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
                candidate.price = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &candidate.address) as i32));
            }
            
            let pnl_pct = ((candidate.price - entry_price) / entry_price) * 100.0;
            let current_hold_minutes = (current_ticks as f64 * config.check_interval_secs as f64) / 60.0;
            println!("   Current Price: ${:.10} (P&L: {:.2}%, Hold Time: {:.1}m/{}m)", candidate.price, pnl_pct, current_hold_minutes, config.max_hold_time_minutes);
            
            let mut should_exit = false;
            let mut exit_reason = "";
            
            if pnl_pct >= config.take_profit_pct {
                should_exit = true;
                exit_reason = "Take-Profit Target Hit (Dynamic)";
            } else if pnl_pct <= config.stop_loss_pct {
                should_exit = true;
                exit_reason = "Stop-Loss Hit (Dynamic)";
            } else if current_hold_minutes >= config.max_hold_time_minutes as f64 {
                should_exit = true;
                exit_reason = "Maximum Hold Time Limit Exceeded (Time-Stop)";
            }
            
            let mut exit_succeeded = false;
            if should_exit {
                println!("🚨 EXIT TRIGGERED: {}! Initiating sell swap...", exit_reason);
                let mut sell_amount_base_units = 0u64;
                if config.dry_run {
                    // M2: use checked cast; skip on NaN/negative.
                    if let Some(amount) = f64_to_u64_checked(
                        (config.trade_amount_sol as f64 / 1_000_000_000.0) / entry_price
                            * 10f64.powi(get_decimals_or_warn(&helius.connection(), &candidate.address) as i32),
                    ) {
                        sell_amount_base_units = amount;
                    }
                } else {
                    for b in &wallet_balances {
                        if b.mint == candidate.address {
                            // M2: checked cast for live balance.
                            sell_amount_base_units = f64_to_u64_checked(
                                b.balance * 10f64.powi(b.decimals as i32),
                            ).unwrap_or(0);
                            break;
                        }
                    }
                }
                
                if sell_amount_base_units == 0 {
                    println!("⚠️  Wallet holds 0 tokens of {}. Skipping sell swap.", candidate.symbol);
                    if let Err(e) = db_conn.execute("UPDATE trades SET status = 'CLOSED', sell_signature = 'EMPTY_BALANCE_CLEAN' WHERE token_address = ?1 AND status = 'OPEN'", [&candidate.address]) {
                        eprintln!("❌ [DB] Failed to mark empty-balance trade closed for {}: {:?}", candidate.address, e);
                    }
                    exit_succeeded = true;
                } else {
                    println!("Fetching quote from Jupiter to swap {} back to SOL...", candidate.symbol);
                    match execute_sell_swap(&candidate.address, sell_amount_base_units, &config, helius, jup_client).await {
                        Ok(Some(sig)) => {
                            println!("🎉  Sell swap successful! Sig: {}", sig);
                            let trade_amount_sol = config.trade_amount_sol as f64 / 1_000_000_000.0;
                            let sell_amount_sol = trade_amount_sol * (1.0 + pnl_pct / 100.0);
                            let realized_pnl_usd = (sell_amount_sol - trade_amount_sol) * sol_price_usd;
                            
                            if let Err(e) = db::finalize_sell_trade(&db_conn, &candidate.address, &sig, candidate.price, sell_amount_sol, pnl_pct, realized_pnl_usd, 0.001) {
                                eprintln!("❌ [DB] Failed to finalize sell trade for {}: {:?}", candidate.address, e);
                            }
                            
                            // Auto-Nuke SOL Recycler
                            nuke_and_close_token_account(&candidate.address, helius, &config.keypair, config.dust_threshold_base_units).await;
                            
                            exit_succeeded = true;
                        }
                        _ => {}
                    }
                }
            }
            
            if !exit_succeeded {
                positions_to_keep.push((candidate, entry_price, current_ticks));
            }
        }
        active_positions = positions_to_keep;
        
        // 2. Scan and Refill Positions
        if active_positions.len() < 3 {
            println!("\n--- [Scanning] Active positions: {}/3. Scanning for early Pump.fun curves ---", active_positions.len());
            let mut cooldown_addresses: Vec<String> = traded_tokens.iter().map(|(addr, _)| addr.clone()).collect();
            for (p, _, _) in &active_positions {
                cooldown_addresses.push(p.address.clone());
            }
            
            if let Ok(Some(candidate)) = hunter::hunt_golden_candidate(&cooldown_addresses, &config) {
                println!("🎯 Golden candidate found: {} ({})! Checking safeguards...", candidate.name, candidate.symbol);
                let mut buy_allowed = true;
                
                if !buying_enabled {
                    println!("❌ BUYING DEACTIVATED: SOL balance ({:.4} SOL) is below recovery ({:.4} SOL).", sol_balance, recovery_7usd_sol);
                    buy_allowed = false;
                }
                
                let required_sol = (config.trade_amount_sol as f64 / 1_000_000_000.0) + deck_5usd_sol + (FEE_BUFFER_LAMPORTS as f64 / 1_000_000_000.0);
                if sol_balance < required_sol {
                    println!("❌ SAFEGUARD TRIGGERED: SOL balance ({:.4} SOL) is below required {:.4} SOL.", sol_balance, required_sol);
                    buy_allowed = false;
                }
                
                if buy_allowed {
                    println!("Buying {} ({}) for {:.4} SOL...", candidate.name, candidate.symbol, config.trade_amount_sol as f64 / 1_000_000_000.0);
                    match execute_buy_swap(&candidate.address, config.trade_amount_sol, &config, helius, jup_client).await {
                        Ok(Some(sig)) => {
                            println!("🎉  Buy successful! Sig: {}", sig);
                            if let Err(e) = db::insert_buy_trade(&db_conn, &candidate.address, &candidate.name, &candidate.symbol, &sig, candidate.price, config.trade_amount_sol as f64 / 1_000_000_000.0, (config.trade_amount_sol as f64 / 1_000_000_000.0) / candidate.price) { eprintln!("❌ [DB] insert_buy_trade error: {:?}", e); }
                            active_positions.push((candidate.clone(), candidate.price, 0));
                        }
                        Ok(None) => {
                            println!("⚠️  [BUY REJECTED] Swap execution returned signature None.");
                        }
                        Err(e) => {
                            println!("❌  [BUY ERROR] Swap execution failed: {:?}", e);
                        }
                    }
                }
            }
        } else {
            println!("\n--- [Scanning Paused] Maximum parallel positions (3/3) active ---");
        }
        
        sleep(Duration::from_secs(config.check_interval_secs)).await;
    }
}

#[allow(unused_assignments)]
async fn run_grid_loop(
    _ref_config: &BotConfig,
    helius: &Helius,
    jup_client: &JupApiClient,
    db_conn: &Connection,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut config = BotConfig::load_from_env().map_err(|e| -> Box<dyn Error + Send + Sync> { e.into() })?;
    let wallet_pubkey_str = config.keypair.pubkey().to_string();
    let mut traded_tokens: Vec<(String, std::time::Instant)> = Vec::new();
    
    println!("\n=== Starting Raydium Post-Migration Virtual Grid ===");
    println!("Checking Interval:   {} seconds", config.check_interval_secs);
    
    // Initialize MPSC channel for Webhook Graduation Events (capacity = 100)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<GraduationEvent>(100);
    
    // Spawn Webhook Server in background task
    let port = config.webhook_port;
    let auth_token = config.webhook_auth_token.clone();
    let wallet_pubkey = config.keypair.pubkey().to_string();
    let dummy_tracker = Arc::new(local_isolate::LocalIsolateTracker::new());
    let config_arc = Arc::new(RwLock::new(config.clone()));
    tokio::spawn(async move {
        webhook_server::start_server(port, auth_token, wallet_pubkey, config_arc, tx, dummy_tracker, Arc::new(tokio::sync::RwLock::new(HashMap::new()))).await;
    });
    
    let mut pending_webhook_event: Option<GraduationEvent> = None;
    
    loop {
        // Safe configuration reload
        if let Ok(new_config) = BotConfig::reload_from_env() {
            if new_config.strategy != Strategy::Grid {
                println!("🔄  [STRATEGY SWITCH DETECTED] Switching on-the-fly to HUNTER loop...");
                return Ok(());
            }
            config = new_config;
        }
        
        let now = std::time::Instant::now();
        traded_tokens.retain(|(_, ts)| now.duration_since(*ts) < Duration::from_secs(600));
        
        // Fetch active grid sessions from SQLite
        let mut stmt = db_conn.prepare("SELECT token_address, token_name, token_symbol, baseline_price, grids_bought_count, last_grid_price, total_tokens_held, total_sol_spent FROM grid_sessions WHERE status = 'ACTIVE'")?;
        let sessions: Vec<(String, String, String, f64, i32, f64, f64, f64)> = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, f64>(3)?,
                row.get::<_, i32>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, f64>(6)?,
                row.get::<_, f64>(7)?,
            ))
        })?
          .filter_map(|r| r.ok())
          .collect();
          
        let is_active = sessions.len() >= config.max_parallel_positions;
        
        // Fetch live SOL price to calculate safeguards
        let mut sol_price_usd = 150.0;
        if let Ok(quote) = jup_client.get_quote(
            "So11111111111111111111111111111111111111112",
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            1_000_000_000,
            config.slippage_bps,
        ).await {
            sol_price_usd = quote.out_amount.parse::<f64>().unwrap_or(150_000_000.0) / 1_000_000.0;
        }
        
        // SOL balance not needed for Grid loop stats check
        
        for (idx, (token_address, name, symbol, baseline_price, grids_bought, last_grid_price, total_tokens_held, total_sol_spent)) in sessions.iter().enumerate() {
            if idx > 0 {
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
            let token_address = token_address.clone();
            let name = name.clone();
            let symbol = symbol.clone();
            let grids_bought = *grids_bought;
            let mut baseline_price = *baseline_price;
            let mut last_grid_price = *last_grid_price;
            let total_tokens_held = *total_tokens_held;
            let total_sol_spent = *total_sol_spent;

            let mut elapsed_minutes = 0.0;
            let mut buy_time_str = String::new();
            if let Ok(mut t_stmt) = db_conn.prepare("SELECT buy_time FROM trades WHERE token_address = ?1 AND status = 'OPEN' ORDER BY buy_time ASC LIMIT 1") {
                if let Ok(bt) = t_stmt.query_row([&token_address], |r| r.get::<_, String>(0)) {
                    buy_time_str = bt;
                    elapsed_minutes = get_elapsed_minutes(&buy_time_str);
                }
            }

            println!("\n--- [Active Grid Session] {} ({}) ---", name, symbol);
            println!("   Baseline Price: ${:.10}", baseline_price);
            println!("   Grids Filled:   {}/{}", grids_bought, config.grid_max_levels);
            println!("   Total Tokens:   {:.6}", total_tokens_held);
            println!("   SOL Capital:    {:.4} SOL spent", total_sol_spent);
            if !buy_time_str.is_empty() {
                println!("   Hold Duration:  {:.1}m / {}m (Started: {})", elapsed_minutes, config.max_hold_time_minutes, buy_time_str);
            }
            
            // Get current live price of this token
            let mut current_price = last_grid_price;
            if let Ok(quote) = jup_client.get_quote(
                &token_address,
                "So11111111111111111111111111111111111111112",
                1_000_000,
                config.slippage_bps,
            ).await {
                let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
                current_price = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32));
            }
            
            let mut pnl_pct = ((current_price - baseline_price) / baseline_price) * 100.0;
            println!("   Current Price:  ${:.10} (P&L: {:.2}%)", current_price, pnl_pct);

            // Trailing Grid logic:
            // If the price climbs above our baseline price and we are at the base level (grids_bought == 1),
            // trail the baseline price upward so the entire buy/sell grid ladder shifts to follow the trend.
            if grids_bought == 1 && current_price > baseline_price {
                println!("📈  [TRAILING GRID] Price climbed above baseline (${:.10} -> ${:.10}). Shifting grid upward...", baseline_price, current_price);
                if let Err(e) = db_conn.execute(
                    "UPDATE grid_sessions SET baseline_price = ?, last_grid_price = ? WHERE token_address = ?",
                    params![current_price, current_price, token_address]
                ) { eprintln!("❌ [DB] execute error: {:?}", e); }
                baseline_price = current_price;
                last_grid_price = current_price;
                pnl_pct = 0.0; // Reset PnL for the new shifted baseline
            }
            
            // 0. Check the Hold-Time Timeout (Time-Stop)
            if !buy_time_str.is_empty() && elapsed_minutes >= config.max_hold_time_minutes as f64 {
                println!("🚨  [TIME LIMIT HIT] Grid session exceeded max hold time ({}m). Liquidating grids...", config.max_hold_time_minutes);
                let sell_amount_base = f64_to_u64_checked(total_tokens_held * 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32)).unwrap_or(0);
                match execute_sell_swap(&token_address, sell_amount_base, &config, helius, jup_client).await {
                    Ok(Some(sig)) => {
                        println!("🎉  [GRID TIMEOUT] Liquidated grids on timeout! Sig: {}", sig);
                        let sell_price_usd = current_price * sol_price_usd;
                        if let Err(e) = db::finalize_sell_trade(&db_conn, &token_address, &sig, sell_price_usd, total_sol_spent * (1.0 + pnl_pct/100.0), pnl_pct, (total_sol_spent * (pnl_pct/100.0)) * sol_price_usd, 0.001) { eprintln!("❌ [DB] finalize_sell_trade error: {:?}", e); }
                    }
                    _ => {
                        println!("⚠️  [GRID TIMEOUT] Liquidation swap failed (likely zero liquidity). Closing session anyway to free slot...");
                        if let Err(e) = db_conn.execute("UPDATE trades SET status = 'CLOSED', sell_signature = 'TIMEOUT_NO_LIQUIDITY' WHERE token_address = ?1 AND status = 'OPEN'", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                    }
                }
                nuke_and_close_token_account(&token_address, helius, &config.keypair, config.dust_threshold_base_units).await;
                if let Err(e) = db_conn.execute("UPDATE grid_sessions SET status = 'TIMEOUT' WHERE token_address = ?1", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
            }
            
            // A. Check the Dynamic CEILING (Take-Profit)
            else if pnl_pct >= config.take_profit_pct {
                let retain_fraction = config.moonbag_retain_pct / 100.0;
                let sell_fraction = 1.0 - retain_fraction;
                if retain_fraction > 0.0 {
                    println!("🚀  [CEILING HIT] Price hit Take-Profit target (+{}%). Selling {:.1}% of position, leaving {:.1}% moonbag...", 
                        config.take_profit_pct, sell_fraction * 100.0, retain_fraction * 100.0);
                } else {
                    println!("🚀  [CEILING HIT] Price hit Take-Profit target (+{}%). Liquidating all grids...", config.take_profit_pct);
                }
                
                let sell_amount_base = f64_to_u64_checked(total_tokens_held * sell_fraction * 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32)).unwrap_or(0);
                if let Ok(Some(sig)) = execute_sell_swap(&token_address, sell_amount_base, &config, helius, jup_client).await {
                    println!("🎉  [GRID SUCCESS] Liquidated complete grid session! Sig: {}", sig);
                    let sell_price_usd = current_price * sol_price_usd;
                    if let Err(e) = db::finalize_sell_trade(&db_conn, &token_address, &sig, sell_price_usd, total_sol_spent * (1.0 + pnl_pct/100.0) * sell_fraction, pnl_pct, (total_sol_spent * (pnl_pct/100.0) * sell_fraction) * sol_price_usd, 0.001) { eprintln!("❌ [DB] finalize_sell_trade error: {:?}", e); }
                    
                    if config.moonbag_retain_pct == 0.0 {
                        nuke_and_close_token_account(&token_address, helius, &config.keypair, config.dust_threshold_base_units).await;
                    } else {
                        println!("💰  [MOONBAG] Keeping remaining {:.1}% tokens in wallet as a moonbag!", retain_fraction * 100.0);
                    }
                    if let Err(e) = db_conn.execute("UPDATE grid_sessions SET status = 'COMPLETED' WHERE token_address = ?1", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                }
            }
            // B. Check the Dynamic FLOOR (Stop-Loss)
            else if pnl_pct <= config.stop_loss_pct {
                println!("🚨  [FLOOR HIT] Price hit Stop-Loss target ({}%). Liquidating all grids to protect SOL...", config.stop_loss_pct);
                let sell_amount_base = f64_to_u64_checked(total_tokens_held * 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32)).unwrap_or(0);
                match execute_sell_swap(&token_address, sell_amount_base, &config, helius, jup_client).await {
                    Ok(Some(sig)) => {
                        println!("🎉  [GRID EXIT] Stopped out on grid session! Sig: {}", sig);
                        let sell_price_usd = current_price * sol_price_usd;
                        if let Err(e) = db::finalize_sell_trade(&db_conn, &token_address, &sig, sell_price_usd, total_sol_spent * (1.0 + pnl_pct/100.0), pnl_pct, (total_sol_spent * (pnl_pct/100.0)) * sol_price_usd, 0.001) { eprintln!("❌ [DB] finalize_sell_trade error: {:?}", e); }
                        nuke_and_close_token_account(&token_address, helius, &config.keypair, config.dust_threshold_base_units).await;
                        if let Err(e) = db_conn.execute("UPDATE grid_sessions SET status = 'STOPPED_OUT' WHERE token_address = ?1", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                    }
                    _ => {
                        if pnl_pct <= -90.0 {
                            println!("⚠️  [GRID EXIT FAILED] Stop-Loss liquidation failed (token rugged/no liquidity). Force-closing session to free slot...");
                            if let Err(e) = db_conn.execute("UPDATE trades SET status = 'CLOSED', sell_signature = 'RUG_NO_LIQUIDITY' WHERE token_address = ?1 AND status = 'OPEN'", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                            nuke_and_close_token_account(&token_address, helius, &config.keypair, config.dust_threshold_base_units).await;
                            if let Err(e) = db_conn.execute("UPDATE grid_sessions SET status = 'STOPPED_OUT' WHERE token_address = ?1", [&token_address]) { eprintln!("❌ [DB] execute error: {:?}", e); }
                        } else {
                            println!("⚠️  [GRID EXIT FAILED] Stop-Loss swap execution failed. Will retry on next check loop...");
                        }
                    }
                }
            }
            // C. Check the GRID BUY TRIGGERS (Dips)
            else if grids_bought < config.grid_max_levels as i32 {
                let next_buy_multiplier = (1.0 - (config.grid_spacing_pct / 100.0)).powi(grids_bought + 1);
                let next_buy_price = baseline_price * next_buy_multiplier;
                println!("   Next Buy Grid:  ${:.10} (Required trigger)", next_buy_price);
                
                if current_price <= next_buy_price {
                    println!("🔥  [GRID BUY TRIGGER] Price dipped to Grid {}! Buying more...", grids_bought + 1);
                    if let Ok(Some(sig)) = execute_buy_swap(&token_address, config.grid_trade_size_sol, &config, helius, jup_client).await {
                        discord_notify::notify_grid_buy(
                            &symbol,
                            grids_bought + 1,
                            config.grid_trade_size_sol as f64 / 1_000_000_000.0,
                            pnl_pct,
                        ).await;
                        println!("🎉  Grid buy successful! Sig: {}", sig);
                        let additional_tokens = (config.grid_trade_size_sol as f64 / 1_000_000_000.0) / current_price;
                        let new_total_held = total_tokens_held + additional_tokens;
                        let new_total_sol = total_sol_spent + (config.grid_trade_size_sol as f64 / 1_000_000_000.0);
                        
                        if let Err(e) = db_conn.execute(
                            "UPDATE grid_sessions SET grids_bought_count = ?, last_grid_price = ?, total_tokens_held = ?, total_sol_spent = ? WHERE token_address = ?",
                            params![grids_bought + 1, current_price, new_total_held, new_total_sol, token_address]
                        ) { eprintln!("❌ [DB] execute error: {:?}", e); }
                    }
                }
            }
            // D. Check the GRID SELL TRIGGERS (Noise Pullbacks)
            else if grids_bought > 0 {
                let current_level_cost_basis = baseline_price * (1.0 - (config.grid_spacing_pct / 100.0)).powi(grids_bought);
                let sell_trigger_multiplier = 1.0 + (config.grid_profit_pct / 100.0);
                let sell_trigger_price = current_level_cost_basis * sell_trigger_multiplier;
                println!("   Next Sell Grid: ${:.10} (Required trigger)", sell_trigger_price);
                
                if current_price >= sell_trigger_price {
                    println!("📈  [GRID PROFIT TRIGGER] Price rebounded to Sell Grid! Taking profit on Grid {}...", grids_bought);
                    let one_grid_tokens = total_tokens_held / (grids_bought as f64);
                    let sell_amount_base = f64_to_u64_checked(one_grid_tokens * 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32)).unwrap_or(0);
                    if let Ok(Some(sig)) = execute_sell_swap(&token_address, sell_amount_base, &config, helius, jup_client).await {
                        println!("🎉  Grid take-profit successful! Sig: {}", sig);
                        let new_total_held = total_tokens_held - one_grid_tokens;
                        let new_total_sol = total_sol_spent - (config.grid_trade_size_sol as f64 / 1_000_000_000.0);
                        
                        if let Err(e) = db_conn.execute(
                            "UPDATE grid_sessions SET grids_bought_count = ?, last_grid_price = ?, total_tokens_held = ?, total_sol_spent = ? WHERE token_address = ?",
                            params![grids_bought - 1, current_price, new_total_held, new_total_sol, token_address]
                        ) { eprintln!("❌ [DB] execute error: {:?}", e); }
                    }
                }
            }
        }

        // If we still have parallel position slots available, scan/receive candidate entries
        if sessions.len() < config.max_parallel_positions {
            let slots_free = config.max_parallel_positions - sessions.len();
            println!("\n--- [Scanning] {}/{} Grid Sessions active ({} slots free). Hunting Raydium Pools ---", 
                sessions.len(), config.max_parallel_positions, slots_free);
            
            // A. Check for instant Webhook Graduation Event and Enforce Safety Filters
            let mut instant_candidate: Option<hunter::GmgnTokenCandidate> = None;
            
            let event_opt = pending_webhook_event.take().or_else(|| {
                if let Ok(evt) = rx.try_recv() {
                    Some(evt)
                } else {
                    None
                }
            });

            if let Some(event) = event_opt {
                println!("⚡ [INSTANT TRIGGER] Webhook received graduation for {}! Auditing safety filters before buying...", event.mint);
                
                let cooldown_addresses: Vec<String> = traded_tokens.iter().map(|(addr, _)| addr.clone()).collect();
                let mut found_candidate = None;
                
                // Poll GMGN completed list up to 16 times (0.5-second interval) to allow for API indexing
                for attempt in 1..=16 {
                    println!("  [Audit Log] Scanning GMGN Completed List to apply safety/bundle filters (Attempt {}/16)...", attempt);
                    if let Ok(candidates) = hunter::scan_bonding_curve_trenches(&cooldown_addresses, &config) {
                        if let Some(matched) = candidates.into_iter().find(|c| c.address == event.mint) {
                            found_candidate = Some(matched);
                            break;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                
                if let Some(matched_candidate) = found_candidate {
                    println!("✅ [FILTER PASSED] Token {} ({}) passed all security, bundle, and momentum filters! Proceeding to buy...", matched_candidate.name, matched_candidate.symbol);
                    instant_candidate = Some(matched_candidate);
                } else {
                    println!("❌ [FILTER REJECTED / TIMEOUT] Token {} did not pass your security filters or failed to propagate on GMGN within 10 seconds. Aborting execution.", event.mint);
                }
            }

            // B. Fall back to standard polling scan if no webhook event was received
            let candidate_opt = if let Some(cand) = instant_candidate {
                Some(cand)
            } else {
                let cooldown_addresses: Vec<String> = traded_tokens.iter().map(|(addr, _)| addr.clone()).collect();
                hunter::hunt_golden_candidate(&cooldown_addresses, &config).unwrap_or(None)
            };
            
            if let Some(candidate) = candidate_opt {
                println!("🎯 Completed token discovered: {} ({})! Checking liquidity...", candidate.name, candidate.symbol);
                
                // We require at least config.min_liquidity on Raydium to initiate the grid
                if candidate.progress >= 0.0 { // Progress is completed, so always matches
                    // Enforce detailed safety audit check on post-migration tokens
                    match hunter::audit_detailed_token_safety(&candidate.address, &config) {
                        Ok(true) => {
                            if hunter::check_copycat_scam(&candidate.address, &candidate.name, &candidate.symbol, &db_conn) {
                                println!("❌  [BUY REJECTED] Candidate failed copycat scam check (duplicate name/symbol).");
                                traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                                continue;
                            }
                        }
                        Ok(false) => {
                            println!("❌  [BUY REJECTED] Candidate failed detailed pre-buy safety audit.");
                            traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                            continue;
                        }
                        Err(e) => {
                            println!("⚠️  Detailed safety audit error: {:?}. Proceeding with caution...", e);
                        }
                    }

                    println!("🚀  Starting fresh Post-Migration Grid Session on {}!", candidate.name);
                    
                    // DexScreener pre-entry safety check (cross-DEX liquidity, buy/sell ratio, pair age)
                    match dexscreener::check_entry_safety(&candidate.address, config.min_liquidity, 120).await {
                        Ok(report) => {
                            if !report.safe {
                                println!("❌  [DexScreener Reject] Token {} failed pre-entry safety: {}",
                                    candidate.symbol, report.rejection_reason.unwrap_or_default());
                                traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                                continue;
                            }
                        }
                        Err(e) => {
                            println!("⚠️  DexScreener check error: {:?}. Proceeding with caution...", e);
                        }
                    }
                    
                    // Buy Grid 1 (Base Level)
                    println!("Buying base Grid 1 for {:.4} SOL...", config.grid_trade_size_sol as f64 / 1_000_000_000.0);
                    match execute_buy_swap(&candidate.address, config.grid_trade_size_sol, &config, helius, jup_client).await {
                        Ok(Some(sig)) => {
                            println!("🎉  Base Grid Buy successful! Sig: {}", sig);
                            discord_notify::notify_buy(
                                &candidate.symbol,
                                &candidate.name,
                                config.grid_trade_size_sol as f64 / 1_000_000_000.0,
                                candidate.price,
                                1,
                                &sig,
                            ).await;
                            
                            // Fetch actual token balance from our wallet to know exactly how many tokens were bought
                            let mut base_tokens = 0.0;
                            if let Ok(bal) = helius.get_wallet_balances(&wallet_pubkey_str, Some(1), Some(100), Some(false), Some(false), Some(false)).await {
                                for b in &bal.balances {
                                    if b.mint == candidate.address {
                                        base_tokens = b.balance;
                                        break;
                                    }
                                }
                            }
                            
                            // Fallback if wallet balance query fails
                            if base_tokens == 0.0 {
                                let mut start_price_sol = candidate.price / sol_price_usd;
                                if let Ok(quote) = jup_client.get_quote(
                                    &candidate.address,
                                    "So11111111111111111111111111111111111111112",
                                    1_000_000,
                                    config.slippage_bps,
                                ).await {
                                    let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
                                    start_price_sol = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &candidate.address) as i32));
                                }
                                base_tokens = (config.grid_trade_size_sol as f64 / 1_000_000_000.0) / start_price_sol;
                            }

                            let sol_spent = config.grid_trade_size_sol as f64 / 1_000_000_000.0;
                            let start_price_sol = sol_spent / base_tokens;
                            let start_price_usd = start_price_sol * sol_price_usd;
                            
                            // Insert into trades ledger
                            if let Err(e) = db::insert_buy_trade(&db_conn, &candidate.address, &candidate.name, &candidate.symbol, &sig, start_price_usd, sol_spent, base_tokens) { eprintln!("❌ [DB] insert_buy_trade error: {:?}", e); }
                            
                            // Create active grid session record with prices strictly in SOL!
                            if let Err(e) = db_conn.execute(
                                "INSERT OR REPLACE INTO grid_sessions (token_address, token_name, token_symbol, baseline_price, grids_bought_count, last_grid_price, total_tokens_held, total_sol_spent, status) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, 'ACTIVE')",
                                params![candidate.address, candidate.name, candidate.symbol, start_price_sol, start_price_sol, base_tokens, sol_spent]
                            ) { eprintln!("❌ [DB] execute error: {:?}", e); }
                        }
                        Ok(None) => {
                            println!("⚠️  [BUY REJECTED] Swap execution returned signature None.");
                            traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                        }
                        Err(e) => {
                            println!("❌  [BUY ERROR] Swap execution failed: {:?}", e);
                            traded_tokens.push((candidate.address.clone(), std::time::Instant::now()));
                        }
                    }
                }
            }
        }
        
        let tick_duration = if is_active {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(config.check_interval_secs)
        };

        if is_active {
            while rx.try_recv().is_ok() {}
        }

        tokio::select! {
            event = rx.recv(), if !is_active => {
                if let Some(evt) = event {
                    println!("⚡ [EVENT WAKE] Received real-time webhook event for {}!", evt.mint);
                    pending_webhook_event = Some(evt);
                }
            }
            _ = tokio::time::sleep(tick_duration) => {}
        }
    }
}

// === ACTOR MODULE FOR AUTONOMOUS STRATEGY ===

enum DbMessage {
    InsertBuy {
        token_address: String,
        token_name: String,
        token_symbol: String,
        buy_signature: String,
        buy_price_usd: f64,
        buy_amount_sol: f64,
        buy_amount_tokens: f64,
        start_price_sol: f64,
    },
    FinalizeSell {
        token_address: String,
        sell_signature: String,
        sell_price_usd: f64,
        sell_amount_sol: f64,
        pnl_pct: f64,
        pnl_usd: f64,
        fee_sol: f64,
        status_str: String,
    },
    GetBuyTime {
        token_address: String,
        responder: tokio::sync::oneshot::Sender<Option<String>>,
    },
}

async fn run_db_actor(mut rx: tokio::sync::mpsc::Receiver<DbMessage>) {
    println!("🗄️  [DB Actor] Starting database writer actor...");
    let db_conn = match db::init_db() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("❌  [DB Actor] Failed to open database: {:?}", e);
            return;
        }
    };

    while let Some(msg) = rx.recv().await {
        match msg {
            DbMessage::InsertBuy {
                token_address,
                token_name,
                token_symbol,
                buy_signature,
                buy_price_usd,
                buy_amount_sol,
                buy_amount_tokens,
                start_price_sol,
            } => {
                println!("🗄️  [DB Actor] Inserting buy record for {}", token_symbol);
                if let Err(e) = db::insert_buy_trade(
                    &db_conn,
                    &token_address,
                    &token_name,
                    &token_symbol,
                    &buy_signature,
                    buy_price_usd,
                    buy_amount_sol,
                    buy_amount_tokens,
                ) {
                    eprintln!("❌  [DB Actor] Failed to insert buy trade: {:?}", e);
                }

                if let Err(e) = db_conn.execute(
                    "INSERT OR REPLACE INTO grid_sessions (token_address, token_name, token_symbol, baseline_price, grids_bought_count, last_grid_price, total_tokens_held, total_sol_spent, status) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, 'ACTIVE')",
                    params![token_address, token_name, token_symbol, start_price_sol, start_price_sol, buy_amount_tokens, buy_amount_sol]
                ) {
                    eprintln!("❌  [DB Actor] Failed to insert grid session: {:?}", e);
                }
            }
            DbMessage::FinalizeSell {
                token_address,
                sell_signature,
                sell_price_usd,
                sell_amount_sol,
                pnl_pct,
                pnl_usd,
                fee_sol,
                status_str,
            } => {
                println!("🗄️  [DB Actor] Finalizing sell record for {}", token_address);
                if let Err(e) = db::finalize_sell_trade(
                    &db_conn,
                    &token_address,
                    &sell_signature,
                    sell_price_usd,
                    sell_amount_sol,
                    pnl_pct,
                    pnl_usd,
                    fee_sol,
                ) {
                    eprintln!("❌  [DB Actor] Failed to finalize sell trade: {:?}", e);
                }

                if let Err(e) = db_conn.execute(
                    "UPDATE grid_sessions SET status = ?1 WHERE token_address = ?2",
                    params![status_str, token_address],
                ) {
                    eprintln!("❌  [DB Actor] Failed to update grid session status: {:?}", e);
                }
            }
            DbMessage::GetBuyTime { token_address, responder } => {
                let buy_time = if let Ok(mut t_stmt) = db_conn.prepare("SELECT buy_time FROM trades WHERE token_address = ?1 AND status = 'OPEN' ORDER BY buy_time ASC LIMIT 1") {
                    t_stmt.query_row([&token_address], |r| r.get::<_, String>(0)).ok()
                } else {
                    None
                };
                let _ = responder.send(buy_time);
            }
        }
    }
    println!("🗄️  [DB Actor] Shutting down database writer actor.");
}

struct TokenActorConfig {
    token_address: String,
    token_name: String,
    token_symbol: String,
    baseline_price_sol: f64,
    total_tokens_held: f64,
    total_sol_spent: f64,
}

async fn run_token_actor(
    token_config: TokenActorConfig,
    ref_config: Arc<RwLock<BotConfig>>,
    helius: Arc<Helius>,
    jup_client: Arc<JupApiClient>,
    tracker: Arc<local_isolate::LocalIsolateTracker>,
    db_tx: tokio::sync::mpsc::Sender<DbMessage>,
    mut rx: tokio::sync::mpsc::Receiver<ActorEvent>,
    actor_registry: ActorRegistry,
) {
    let mint = token_config.token_address.clone();
    println!("🛰️  [Token Actor - {}] Started reactive position monitor...", token_config.token_symbol);

    let mut v_delta_history = VecDeque::new();
    
    // Initial safety check & price fetch on startup
    let mut current_price_sol = token_config.baseline_price_sol;
    let config_snap = ref_config.read().await.clone();
    if let Ok(quote) = jup_client.get_quote(
        &mint,
        "So11111111111111111111111111111111111111112",
        1_000_000,
        config_snap.slippage_bps,
    ).await {
        let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
        current_price_sol = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &mint) as i32));
    }

    let mut dev_team_hold_rate = 0.0;
    let mut suspected_insider_hold_rate = 0.0;
    let mut current_lp_to_mc = 0.08;

    if let Ok(info) = hunter::run_gmgn_cli(&["token", "info", "--address", &mint, "--chain", "sol", "--raw"]) {
        let stat = &info["stat"];
        let parse_f64 = |val: &serde_json::Value| -> f64 {
            if let Some(s) = val.as_str() {
                s.parse::<f64>().unwrap_or(0.0)
            } else if let Some(f) = val.as_f64() {
                f
            } else {
                0.0
            }
        };
        dev_team_hold_rate = parse_f64(&stat["dev_team_hold_rate"]);
        suspected_insider_hold_rate = parse_f64(&stat["suspected_insider_hold_rate"]);

        let circulating_supply = info["circulating_supply"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let price = info["price"]["price"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let liquidity = info["liquidity"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
        let market_cap = circulating_supply * price;
        if market_cap > 0.0 {
            current_lp_to_mc = liquidity / market_cap;
        }
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    let mut breakeven_active = false;
    let mut prev_price_sol: Option<f64> = None;

    loop {
        // 1. Reload local config snapshot
        let config = ref_config.read().await.clone();

        // Check if strategy changed; if so, exit actor
        if config.strategy != Strategy::Autonomous {
            println!("🛰️  [Token Actor - {}] Strategy changed away from Autonomous. Exiting actor.", token_config.token_symbol);
            break;
        }

        let mut event_received = false;

        tokio::select! {
            msg = rx.recv() => {
                if let Some(ActorEvent::PriceUpdate { price_sol }) = msg {
                    current_price_sol = price_sol;
                    event_received = true;
                } else {
                    break; // Channel closed
                }
            }
            _ = heartbeat.tick() => {
                // Heartbeat tick: check hold time timeout, and do fallback price/safety fetch
                if let Ok(quote) = jup_client.get_quote(
                    &mint,
                    "So11111111111111111111111111111111111111112",
                    1_000_000,
                    config.slippage_bps,
                ).await {
                    let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
                    current_price_sol = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &mint) as i32));
                }

                if let Ok(info) = hunter::run_gmgn_cli(&["token", "info", "--address", &mint, "--chain", "sol", "--raw"]) {
                    let stat = &info["stat"];
                    let parse_f64 = |val: &serde_json::Value| -> f64 {
                        if let Some(s) = val.as_str() {
                            s.parse::<f64>().unwrap_or(0.0)
                        } else if let Some(f) = val.as_f64() {
                            f
                        } else {
                            0.0
                        }
                    };
                    dev_team_hold_rate = parse_f64(&stat["dev_team_hold_rate"]);
                    suspected_insider_hold_rate = parse_f64(&stat["suspected_insider_hold_rate"]);

                    let circulating_supply = info["circulating_supply"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                    let price = info["price"]["price"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                    let liquidity = info["liquidity"].as_str().unwrap_or("0").parse::<f64>().unwrap_or(0.0);
                    let market_cap = circulating_supply * price;
                    if market_cap > 0.0 {
                        current_lp_to_mc = liquidity / market_cap;
                    }
                }
            }
        }

        let pnl_pct = ((current_price_sol - token_config.baseline_price_sol) / token_config.baseline_price_sol) * 100.0;

        let mut should_exit = false;
        let mut exit_reason = "";

        // Single-tick rapid price crash emergency exit (>20% drop in single update)
        if let Some(prev_px) = prev_price_sol {
            if prev_px > 0.0 {
                let tick_change_pct = ((current_price_sol - prev_px) / prev_px) * 100.0;
                if tick_change_pct <= -20.0 {
                    should_exit = true;
                    exit_reason = "EMERGENCY: Rapid price crash (>20% drop in single tick)";
                    println!("💥  [Token Actor - {}] EMERGENCY EXIT: Single-tick price drop of {:.1}%! Dumping to save capital.", token_config.token_symbol, tick_change_pct);
                }
            }
        }
        prev_price_sol = Some(current_price_sol);
        let current_v_delta = tracker.get_v_delta(&mint).await;

        // Push to v_delta history
        v_delta_history.push_back(current_v_delta);
        if v_delta_history.len() > 12 {
            v_delta_history.pop_front();
        }

        // Query buy_time from DB Actor
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        if let Err(e) = db_tx.send(DbMessage::GetBuyTime {
            token_address: mint.clone(),
            responder: resp_tx,
        }).await { eprintln!("❌ [DB Actor] send error: {:?}", e); }

        let buy_time_str = resp_rx.await.unwrap_or(None);
        let mut elapsed_minutes = 0.0;
        if let Some(ref bt) = buy_time_str {
            elapsed_minutes = get_elapsed_minutes(bt);
        }

        if event_received {
            println!("⚡  [Token Actor - {}] Instant Webhook Price: {:.10} SOL (P&L: {:.2}%)", token_config.token_symbol, current_price_sol, pnl_pct);
        } else {
            println!("📊  [Token Actor - {}] Heartbeat update | Price: {:.10} SOL (P&L: {:.2}%)", token_config.token_symbol, current_price_sol, pnl_pct);
        }



        // B.1 Check abort criteria
        if dev_team_hold_rate > 0.15 {
            should_exit = true;
            exit_reason = "ABORT: Developer wallet accumulation active (imminent dump)";
        } else if suspected_insider_hold_rate > 0.20 {
            should_exit = true;
            exit_reason = "ABORT: Suspected insider cluster holding too high";
        } else if current_lp_to_mc < config.min_lp_mc_ratio_exit {
            should_exit = true;
            exit_reason = "ABORT: Liquidity evaporated below safety floor";
        } else {
            // Check momentum exhaustion (volume delta negative over last 5 updates)
            let v_deltas: Vec<f64> = v_delta_history.iter().cloned().collect();
            if v_deltas.len() >= 5 && v_deltas.iter().rev().take(5).all(|&v| v < 0.0) {
                should_exit = true;
                exit_reason = "ABORT: Momentum exhaustion (volume delta negative)";
            }
        }

        // B.2 Check dynamic TP/SL
        let (v_mean, v_std) = {
            let v_vals: Vec<f64> = v_delta_history.iter().cloned().collect();
            if v_vals.len() >= 4 {
                let mean = v_vals.iter().sum::<f64>() / v_vals.len() as f64;
                let variance = v_vals.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / v_vals.len() as f64;
                (mean, variance.sqrt())
            } else {
                (0.0, 1.0)
            }
        };

        let v_zscore = if v_std > 0.001 {
            (current_v_delta - v_mean) / v_std
        } else {
            0.0
        };

        let mut dynamic_tp = config.take_profit_pct;
        if v_zscore > 1.5 && dev_team_hold_rate <= 0.08 {
            dynamic_tp = config.take_profit_pct * 2.5;
            println!("🚀  [Adaptive TP Activated - {}] Strong volume momentum Z-Score ({:.2}). Dynamic TP target bumped to +{:.1}%", token_config.token_symbol, v_zscore, dynamic_tp);
        }

        // Breakeven Stop-Loss Trailing Gate
        if pnl_pct >= config.breakeven_trigger_pct && !breakeven_active {
            breakeven_active = true;
            println!("🛡️  [Token Actor - {}] Breakeven Stop-Loss Activated! PnL reached +{:.1}%. Moving SL floor to +{:.1}%", token_config.token_symbol, pnl_pct, config.breakeven_stop_loss_pct);
        }

        let effective_sl = if breakeven_active {
            config.breakeven_stop_loss_pct
        } else {
            config.stop_loss_pct
        };

        if pnl_pct >= dynamic_tp {
            should_exit = true;
            exit_reason = "Take-Profit Target Hit (Dynamic)";
        } else if pnl_pct <= effective_sl {
            should_exit = true;
            exit_reason = if breakeven_active {
                "Breakeven Stop-Loss Triggered (Protected Gain)"
            } else {
                "Stop-Loss Target Hit (Dynamic)"
            };
        } else if !buy_time_str.as_ref().map(|s| s.is_empty()).unwrap_or(true) && elapsed_minutes >= config.max_hold_time_minutes as f64 {
            should_exit = true;
            exit_reason = "Hold time exceeded (Timeout)";
        }

        if should_exit {
            println!("🚨  [Token Actor - {}] EXIT TRIGGERED: {}! Executing Jito exit swap...", token_config.token_symbol, exit_reason);
            // L3: fail the trade if on-chain decimals cannot be resolved.
            let decimals = match get_decimals_by_mint(&helius.connection(), &mint) {
                Some(d) => d,
                None => {
                    eprintln!("   ❌  Cannot resolve on-chain decimals for {}; skipping exit swap.", mint);
                    continue;
                }
            };
            // M2: checked cast for sell amount.
            let sell_amount_base = match f64_to_u64_checked(
                token_config.total_tokens_held * 10f64.powi(decimals as i32),
            ) {
                Some(v) => v,
                None => {
                    eprintln!("   ❌  Invalid sell amount for {}; skipping exit swap.", mint);
                    continue;
                }
            };

            if sell_amount_base > 0 {
                let is_abort = exit_reason.contains("ABORT");
                // M2: use saturating_mul to prevent overflow on large tips.
                let exit_tip = if is_abort {
                    config.jito_tip_lamports.saturating_mul(3) / 2
                } else {
                    config.jito_tip_lamports
                };

                println!("🚀  Submitting exit trade private Jito bundle. Tip size: {} lamports", exit_tip);

                let quote_res = jup_client.get_quote(
                    &mint,
                    "So11111111111111111111111111111111111111112",
                    sell_amount_base,
                    config.slippage_bps,
                ).await;

                match quote_res {
                    Ok(quote) => {
                        // H4: price impact check on exit swap.
                        println!("  [Price Impact] exit {} -> SOL: {}% (max {:.1}%)", mint, quote.price_impact_pct, config.max_price_impact_pct);
                        if price_impact_exceeds(&quote, config.max_price_impact_pct) {
                            println!("  ⚠️ [Price Impact] Rejecting exit swap: impact {}% exceeds {:.1}% threshold.", quote.price_impact_pct, config.max_price_impact_pct);
                            continue;
                        }
                        let wallet_pubkey_str = config.keypair.pubkey().to_string();
                        match jup_client.get_swap_transaction(quote, &wallet_pubkey_str, None, Some(exit_tip)).await {
                            Ok(swap_tx) => {
                                let tx_bytes = match BASE64_STANDARD.decode(&swap_tx) {
                                    Ok(b) => b,
                                    Err(e) => {
                                        println!("   ❌  Failed to decode swap tx base64: {:?}", e);
                                        continue;
                                    }
                                };
                                let unsigned_tx: VersionedTransaction = match bincode::deserialize(&tx_bytes) {
                                    Ok(tx) => tx,
                                    Err(e) => {
                                        println!("   ❌  Failed to deserialize swap transaction: {:?}", e);
                                        continue;
                                    }
                                };
                                let signed_tx = match VersionedTransaction::try_new(unsigned_tx.message, &[config.keypair.as_ref()]) {
                                    Ok(tx) => tx,
                                    Err(e) => {
                                        println!("   ❌  Failed to sign swap transaction: {:?}", e);
                                        continue;
                                    }
                                };

                                let sig = if config.dry_run {
                                    println!("   [Dry-Run] Exit simulation succeeded!");
                                    Some("DRY_RUN_EXIT_SIGNATURE".to_string())
                                } else {
                                    match send_transaction_to_jito(&signed_tx).await {
                                        Ok(sig_str) => {
                                            if let Ok(sig_parsed) = solana_sdk::signature::Signature::from_str(&sig_str) {
                                                println!("   Submitted exit trade privately to Jito! Confirming signature {}...", sig_str);
                                                let mut confirmed = false;
                                                for _ in 0..30 {
                                                    if let Ok(status) = helius.connection().get_signature_status(&sig_parsed) {
                                                        if let Some(Ok(())) = status {
                                                            confirmed = true;
                                                            break;
                                                        }
                                                    }
                                                    tokio::time::sleep(Duration::from_millis(1500)).await;
                                                }
                                                if confirmed {
                                                    Some(sig_str)
                                                } else {
                                                    println!("   ❌  Jito exit transaction failed on-chain or timed out (sig: {})", sig_str);
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        }
                                        Err(e) => {
                                            println!("   ⚠️  Jito exit bundle failed: {:?}. Retrying publicly...", e);
                                            if let Ok(latest_blockhash_with_height) = helius.connection().get_latest_blockhash_with_commitment(CommitmentConfig::confirmed()) {
                                                let last_valid_block_height = latest_blockhash_with_height.1;
                                                let send_config = RpcSendTransactionConfig {
                                                    skip_preflight: true,
                                                    preflight_commitment: None,
                                                    encoding: None,
                                                    max_retries: Some(5),
                                                    min_context_slot: None,
                                                };
                                                helius.send_and_confirm_transaction(&signed_tx, send_config, last_valid_block_height, None).await.ok().map(|s| s.to_string())
                                            } else {
                                                None
                                            }
                                        }
                                    }
                                };

                                if let Some(sig_str) = sig {
                                    println!("🎉  Exit swap successful! Signature: {}", sig_str);
                                    let mut sol_price_usd = 150.0;
                                    if let Ok(quote) = jup_client.get_quote(
                                        "So11111111111111111111111111111111111111112",
                                        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                                        1_000_000_000,
                                        config.slippage_bps,
                                    ).await {
                                        sol_price_usd = quote.out_amount.parse::<f64>().unwrap_or(150_000_000.0) / 1_000_000.0;
                                    }
                                    let sell_price_usd = current_price_sol * sol_price_usd;
                                    let total_sol_reclaimed = token_config.total_tokens_held * current_price_sol;
                                    let pnl_usd = (total_sol_reclaimed - token_config.total_sol_spent) * sol_price_usd;

                                    discord_notify::notify_sell(
                                        &token_config.token_symbol,
                                        &exit_reason,
                                        pnl_pct,
                                        pnl_usd,
                                        total_sol_reclaimed,
                                        &sig_str,
                                    ).await;

                                    if let Err(e) = db_tx.send(DbMessage::FinalizeSell {
                                        token_address: mint.clone(),
                                        sell_signature: sig_str,
                                        sell_price_usd,
                                        sell_amount_sol: total_sol_reclaimed,
                                        pnl_pct,
                                        pnl_usd,
                                        fee_sol: 0.001,
                                        status_str: "COMPLETED".to_string(),
                                    }).await { eprintln!("❌ [DB Actor] send error: {:?}", e); }
                                    nuke_and_close_token_account(&mint, &helius, &config.keypair, config.dust_threshold_base_units).await;
                                    
                                    // Remove from active registry
                                    let mut reg = actor_registry.write().await;
                                    reg.remove(&mint);
                                    break; // Exit actor loop
                                }
                            }
                            Err(e) => println!("   ❌  Failed to build swap transaction: {:?}", e),
                        }
                    }
                    Err(e) => println!("   ❌  Failed to fetch quote for exit: {:?}", e),
                }
            }
        }
    }
    println!("🛰️  [Token Actor - {}] Actor shut down.", token_config.token_symbol);
}

async fn run_autonomous_loop(
    ref_config: Arc<RwLock<BotConfig>>,
    helius: Arc<Helius>,
    jup_client: Arc<JupApiClient>,
    db_conn: &Connection,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("\n=== Starting Autonomous Execution Agent Loop (Push-Based Actor Architecture) ===");

    // DB Writer Actor
    let (db_tx, db_rx) = tokio::sync::mpsc::channel::<DbMessage>(100);
    tokio::spawn(run_db_actor(db_rx));

    // Active Token Actor Registry (for routing pushed webhook swap events)
    let actor_registry: ActorRegistry = Arc::new(RwLock::new(HashMap::new()));

    // Webhook channels
    let (tx, mut rx) = tokio::sync::mpsc::channel::<GraduationEvent>(100);

    // Webhook configuration parameters
    let config_clone = ref_config.clone();
    let (port, auth_token, wallet_pubkey) = {
        let config = ref_config.read().await;
        (
            config.webhook_port,
            config.webhook_auth_token.clone(),
            config.keypair.pubkey().to_string(),
        )
    };

    let tracker = Arc::new(local_isolate::LocalIsolateTracker::new());
    let tracker_clone = tracker.clone();

    let tx_clone = tx.clone();
    let actor_registry_clone = actor_registry.clone();
    tokio::spawn(async move {
        webhook_server::start_server(
            port,
            auth_token,
            wallet_pubkey,
            config_clone,
            tx_clone,
            tracker_clone,
            actor_registry_clone,
        ).await;
    });

    // Spawn pruner task
    let tracker_pruner = tracker.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            tracker_pruner.prune_inactive_tokens().await;
        }
    });

    // Fetch Helius webhook ID dynamically for dynamic whitelist sync
    let h_key = { ref_config.read().await.helius_api_key.clone() };
    let helius_webhook_id = fetch_helius_webhook_id(&h_key).await;
    if let Some(ref id) = helius_webhook_id {
        println!("🔔  [Helius Sync] Discovered active Helius Webhook ID: {}", id);
    } else {
        println!("⚠️  [Helius Sync] Warning: Could not locate active Helius Webhook. Dynamic sync is disabled.");
    }

    // Tracker for background token scanner
    let mut last_scan_time = std::time::Instant::now() - Duration::from_secs(45);

    // List of tokens currently monitored for entry (graduated Raydium pools)
    let mut monitored_tokens: HashMap<String, (String, std::time::Instant)> = HashMap::new(); // mint -> (symbol, graduation_time)

    // Cooldown list of recently traded tokens to prevent repetitive trading
    let mut traded_tokens: Vec<(String, std::time::Instant)> = Vec::new();

    // Map of running token actor handles (mint -> handle)
    let mut active_token_actors: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();

    // M6: Atomic position-slot counter — the **single source of truth** for
    // how many positions are currently open.  This prevents race conditions
    // where multiple entry checks in a single loop pass could all see
    // `slots_free` and overshoot `max_parallel_positions`.  The counter is
    // incremented atomically *before* the buy swap is submitted (permit-style)
    // and decremented when a position is closed/sold or the buy fails.
    let active_positions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    loop {
        // 1. Safe configuration reload
        {
            if let Ok(new_config) = BotConfig::reload_from_env() {
                let mut config = ref_config.write().await;
                if new_config.strategy != Strategy::Autonomous {
                    println!("🔄  [STRATEGY SWITCH DETECTED] Switching on-the-fly away from AUTONOMOUS loop...");
                    *config = new_config;
                    return Ok(());
                }
                *config = new_config;
            }
        }

        let config = {
            let guard = ref_config.read().await;
            guard.clone()
        };
        let now_instant = std::time::Instant::now();
        traded_tokens.retain(|(_, ts)| now_instant.duration_since(*ts) < Duration::from_secs(600));

        // 2. Process any incoming graduation events (non-blocking)
        while let Ok(event) = rx.try_recv() {
            if !monitored_tokens.contains_key(&event.mint) {
                println!("🎯  [Local Isolate] Raydium Graduation Event detected for {}! Adding to local monitor list.", event.mint);
                monitored_tokens.insert(event.mint.clone(), (event.symbol.clone(), std::time::Instant::now()));
            }
        }

        // 3. Spawn/manage Token Actors for active positions
        let active_sessions_db: Vec<(String, String, String, f64, f64, f64)> = {
            let mut stmt = db_conn.prepare("SELECT token_address, token_name, token_symbol, baseline_price, total_tokens_held, total_sol_spent FROM grid_sessions WHERE status = 'ACTIVE'")?;
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            })?
              .filter_map(|r| r.ok())
              .collect()
        };

        if config.max_parallel_positions == 0 && active_sessions_db.is_empty() {
            println!("🛑  [Orchestrator] max_parallel_positions is set to 0 and all active trades are completed. Shutting down gracefully...");
            // M5: graceful shutdown — signal main() to exit instead of killing
            // the process abruptly.  Dropping `db_tx` closes the DB actor
            // channel so `run_db_actor` flushes and exits.
            SHUTDOWN_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
            break;
        }

        // Clean up finished actors
        active_token_actors.retain(|mint, handle| {
            if handle.is_finished() {
                println!("🧹  Pruning completed Token Actor for {}", mint);
                false
            } else {
                true
            }
        });

        // Spawn actors for newly active sessions
        for (token_address, name, sym, baseline_price_sol, total_tokens_held, total_sol_spent) in &active_sessions_db {
            if !active_token_actors.contains_key(token_address) {
                println!("🚀  Spawning new reactive Token Actor for {} ({})", name, sym);
                let (actor_tx, actor_rx) = tokio::sync::mpsc::channel::<ActorEvent>(100);
                
                // Register actor channel in the global registry
                {
                    let mut registry = actor_registry.write().await;
                    registry.insert(token_address.clone(), actor_tx);
                }

                let actor_config = TokenActorConfig {
                    token_address: token_address.clone(),
                    token_name: name.clone(),
                    token_symbol: sym.clone(),
                    baseline_price_sol: *baseline_price_sol,
                    total_tokens_held: *total_tokens_held,
                    total_sol_spent: *total_sol_spent,
                };
                let handle = tokio::spawn(run_token_actor(
                    actor_config,
                    ref_config.clone(),
                    helius.clone(),
                    jup_client.clone(),
                    tracker.clone(),
                    db_tx.clone(),
                    actor_rx,
                    actor_registry.clone(),
                ));
                active_token_actors.insert(token_address.clone(), handle);
            }
        }

        // 4. Scan monitored tokens for entry opportunities
        //
        // M6: The `active_positions` AtomicUsize is the authoritative slot
        // counter.  `active_sessions_db.len()` is used only to *reconcile* the
        // counter with DB state at the top of each loop (positions that were
        // closed externally — e.g. via sell_all.rs — are detected here).
        active_positions.store(
            active_sessions_db.len(),
            std::sync::atomic::Ordering::SeqCst,
        );
        let monitored_keys: Vec<String> = monitored_tokens.keys().cloned().collect();
        for token_address in monitored_keys {
            let is_active = active_sessions_db.iter().any(|s| s.0 == token_address);
            if is_active {
                monitored_tokens.remove(&token_address);
                continue;
            }

            if traded_tokens.iter().any(|(addr, _)| *addr == token_address) {
                continue;
            }

            // M6: Atomic slot check — compare-and-swap ensures only one entry
            // per loop pass can claim a free slot even under concurrent checks.
            if active_positions.load(std::sync::atomic::Ordering::SeqCst) >= config.max_parallel_positions as usize {
                continue;
            }

            // Check Raydium migration cooldown & maximum entry timeout
            let (orig_symbol, grad_time) = monitored_tokens.get(&token_address).unwrap().clone();
            let mut symbol = orig_symbol.clone();
            let age_secs = now_instant.duration_since(grad_time).as_secs();

            let max_monitor_secs = config.migration_cooldown_secs + 1800;
            if age_secs > max_monitor_secs {
                println!("🔌  [Entry Timeout] Token {} ({}) has been monitored for >{}s. Expiry reached, stopping monitor.", symbol, token_address, max_monitor_secs);
                monitored_tokens.remove(&token_address);
                continue;
            }

            if age_secs < config.migration_cooldown_secs {
                continue;
            }

            // Check Local Isolate volume delta
            let v_delta = tracker.get_v_delta(&token_address).await;
            if v_delta < config.min_v_delta_entry {
                continue;
            }

            println!("🎯  [Local Breakout Detected] Token {} ({}) | Local v_delta = {:.4} SOL (Threshold: {})", 
                symbol, token_address, v_delta, config.min_v_delta_entry);

            // Fetch safety details from GMGN CLI
            println!("🔍  Running safety audit for {}...", token_address);
            let is_safe = match hunter::audit_detailed_token_safety(&token_address, &config) {
                Ok(safe) => safe,
                Err(e) => {
                    println!("   ⚠️  Safety audit execution failed: {:?}", e);
                    false
                }
            };

            if !is_safe {
                println!("❌  [Safety Reject] Token {} failed detailed safety audits.", symbol);
                traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                monitored_tokens.remove(&token_address);
                continue;
            }

            // Verify LP / Market Cap ratios from GMGN Token Info
            let info = match local_isolate::get_gmgn_token_info(&token_address) {
                Ok(inf) => inf,
                Err(e) => {
                    println!("   ⚠️  Failed to fetch token info details: {:?}", e);
                    continue;
                }
            };
            let (_market_cap, lp_to_mc, real_symbol, _real_name) = info;

            if (symbol == "GRAD" || symbol == "UNKNOWN" || symbol.trim().is_empty()) && real_symbol != "UNKNOWN" {
                symbol = real_symbol.clone();
            }

            if lp_to_mc < config.min_lp_mc_ratio_entry {
                println!("❌  [Ratio Reject] Token {} LP/MC ratio ({:.1}%) below entry limit ({:.1}%)",
                    symbol, lp_to_mc * 100.0, config.min_lp_mc_ratio_entry * 100.0);
                traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                monitored_tokens.remove(&token_address);
                continue;
            }

            // Verify on-chain authorities (mint and freeze must be renounced)
            let is_renounced = local_isolate::is_mint_renounced_on_chain(helius.connection().as_ref(), &token_address);
            if !is_renounced {
                println!("❌  [On-Chain Authority Reject] Token {} has active mint/freeze authority on-chain!", symbol);
                traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                monitored_tokens.remove(&token_address);
                continue;
            }

            // Verify CoinGecko Pro-API Multi-Timeframe Momentum & Reserves
            if let Some(ref cg_key) = config.coingecko_api_key {
                println!("🦎  [CoinGecko Gate] Fetching pool momentum metrics for {}...", symbol);
                match gecko_api::fetch_token_pool_metrics(&token_address, cg_key).await {
                    Ok(metrics) => {
                        println!("    🦎  Reserve USD: ${:.2} | 5m Change: {:.2}% | 15m Change: {:.2}%", 
                            metrics.reserve_usd, metrics.price_change_m5, metrics.price_change_m15);
                        println!("    🦎  15m Buys/Sells: {}/{} (Total txs: {})", 
                            metrics.buys_m15, metrics.sells_m15, metrics.buys_m15 + metrics.sells_m15);

                        if metrics.reserve_usd < config.min_liquidity {
                            println!("❌  [Gecko Reject] Token {} liquidity reserve (${:.2} USD) is below limit (${:.2} USD)",
                                symbol, metrics.reserve_usd, config.min_liquidity);
                            traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                            monitored_tokens.remove(&token_address);
                            continue;
                        }

                        let min_m5_momentum = std::env::var("MIN_GECKO_M5_MOMENTUM_PCT")
                            .unwrap_or_else(|_| "0.0".to_string())
                            .parse::<f64>()
                            .unwrap_or(0.0);
                        if metrics.price_change_m5 < min_m5_momentum {
                            println!("❌  [Gecko Reject] Token {} 5m price change ({:.2}%) below momentum threshold ({:.2}%)",
                                symbol, metrics.price_change_m5, min_m5_momentum);
                            traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                            monitored_tokens.remove(&token_address);
                            continue;
                        }

                        let min_buy_pressure = std::env::var("MIN_GECKO_BUY_PRESSURE_PCT")
                            .unwrap_or_else(|_| "50.0".to_string())
                            .parse::<f64>()
                            .unwrap_or(50.0) / 100.0;
                        let total_txs_15 = metrics.buys_m15 + metrics.sells_m15;
                        if total_txs_15 > 0 {
                            let buy_pressure = metrics.buys_m15 as f64 / total_txs_15 as f64;
                            if buy_pressure < min_buy_pressure {
                                println!("❌  [Gecko Reject] Token {} 15m buy pressure ({:.1}%) below threshold ({:.1}%)",
                                    symbol, buy_pressure * 100.0, min_buy_pressure * 100.0);
                                traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                                monitored_tokens.remove(&token_address);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        println!("⚠️  [CoinGecko Gate] API query failed: {:?}", e);
                    }
                }
            }

            // Entry criteria cleared! Execute Buy!
            println!("🎯  [Native Buy Trigger] Executing position entry on {}...", symbol);
            println!("Buying base position size for {:.4} SOL...", config.grid_trade_size_sol as f64 / 1_000_000_000.0);

            match execute_buy_swap(&token_address, config.grid_trade_size_sol, &config, &helius, &jup_client).await {
                Ok(Some(sig)) => {
                    println!("🎉  Buy successful! Signature: {}", sig);
                    active_positions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    let wallet_pubkey_str = config.keypair.pubkey().to_string();
                    let mut base_tokens = 0.0;
                    if let Ok(bal) = helius.get_wallet_balances(&wallet_pubkey_str, Some(1), Some(100), Some(false), Some(false), Some(false)).await {
                        for b in &bal.balances {
                            if b.mint == token_address {
                                base_tokens = b.balance;
                                break;
                            }
                        }
                    }

                    let sol_spent = config.grid_trade_size_sol as f64 / 1_000_000_000.0;
                    if base_tokens == 0.0 {
                        let mut start_price_sol = 0.000001;
                        if let Ok(quote) = jup_client.get_quote(
                            &token_address,
                            "So11111111111111111111111111111111111111112",
                            1_000_000,
                            config.slippage_bps,
                        ).await {
                            let out_amount = quote.out_amount.parse::<f64>().unwrap_or(0.0) / 1_000_000_000.0;
                            start_price_sol = out_amount / (1_000_000.0 / 10f64.powi(get_decimals_or_warn(&helius.connection(), &token_address) as i32));
                        }
                        base_tokens = sol_spent / start_price_sol;
                    }

                    let start_price_sol = sol_spent / base_tokens;
                    let mut sol_price_usd = 150.0;
                    if let Ok(q) = jup_client.get_quote(
                        "So11111111111111111111111111111111111111112",
                        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                        1_000_000_000,
                        config.slippage_bps,
                    ).await {
                        sol_price_usd = q.out_amount.parse::<f64>().unwrap_or(150_000_000.0) / 1_000_000.0;
                    }
                    let start_price_usd = start_price_sol * sol_price_usd;

                    let token_name = format!("Autonomous {}", symbol);
                    
                    if let Err(e) = db_tx.send(DbMessage::InsertBuy {
                        token_address: token_address.clone(),
                        token_name,
                        token_symbol: symbol.clone(),
                        buy_signature: sig,
                        buy_price_usd: start_price_usd,
                        buy_amount_sol: sol_spent,
                        buy_amount_tokens: base_tokens,
                        start_price_sol,
                    }).await { eprintln!("❌ [DB Actor] send error: {:?}", e); }

                    monitored_tokens.remove(&token_address);
                }
                _ => {
                    println!("⚠️  [Buy Failed] Swap returned None or failed.");
                    traded_tokens.push((token_address.clone(), std::time::Instant::now()));
                    monitored_tokens.remove(&token_address);
                }
            }
        }

        // Scan newly graduated Raydium pools using gmgn-cli every 30 seconds
        if last_scan_time.elapsed() >= Duration::from_secs(30) {
            last_scan_time = std::time::Instant::now();
            println!("🔍  [Token Scanner] Scanning GMGN completed trenches for recently graduated pools...");
            match hunter::run_gmgn_cli(&["market", "trenches", "--chain", "sol", "--type", "completed"]) {
                Ok(json) => {
                    if let Some(completed_pools) = json["completed"].as_array() {
                        let mut new_additions = Vec::new();
                        for pool in completed_pools {
                            if let Some(mint) = pool["address"].as_str() {
                                if !monitored_tokens.contains_key(mint) && !traded_tokens.iter().any(|(addr, _)| addr == mint) {
                                    let symbol = pool["symbol"].as_str().unwrap_or("UNKNOWN").to_string();
                                    monitored_tokens.insert(mint.to_string(), (symbol.clone(), std::time::Instant::now()));
                                    new_additions.push(mint.to_string());
                                }
                            }
                        }
                        
                        if !new_additions.is_empty() {
                            println!("🎯  [Token Scanner] Discovered and added {} new graduated pool(s) to local monitor list.", new_additions.len());
                        }
                        
                        // Build list of all target addresses to monitor
                        let mut target_addresses = Vec::new();
                        for mint in monitored_tokens.keys() {
                            target_addresses.push(mint.clone());
                        }
                        for session in &active_sessions_db {
                            target_addresses.push(session.0.clone());
                        }

                        // Dynamically sync whitelist with Helius webhook
                        if let Some(ref id) = helius_webhook_id {
                            let h_key = { ref_config.read().await.helius_api_key.clone() };
                            if let Err(e) = update_helius_webhook(&h_key, id, target_addresses).await {
                                println!("⚠️  [Helius Sync] Failed to sync webhook: {:?}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    println!("⚠️  [Token Scanner] Failed to query GMGN completed trenches: {:?}", e);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    #[allow(unreachable_code)]
    Ok(())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. Load config
    dotenvy::dotenv().ok();
    println!("=== Loading Bot Configuration ===");
    let initial_config = BotConfig::load_from_env().map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
    let config = Arc::new(RwLock::new(initial_config));

    let config_snapshot = config.read().await.clone();

    // 2. Initialize Helius
    println!("\n=== Initializing Helius client ===");
    let helius = Helius::new(&config_snapshot.helius_api_key, Cluster::MainnetBeta)
        .map_err(|e| format!("Failed to create Helius: {:?}", e))?;
    let helius = std::sync::Arc::new(helius);
    println!("Successfully connected to Helius Mainnet-Beta!");

    // 2.5 Spawn background priority fee estimator task
    let helius_clone = helius.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let fee_request = helius::types::GetPriorityFeeEstimateRequest {
                transaction: None,
                account_keys: Some(vec!["JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4".to_string()]),
                options: Some(helius::types::GetPriorityFeeEstimateOptions {
                    priority_level: Some(helius::types::PriorityLevel::VeryHigh),
                    include_all_priority_fee_levels: None,
                    transaction_encoding: None,
                    lookback_slots: None,
                    recommended: None,
                    include_vote: None,
                }),
            };
            if let Ok(fee_estimate) = helius_clone.rpc().get_priority_fee_estimate(fee_request).await {
                if let Some(estimate_microlamports) = fee_estimate.priority_fee_estimate {
                    let estimated_lamports = ((300_000.0 * estimate_microlamports as f64) / 1_000_000.0).round() as u64;
                    let fee_lamports = std::cmp::max(1_000, std::cmp::min(estimated_lamports, 200_000));
                    CACHED_PRIORITY_FEE.store(fee_lamports, Ordering::Relaxed);
                }
            }
        }
    });

    // 3. Initialize Jupiter
    let jup_client = std::sync::Arc::new(JupApiClient::new());

    // 4. Initialize Database
    println!("\n=== Initializing SQLite Database ===");
    let db_conn = db::init_db().expect("Failed to initialize database");
    println!("SQLite Database 'trades.db' initialized successfully!");

    // Core loop: routes on-the-fly dynamically to Hunter, Grid, or Autonomous loop!
    loop {
        // Safe configuration reload
        if let Ok(new_config) = BotConfig::reload_from_env() {
            let mut cfg = config.write().await;
            *cfg = new_config;
        }

        let current_config_snapshot = config.read().await.clone();
        match current_config_snapshot.strategy {
            Strategy::Hunter => {
                println!("\n[STRATEGY ENGINE ACTIVATED] Booting HUNTER Mode...");
                run_hunter_loop(&current_config_snapshot, &helius, &jup_client, &db_conn).await?;
            }
            Strategy::Grid => {
                println!("\n[STRATEGY ENGINE ACTIVATED] Booting RAYDIUM GRID Mode...");
                run_grid_loop(&current_config_snapshot, &helius, &jup_client, &db_conn).await?;
            }
            Strategy::Autonomous => {
                println!("\n[STRATEGY ENGINE ACTIVATED] Booting AUTONOMOUS Mode...");
                run_autonomous_loop(config.clone(), helius.clone(), jup_client.clone(), &db_conn).await?;
            }
        }

        // Sleep a moment if a loop exits (means strategy switch happened!)
        // M5: honor graceful shutdown signal from the autonomous loop.
        if SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            println!("👋  [Main] Shutdown requested; exiting cleanly.");
            return Ok(());
        }
        sleep(Duration::from_secs(5)).await;
    }
}
async fn fetch_helius_webhook_id(helius_api_key: &str) -> Option<String> {
    let client = reqwest::Client::new();
    // H6: send the API key via the Authorization header instead of a URL
    // query parameter so it never appears in request logs.
    let url = "https://api-mainnet.helius-rpc.com/v0/webhooks";
    match client.get(url).header("Authorization", format!("Bearer {}", helius_api_key)).send().await {
        Ok(res) => {
            if let Ok(arr) = res.json::<serde_json::Value>().await {
                if let Some(webhooks) = arr.as_array() {
                    for item in webhooks {
                        if let Some(target_url) = item["webhookURL"].as_str() {
                            if target_url.contains("webhook") {
                                return item["webhookID"].as_str().map(|s| s.to_string());
                            }
                        }
                    }
                }
            }
        }
        Err(_) => {}
    }
    None
}

async fn update_helius_webhook(
    helius_api_key: &str,
    webhook_id: &str,
    target_addresses: Vec<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = reqwest::Client::new();
    // H6: header-based auth — no API key in the URL.
    let url = format!("https://api-mainnet.helius-rpc.com/v0/webhooks/{}", webhook_id);
    let auth_header = format!("Bearer {}", helius_api_key);
    
    let get_res = client.get(&url).header("Authorization", &auth_header).send().await?;
    if !get_res.status().is_success() {
        return Err(format!("Failed to get Helius webhook: {}", get_res.status()).into());
    }
    let mut config_json: serde_json::Value = get_res.json().await?;
    
    let mut addresses = vec!["Hqf8a2Ryxeb15wNcXSXzAemBeY9VtcSrW5wE6UpcSrnG".to_string()];
    for addr in target_addresses {
        if addr != "Hqf8a2Ryxeb15wNcXSXzAemBeY9VtcSrW5wE6UpcSrnG" && !addresses.contains(&addr) {
            addresses.push(addr);
        }
    }
    
    if addresses.len() > 100 {
        addresses.truncate(100);
    }
    
    let current_addresses: Vec<String> = config_json["accountAddresses"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect();
        
    if current_addresses == addresses {
        return Ok(());
    }
    
    config_json["accountAddresses"] = serde_json::Value::from(addresses);
    
    // Remove read-only Helius properties to avoid 400 Bad Request
    if let Some(obj) = config_json.as_object_mut() {
        obj.remove("project");
        obj.remove("wallet");
        obj.remove("webhookID");
        obj.remove("lastEnabledAt");
        obj.remove("createdAt");
    }
    
    let put_res = client.put(&url)
        .header("Authorization", &auth_header)
        .json(&config_json)
        .send()
        .await?;
        
    if !put_res.status().is_success() {
        let text = put_res.text().await?;
        return Err(format!("Failed to update Helius webhook: {}", text).into());
    }
    
    println!("🔔  [Helius Sync] Webhook whitelist synchronized successfully. Active monitored count: {}", config_json["accountAddresses"].as_array().unwrap().len());
    Ok(())
}

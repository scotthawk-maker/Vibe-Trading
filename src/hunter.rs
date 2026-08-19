use std::sync::{LazyLock, Mutex};
use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};
use serde_json::Value;
use std::error::Error;
use crate::validation;

#[derive(Debug, Clone, PartialEq)]
pub struct GmgnTokenCandidate {
    pub address: String,
    pub symbol: String,
    pub name: String,
    pub price: f64,
    pub progress: f64,
}

/// Maximum time to wait for a gmgn-cli subprocess before killing it (H5).
const GMGN_TIMEOUT: Duration = Duration::from_secs(15);
/// Maximum stdout/stderr bytes to collect from gmgn-cli (H5).
const GMGN_OUTPUT_CAP: usize = 1_000_000;

/// Run a command with a timeout and an output-size cap (H5).  Spawns the
/// process, polls `try_wait` every 50 ms, and `kill()`s it if it exceeds
/// [`GMGN_TIMEOUT`].  Collected stdout is truncated at [`GMGN_OUTPUT_CAP`].
fn run_command_with_timeout(
    cmd_path: &str,
    args: &[&str],
) -> Result<Value, Box<dyn Error + Send + Sync>> {
    use std::io::Read;
    use std::process::Stdio;

    let mut cmd = Command::new(cmd_path);
    cmd.args(args);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let deadline = Instant::now() + GMGN_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(_status) => break,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("gmgn-cli timed out after {:?} (args: {:?})", GMGN_TIMEOUT, args).into());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    // Collect output (capped).
    let mut stdout = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        let mut buf = [0u8; 8192];
        loop {
            let n = out.read(&mut buf)?;
            if n == 0 {
                break;
            }
            if stdout.len() + n > GMGN_OUTPUT_CAP {
                stdout.extend_from_slice(&buf[..GMGN_OUTPUT_CAP.saturating_sub(stdout.len())]);
                break;
            }
            stdout.extend_from_slice(&buf[..n]);
        }
    }
    let mut stderr = String::new();
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
        if stderr.len() > GMGN_OUTPUT_CAP {
            stderr.truncate(GMGN_OUTPUT_CAP);
        }
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(format!("gmgn-cli failed: {}", stderr).into());
    }
    let json: Value = serde_json::from_slice(&stdout)?;
    Ok(json)
}

/// Resolve the full path to `gmgn-cli` at first use (H5).  Honours the
/// `GMGN_CLI_PATH` env var; otherwise falls back to the bare `gmgn-cli`
/// name (relying on PATH as before).
static GMGN_CLI_PATH: LazyLock<String> = LazyLock::new(|| {
    std::env::var("GMGN_CLI_PATH").unwrap_or_else(|_| "gmgn-cli".to_string())
});

/// Helper to execute a gmgn-cli command and parse raw JSON output.
///
/// H5: uses the resolved binary path, enforces a timeout and output cap, and
/// passes arguments explicitly (no shell) - so this is not vulnerable to
/// shell injection.  Callers that pass a token address must validate it first
/// via [`validation::is_valid_solana_pubkey`].
pub fn run_gmgn_cli(args: &[&str]) -> Result<Value, Box<dyn Error + Send + Sync>> {
    let mut all_args: Vec<&str> = args.to_vec();
    if !all_args.contains(&"--raw") {
        all_args.push("--raw");
    }
    run_command_with_timeout(&GMGN_CLI_PATH, &all_args)
}

/// Scan GMGN Trenches for newly created tokens on Pump.fun at 50-60% bonding curve progress
pub fn scan_bonding_curve_trenches(cooldown_addresses: &[String], config: &crate::BotConfig) -> Result<Vec<GmgnTokenCandidate>, Box<dyn Error + Send + Sync>> {
    let type_str = match config.strategy {
        crate::config::Strategy::Grid => "completed",
        _ => "new_creation",
    };

    let mut args = vec![
        "market", "trenches",
        "--chain", "sol",
        "--type", type_str,
    ];

    let min_progress_str;
    let max_progress_str;

    match config.strategy {
        crate::config::Strategy::Hunter => {
            println!("Scanning SOL trenches for Stage 1 bonding curve progress ({}% - {}%)...", config.min_progress_pct, config.max_progress_pct);
            min_progress_str = format!("{:.2}", config.min_progress_pct / 100.0);
            max_progress_str = format!("{:.2}", config.max_progress_pct / 100.0);
            args.push("--min-progress");
            args.push(&min_progress_str);
            args.push("--max-progress");
            args.push(&max_progress_str);
        }
        crate::config::Strategy::Grid | crate::config::Strategy::Autonomous => {
            println!("Scanning Raydium for newly migrated Stage 2 pools...");
        }
    }

    let json = run_gmgn_cli(&args)?;

    let mut candidates = Vec::new();

    if let Some(list_array) = json[type_str].as_array() {
        for item in list_array {
            let address = item["address"].as_str().unwrap_or("").to_string();

            // Skip recently traded tokens inside the cooldown list
            if cooldown_addresses.contains(&address) {
                continue;
            }

            let symbol = item["symbol"].as_str().unwrap_or("").to_string();
            let name = item["name"].as_str().unwrap_or("").to_string();

            // Calculate implied token price from market cap and total supply
            let mcap = item["usd_market_cap"].as_f64().unwrap_or(0.0);
            let supply = item["total_supply"].as_f64().unwrap_or(1_000_000_000.0);
            let price = mcap / supply;

            let progress = item["progress"].as_f64().unwrap_or(0.0);
            let liquidity = item["liquidity"].as_f64().unwrap_or(0.0);
            let is_wash = item["is_wash_trading"].as_bool().unwrap_or(false);

            // Safety audits are already returned inline in the trenches data!
            // Note: For completed/migrated tokens, GMGN's completed API often reports false/null for renounced freeze/mint,
            // but Pump.fun tokens are guaranteed on-chain to have freeze/mint authority permanently renounced.
            let renounced_freeze = item["renounced_freeze_account"].as_bool().unwrap_or(false) || type_str == "completed";
            let renounced_mint = item["renounced_mint"].as_bool().unwrap_or(false) || type_str == "completed";

            // Bundle safety limits:
            let top_10_holder_rate = item["top_10_holder_rate"].as_f64().unwrap_or(1.0);
            let dev_team_hold_rate = item["dev_team_hold_rate"].as_f64().unwrap_or(1.0);
            let creator_balance_rate = item["creator_balance_rate"].as_f64().unwrap_or(1.0);
            let suspected_insider_hold_rate = item["suspected_insider_hold_rate"].as_f64().unwrap_or(1.0);
            let has_socials = item["has_at_least_one_social"].as_bool().unwrap_or(false);

            // Smart money & Rat trader metrics:
            let smart_degen_count = item["smart_degen_count"].as_u64().unwrap_or(0) as u32;
            let rat_trader_rate = item["rat_trader_amount_rate"].as_f64().unwrap_or(1.0);

            let is_honeypot = item["is_honeypot"].as_str().unwrap_or("no").to_string();
            let top70_sniper_hold_rate = item["top70_sniper_hold_rate"].as_f64().unwrap_or(0.0);

            // FILTERS:
            // 1. Minimum liquidity (completed tokens return liquidity in SOL, so we convert to USD assuming ~$140/SOL)
            // 2. Wash trading detected must be false
            // 3. Freeze authority must be renounced (crucial!)
            // 4. Mint authority must be renounced
            // 5. Top 10 holder rate <= config limit (prevents sniper dumps)
            // 6. Dev / team hold rate <= config limit (prevents team rugs)
            // 7. Creator balance rate <= config limit (prevents dev rugs)
            // 8. Suspected insider rate <= config limit
            // 9. Require socials if configured
            // 10. Smart money count >= config limit (bypassed for completed tokens since it takes time to index on Raydium)
            // 11. Rat trader rate <= config limit
            // 12. Cannot be flagged as honeypot
            // 13. Top 70 sniper hold rate <= 50% (prevents massive dev bundle launches)
            let liquidity_usd = if type_str == "completed" {
                liquidity * 140.0
            } else {
                liquidity
            };

            // Dynamic thresholds tailored for Raydium Post-Migration (completed) pools vs standard hunter progress
            let min_liq_threshold = if type_str == "completed" { 10000.0 } else { config.min_liquidity };
            let max_top_10_threshold = if type_str == "completed" { 0.25 } else { config.max_top_10_holder_rate };
            let max_dev_team_threshold = if type_str == "completed" { 0.08 } else { config.max_dev_team_hold_rate };
            let max_creator_threshold = if type_str == "completed" { 0.05 } else { config.max_creator_balance_rate };
            let max_insider_threshold = if type_str == "completed" { 0.10 } else { config.max_suspected_insider_rate };
            let max_rat_trader_threshold = if type_str == "completed" { 0.05 } else { config.max_rat_trader_rate };
            let require_socials_threshold = if type_str == "completed" { true } else { config.require_socials };

            let passes_smart_degen = smart_degen_count >= config.min_smart_degen_count || type_str == "completed";

            if liquidity_usd >= min_liq_threshold
                && !is_wash
                && is_honeypot != "yes"
                && renounced_freeze
                && renounced_mint
                && top_10_holder_rate <= max_top_10_threshold
                && dev_team_hold_rate <= max_dev_team_threshold
                && creator_balance_rate <= max_creator_threshold
                && suspected_insider_hold_rate <= max_insider_threshold
                && top70_sniper_hold_rate <= 0.50
                && (!require_socials_threshold || has_socials)
                && passes_smart_degen
                && rat_trader_rate <= max_rat_trader_threshold
            {
                candidates.push(GmgnTokenCandidate {
                    address,
                    symbol,
                    name,
                    price,
                    progress,
                });
            }
        }
    }

    let list_name = match config.strategy {
        crate::config::Strategy::Grid => "Raydium migrated pools",
        _ => "50-60% bonding curve",
    };
    println!("Found {} safe candidates on the {} list.", candidates.len(), list_name);
    Ok(candidates)
}

/// Hunt for the top active and safe bonding-curve token on Solana
pub fn hunt_golden_candidate(cooldown_addresses: &[String], config: &crate::BotConfig) -> Result<Option<GmgnTokenCandidate>, Box<dyn Error + Send + Sync>> {
    let candidates = scan_bonding_curve_trenches(cooldown_addresses, config)?;

    if let Some(first) = candidates.first() {
        println!("\n🎯 FOUND GOLDEN BONDING CURVE CANDIDATE:");
        println!("   Name:     {}", first.name);
        println!("   Symbol:   {}", first.symbol);
        println!("   Address:  {}", first.address);
        println!("   Progress: {:.2}%", first.progress * 100.0);

        if let Ok(rating) = get_whale_rating(&first.address) {
            println!("   🐳 Whale Wallets:  {}", rating.whale_wallets);
            println!("   🧠 Smart Wallets:  {}", rating.smart_wallets);
            println!("   🏆 Top Traders:    {}", rating.top_wallets);
            println!("   ⭐ Whale Rating:   {} points", rating.rating_score);
        }

        return Ok(Some(first.clone()));
    }

    let list_name = match config.strategy {
        crate::config::Strategy::Grid => "Raydium migrated pools",
        _ => "50-60% bonding curve",
    };
    println!("\n❌ No safe golden candidates found on the {} list.", list_name);
    Ok(None)
}


static SYMBOL_REGISTRY: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| {
    Mutex::new(HashMap::new())
});

static NAME_REGISTRY: LazyLock<Mutex<HashMap<String, String>>> = LazyLock::new(|| {
    Mutex::new(HashMap::new())
});

pub fn check_copycat_scam(address: &str, name: &str, symbol: &str, db_conn: &rusqlite::Connection) -> bool {
    let clean_symbol = symbol.trim().to_lowercase();
    let clean_name = name.trim().to_lowercase();
    let clean_address = address.trim();

    // 1. Check against SQLite database trades history (historical copycats)
    if let Ok(mut stmt) = db_conn.prepare("SELECT token_address FROM trades WHERE LOWER(token_symbol) = ?1 OR LOWER(token_name) = ?2 LIMIT 1") {
        if let Ok(existing_ca) = stmt.query_row([&clean_symbol, &clean_name], |row| row.get::<_, String>(0)) {
            let existing_ca_clean = existing_ca.trim();
            if !existing_ca_clean.is_empty() && existing_ca_clean.to_lowercase() != clean_address.to_lowercase() {
                println!("  🚨 [COPYCAT DETECTED] Token {} ({}) uses a duplicate name/symbol of historical trade {}! Reclaiming slot...", name, symbol, existing_ca_clean);
                return true;
            }
        }
    }

    // 2. Check against in-memory registry of recently scanned graduating pools.
    //    M6: acquire BOTH locks in a single scope with a fixed lock order
    //    (symbol then name) so the check-then-insert is atomic across the two
    //    maps and a token cannot be half-registered.
    if let (Ok(mut sym_reg), Ok(mut name_reg)) = (SYMBOL_REGISTRY.lock(), NAME_REGISTRY.lock()) {
        if let Some(existing_ca) = sym_reg.get(&clean_symbol) {
            let existing_ca_clean = existing_ca.trim();
            if !existing_ca_clean.is_empty() && existing_ca_clean.to_lowercase() != clean_address.to_lowercase() {
                println!("  🚨 [COPYCAT DETECTED] Token {} ({}) uses a duplicate symbol of recently scanned token {}!", name, symbol, existing_ca_clean);
                return true;
            }
        }
        if let Some(existing_ca) = name_reg.get(&clean_name) {
            let existing_ca_clean = existing_ca.trim();
            if !existing_ca_clean.is_empty() && existing_ca_clean.to_lowercase() != clean_address.to_lowercase() {
                println!("  🚨 [COPYCAT DETECTED] Token {} ({}) uses a duplicate name of recently scanned token {}!", name, symbol, existing_ca_clean);
                return true;
            }
        }
        // Neither symbol nor name is registered yet — insert both atomically.
        sym_reg.insert(clean_symbol.clone(), clean_address.to_string());
        name_reg.insert(clean_name.clone(), clean_address.to_string());
    }

    false
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WhaleRating {
    pub whale_wallets: u32,
    pub smart_wallets: u32,
    pub top_wallets: u32,
    pub rating_score: u32,
}

pub fn get_whale_rating(token_address: &str) -> Result<WhaleRating, Box<dyn Error + Send + Sync>> {
    // H5: validate the token address before passing it to the subprocess.
    if !validation::is_valid_solana_pubkey(token_address) {
        return Err(format!("invalid token address passed to get_whale_rating: {}", token_address).into());
    }
    let res_json = run_command_with_timeout(
        &GMGN_CLI_PATH,
        &["token", "info", "--address", token_address, "--chain", "sol", "--raw"],
    )?;
    
    let tags = &res_json["wallet_tags_stat"];
    let whale_wallets = tags["whale_wallets"].as_u64().unwrap_or(0) as u32;
    let smart_wallets = tags["smart_wallets"].as_u64().unwrap_or(0) as u32;
    let top_wallets = tags["top_wallets"].as_u64().unwrap_or(0) as u32;

    // Rating score calculation:
    // Whales are weighted 3x, Smart money 2x, and Top traders 1x.
    let rating_score = (whale_wallets * 3) + (smart_wallets * 2) + top_wallets;

    Ok(WhaleRating {
        whale_wallets,
        smart_wallets,
        top_wallets,
        rating_score,
    })
}

pub fn audit_detailed_token_safety(token_address: &str, config: &crate::BotConfig) -> Result<bool, Box<dyn Error + Send + Sync>> {
    // H5: validate the token address before passing it to the subprocess.
    if !validation::is_valid_solana_pubkey(token_address) {
        return Err(format!("invalid token address passed to audit_detailed_token_safety: {}", token_address).into());
    }
    let res_json = run_command_with_timeout(
        &GMGN_CLI_PATH,
        &["token", "info", "--address", token_address, "--chain", "sol", "--raw"],
    )?;

    // Helper to parse numeric values that might be encoded as strings in GMGN JSON response
    let parse_f64 = |val: &serde_json::Value| -> f64 {
        if let Some(s) = val.as_str() {
            s.parse::<f64>().unwrap_or(0.0)
        } else if let Some(f) = val.as_f64() {
            f
        } else {
            0.0
        }
    };

    let stat = &res_json["stat"];
    let top_10_holder_rate = parse_f64(&stat["top_10_holder_rate"]);
    let dev_team_hold_rate = parse_f64(&stat["dev_team_hold_rate"]);
    let creator_hold_rate = parse_f64(&stat["creator_hold_rate"]);
    let suspected_insider_hold_rate = parse_f64(&stat["suspected_insider_hold_rate"]);
    let top70_sniper_hold_rate = parse_f64(&stat["top70_sniper_hold_rate"]);

    let is_honeypot = res_json["is_honeypot"].as_str().unwrap_or("no").to_string();

    let tags = &res_json["wallet_tags_stat"];
    let whale_wallets = tags["whale_wallets"].as_u64().unwrap_or(0) as u32;
    let smart_wallets = tags["smart_wallets"].as_u64().unwrap_or(0) as u32;
    let top_wallets = tags["top_wallets"].as_u64().unwrap_or(0) as u32;
    let rating_score = (whale_wallets * 3) + (smart_wallets * 2) + top_wallets;

    // Stricter, tailored safety thresholds optimized for Raydium Post-Migration
    let max_top_10 = config.max_top_10_holder_rate;
    let max_dev_team = config.max_dev_team_hold_rate;
    let max_creator = config.max_creator_balance_rate;
    let max_insider = config.max_suspected_insider_rate;
    let min_whale_score = config.min_whale_rating;

    println!("🔍 Detailed Token Safety Audit for {}:", token_address);
    println!("   Top 10 Holder:      {:.2}% (Limit: {}%)", top_10_holder_rate * 100.0, max_top_10 * 100.0);
    println!("   Dev Team Hold:      {:.2}% (Limit: {}%)", dev_team_hold_rate * 100.0, max_dev_team * 100.0);
    println!("   Creator Balance:    {:.2}% (Limit: {}%)", creator_hold_rate * 100.0, max_creator * 100.0);
    println!("   Suspected Insider:  {:.2}% (Limit: {}%)", suspected_insider_hold_rate * 100.0, max_insider * 100.0);
    println!("   Top 70 Sniper:      {:.2}% (Limit: 50.0%)", top70_sniper_hold_rate * 100.0);
    println!("   Is Honeypot:        {}", is_honeypot);
    println!("   🐳 Whale Wallets:  {}", whale_wallets);
    println!("   🧠 Smart Wallets:  {}", smart_wallets);
    println!("   🏆 Top Traders:    {}", top_wallets);
    println!("   ⭐ Whale Rating:   {} points (Required: {})", rating_score, min_whale_score);

    if top_10_holder_rate > max_top_10 {
        println!("   ❌ Rejected: Top 10 holder rate too high!");
        return Ok(false);
    }
    if dev_team_hold_rate > max_dev_team {
        println!("   ❌ Rejected: Dev/Team hold rate too high!");
        return Ok(false);
    }
    if creator_hold_rate > max_creator {
        println!("   ❌ Rejected: Creator balance too high!");
        return Ok(false);
    }
    if suspected_insider_hold_rate > max_insider {
        println!("   ❌ Rejected: Suspected insider hold rate too high!");
        return Ok(false);
    }
    if top70_sniper_hold_rate > 0.50 {
        println!("   ❌ Rejected: Top 70 snipers hold more than 50% of supply!");
        return Ok(false);
    }
    if is_honeypot == "yes" {
        println!("   ❌ Rejected: Token flagged as Honeypot!");
        return Ok(false);
    }
    if rating_score < min_whale_score {
        println!("   ❌ Rejected: Whale rating score too low ({} < {})!", rating_score, min_whale_score);
        return Ok(false);
    }

    println!("   ✅ Passed all detailed safety checks!");
    Ok(true)
}

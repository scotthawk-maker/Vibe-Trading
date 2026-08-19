use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Strategy {
    Hunter,
    Grid,
    Autonomous,
}

#[allow(dead_code)]
pub struct BotConfig {
    pub helius_api_key: String,
    pub keypair: Arc<Keypair>,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    pub trade_amount: u64,
    pub slippage_bps: u32,
    pub check_interval_secs: u64,
    pub dry_run: bool,
    pub limit_price_usdc: Option<f64>,
    pub sell_sol_target_price: Option<f64>,
    pub buy_sol_target_price: Option<f64>,
    pub trade_amount_sol: u64,
    pub trade_amount_usdc: u64,
    pub max_hold_time_minutes: u32,
    pub min_liquidity: f64,
    pub max_top_10_holder_rate: f64,
    pub max_dev_team_hold_rate: f64,
    pub max_creator_balance_rate: f64,
    pub max_suspected_insider_rate: f64,
    pub require_socials: bool,
    pub min_smart_degen_count: u32,
    pub max_rat_trader_rate: f64,
    pub min_progress_pct: f64,
    pub max_progress_pct: f64,
    pub take_profit_pct: f64,
    pub stop_loss_pct: f64,
    pub breakeven_trigger_pct: f64,
    pub breakeven_stop_loss_pct: f64,
    pub strategy: Strategy,
    pub grid_trade_size_sol: u64,
    pub grid_spacing_pct: f64,
    pub grid_profit_pct: f64,
    pub grid_max_levels: u32,
    pub webhook_port: u16,
    pub webhook_auth_token: Option<String>,
    pub jito_tip_lamports: u64,
    pub max_parallel_positions: usize,
    pub moonbag_retain_pct: f64,
    pub min_whale_rating: u32,
    pub migration_cooldown_secs: u64,
    pub min_v_delta_entry: f64,
    pub min_lp_mc_ratio_entry: f64,
    pub min_lp_mc_ratio_exit: f64,
    pub coingecko_api_key: Option<String>,
    // --- New configurable safety parameters ---
    /// Maximum acceptable Jupiter `price_impact_pct` before a swap is rejected
    /// (H4).  Defaults to 10.0 (%).
    pub max_price_impact_pct: f64,
    /// Balance threshold (in raw base units) below which the auto-nuke
    /// recycler will burn & close a token account (L2).  Defaults to 50000.
    pub dust_threshold_base_units: u64,
}

impl BotConfig {
    /// Load configuration from the process environment.
    ///
    /// Returns `Err` with a human-readable message for any malformed or
    /// missing *critical* value (H8) instead of panicking.  Non-critical
    /// values fall back to documented defaults.
    pub fn load_from_env() -> Result<Self, String> {
        let empty = HashMap::new();
        Self::load_from_env_with(&empty)
    }

    /// Load configuration, preferring values from `overrides` (a parsed `.env`
    /// map) and falling back to `std::env::var`.  This is the shared loader
    /// used by both the startup path and the runtime reload path (H1).
    fn load_from_env_with(overrides: &HashMap<String, String>) -> Result<Self, String> {
        let lookup = |key: &str| -> Option<String> {
            overrides
                .get(key)
                .cloned()
                .or_else(|| std::env::var(key).ok())
        };

        // --- Critical values: fail with a clear message on error ---

        let helius_api_key = lookup("HELIUS_API_KEY")
            .ok_or_else(|| "HELIUS_API_KEY is not set in the environment or .env file".to_string())?;

        // Read DRY_RUN *before* the keypair decision so we can decide whether
        // an ephemeral keypair fallback is acceptable (H3).
        let dry_run_str = lookup("DRY_RUN").unwrap_or_else(|| "true".to_string());
        let dry_run = dry_run_str.trim().to_lowercase() != "false" && dry_run_str.trim() != "0";

        // Keypair: fail hard in live mode if missing/unparseable; only allow
        // an ephemeral fallback during dry-run (H3).
        let keypair = match lookup("WALLET_PRIVATE_KEY") {
            Some(private_key_str) => parse_keypair(&private_key_str).ok_or_else(|| {
                "Failed to parse WALLET_PRIVATE_KEY. Must be a JSON byte array or Base58 string"
                    .to_string()
            })?,
            None => {
                if dry_run {
                    println!("⚠️  WALLET_PRIVATE_KEY not set in .env. Generating a temporary ephemeral keypair for dry-runs...");
                    Keypair::new()
                } else {
                    return Err(
                        "WALLET_PRIVATE_KEY is required when DRY_RUN=false (live trading)"
                            .to_string(),
                    );
                }
            }
        };

        let input_mint_str =
            lookup("INPUT_MINT").unwrap_or_else(|| "So11111111111111111111111111111111111111112".to_string());
        let input_mint = Pubkey::from_str(&input_mint_str)
            .map_err(|_| format!("Invalid INPUT_MINT pubkey: {}", input_mint_str))?;

        let output_mint_str =
            lookup("OUTPUT_MINT").unwrap_or_else(|| "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".to_string());
        let output_mint = Pubkey::from_str(&output_mint_str)
            .map_err(|_| format!("Invalid OUTPUT_MINT pubkey: {}", output_mint_str))?;

        let trade_amount_str = lookup("TRADE_AMOUNT_LAMPORTS").unwrap_or_else(|| "50000000".to_string());
        let trade_amount = trade_amount_str
            .parse::<u64>()
            .map_err(|_| format!("Invalid TRADE_AMOUNT_LAMPORTS integer: {}", trade_amount_str))?;

        let trade_amount_sol_str =
            lookup("TRADE_AMOUNT_SOL_LAMPORTS").unwrap_or_else(|| trade_amount_str.clone());
        let trade_amount_sol = trade_amount_sol_str
            .parse::<u64>()
            .map_err(|_| format!("Invalid TRADE_AMOUNT_SOL_LAMPORTS integer: {}", trade_amount_sol_str))?;

        let trade_amount_usdc_str =
            lookup("TRADE_AMOUNT_USDC_BASE").unwrap_or_else(|| "10000000".to_string());
        let trade_amount_usdc = trade_amount_usdc_str
            .parse::<u64>()
            .map_err(|_| format!("Invalid TRADE_AMOUNT_USDC_BASE integer: {}", trade_amount_usdc_str))?;

        let slippage_bps_str = lookup("SLIPPAGE_BPS").unwrap_or_else(|| "50".to_string());
        let slippage_bps = slippage_bps_str
            .parse::<u32>()
            .map_err(|_| format!("Invalid SLIPPAGE_BPS integer: {}", slippage_bps_str))?;

        let check_interval_str =
            lookup("CHECK_INTERVAL_SECS").unwrap_or_else(|| "15".to_string());
        let check_interval_secs = check_interval_str
            .parse::<u64>()
            .map_err(|_| format!("Invalid CHECK_INTERVAL_SECS integer: {}", check_interval_str))?;

        let max_hold_time_str =
            lookup("MAX_HOLD_TIME_MINUTES").unwrap_or_else(|| "30".to_string());
        let max_hold_time_minutes = max_hold_time_str
            .parse::<u32>()
            .map_err(|_| format!("Invalid MAX_HOLD_TIME_MINUTES integer: {}", max_hold_time_str))?;

        // --- Security filter thresholds (non-critical, use defaults) ---

        let min_liquidity = lookup("MIN_LIQUIDITY_USD")
            .unwrap_or_else(|| "3000".to_string())
            .parse::<f64>()
            .unwrap_or(3000.0);

        let max_top_10_holder_rate = lookup("MAX_TOP_10_HOLDER_PCT")
            .unwrap_or_else(|| "30".to_string())
            .parse::<f64>()
            .unwrap_or(30.0) / 100.0;

        let max_dev_team_hold_rate = lookup("MAX_DEV_TEAM_HOLD_PCT")
            .unwrap_or_else(|| "15".to_string())
            .parse::<f64>()
            .unwrap_or(15.0) / 100.0;

        let max_creator_balance_rate = lookup("MAX_CREATOR_BALANCE_PCT")
            .unwrap_or_else(|| "10".to_string())
            .parse::<f64>()
            .unwrap_or(10.0) / 100.0;

        let max_suspected_insider_rate = lookup("MAX_SUSPECTED_INSIDER_PCT")
            .unwrap_or_else(|| "15".to_string())
            .parse::<f64>()
            .unwrap_or(15.0) / 100.0;

        let require_socials_str =
            lookup("REQUIRE_SOCIALS").unwrap_or_else(|| "true".to_string());
        let require_socials =
            require_socials_str.trim().to_lowercase() != "false" && require_socials_str.trim() != "0";

        let min_smart_degen_count = lookup("MIN_SMART_DEGEN_COUNT")
            .unwrap_or_else(|| "0".to_string())
            .parse::<u32>()
            .unwrap_or(0);

        let max_rat_trader_rate = lookup("MAX_RAT_TRADER_PCT")
            .unwrap_or_else(|| "10".to_string())
            .parse::<f64>()
            .unwrap_or(10.0) / 100.0;

        let min_progress_pct = lookup("MIN_PROGRESS_PCT")
            .unwrap_or_else(|| "75".to_string())
            .parse::<f64>()
            .unwrap_or(75.0);

        let max_progress_pct = lookup("MAX_PROGRESS_PCT")
            .unwrap_or_else(|| "90".to_string())
            .parse::<f64>()
            .unwrap_or(90.0);

        let take_profit_pct = lookup("TAKE_PROFIT_PCT")
            .unwrap_or_else(|| "40".to_string())
            .parse::<f64>()
            .unwrap_or(40.0);

        let stop_loss_pct = lookup("STOP_LOSS_PCT")
            .unwrap_or_else(|| "-20".to_string())
            .parse::<f64>()
            .unwrap_or(-20.0);

        let breakeven_trigger_pct = lookup("BREAKEVEN_TRIGGER_PCT")
            .unwrap_or_else(|| "10.0".to_string())
            .parse::<f64>()
            .unwrap_or(10.0);

        let breakeven_stop_loss_pct = lookup("BREAKEVEN_STOP_LOSS_PCT")
            .unwrap_or_else(|| "1.0".to_string())
            .parse::<f64>()
            .unwrap_or(1.0);

        let strategy_str = lookup("STRATEGY").unwrap_or_else(|| "HUNTER".to_string());
        let strategy = match strategy_str.trim().to_uppercase().as_str() {
            "GRID" => Strategy::Grid,
            "AUTONOMOUS" => Strategy::Autonomous,
            _ => Strategy::Hunter,
        };

        let migration_cooldown_secs = lookup("MIGRATION_COOLDOWN_SECS")
            .unwrap_or_else(|| "90".to_string())
            .parse::<u64>()
            .unwrap_or(90);

        let min_v_delta_entry = lookup("MIN_V_DELTA_ENTRY")
            .unwrap_or_else(|| "5.0".to_string())
            .parse::<f64>()
            .unwrap_or(5.0);

        let min_lp_mc_ratio_entry = lookup("MIN_LP_MC_RATIO_ENTRY")
            .unwrap_or_else(|| "0.05".to_string())
            .parse::<f64>()
            .unwrap_or(0.05);

        let min_lp_mc_ratio_exit = lookup("MIN_LP_MC_RATIO_EXIT")
            .unwrap_or_else(|| "0.03".to_string())
            .parse::<f64>()
            .unwrap_or(0.03);

        // M2: validate the f64→u64 cast for the grid trade size.
        let grid_trade_size_str =
            lookup("GRID_TRADE_SIZE_SOL").unwrap_or_else(|| "0.02".to_string());
        let grid_trade_size_val = grid_trade_size_str.parse::<f64>().unwrap_or(0.02);
        let grid_trade_size_sol = if grid_trade_size_val.is_finite() && grid_trade_size_val >= 0.0 {
            (grid_trade_size_val * 1_000_000_000.0).round() as u64
        } else {
            return Err(format!("Invalid GRID_TRADE_SIZE_SOL value: {}", grid_trade_size_str));
        };

        let grid_spacing_pct = lookup("GRID_SPACING_PCT")
            .unwrap_or_else(|| "10.0".to_string())
            .parse::<f64>()
            .unwrap_or(10.0);

        let grid_profit_pct = lookup("GRID_PROFIT_PCT")
            .unwrap_or_else(|| "15.0".to_string())
            .parse::<f64>()
            .unwrap_or(15.0);

        let grid_max_levels = lookup("GRID_MAX_LEVELS")
            .unwrap_or_else(|| "5".to_string())
            .parse::<u32>()
            .unwrap_or(5);

        let max_parallel_positions = lookup("MAX_PARALLEL_POSITIONS")
            .unwrap_or_else(|| "1".to_string())
            .parse::<usize>()
            .unwrap_or(1);

        let moonbag_retain_pct = lookup("MOONBAG_RETAIN_PCT")
            .unwrap_or_else(|| "0".to_string())
            .parse::<f64>()
            .unwrap_or(0.0);

        let min_whale_rating = lookup("MIN_WHALE_RATING")
            .unwrap_or_else(|| "0".to_string())
            .parse::<u32>()
            .unwrap_or(0);

        // Limit price in USDC (optional)
        let limit_price_usdc = lookup("LIMIT_PRICE_USDC").and_then(|s| s.parse::<f64>().ok());

        // Target buy/sell prices for Grid strategy
        let sell_sol_target_price =
            lookup("SELL_SOL_TARGET_PRICE").and_then(|s| s.parse::<f64>().ok());
        let buy_sol_target_price =
            lookup("BUY_SOL_TARGET_PRICE").and_then(|s| s.parse::<f64>().ok());

        // Webhook configuration
        let webhook_port = lookup("WEBHOOK_PORT")
            .unwrap_or_else(|| "3000".to_string())
            .parse::<u16>()
            .unwrap_or(3000);

        let webhook_auth_token = lookup("WEBHOOK_AUTH_TOKEN");
        let jito_tip_lamports = lookup("JITO_TIP_LAMPORTS")
            .unwrap_or_else(|| "100000".to_string())
            .parse::<u64>()
            .unwrap_or(100000);
        let coingecko_api_key = lookup("COINGECKO_API_KEY");

        // --- New configurable safety parameters (H4, L2) ---
        let max_price_impact_pct = lookup("MAX_PRICE_IMPACT_PCT")
            .unwrap_or_else(|| "10.0".to_string())
            .parse::<f64>()
            .unwrap_or(10.0);
        let dust_threshold_base_units = lookup("DUST_THRESHOLD_BASE_UNITS")
            .unwrap_or_else(|| "50000".to_string())
            .parse::<u64>()
            .unwrap_or(50000);

        println!("Loaded Bot Configuration:");
        println!("  Wallet Public Key: {}", keypair.pubkey());
        println!("  Input Mint:        {}", input_mint);
        println!("  Output Mint:       {}", output_mint);
        println!("  Slippage:          {} BPS", slippage_bps);
        println!("  Check Interval:    {}s", check_interval_secs);
        println!("  Dry Run Mode:      {}", dry_run);
        println!("  Max Hold Time:     {} minutes", max_hold_time_minutes);
        println!("  Min Liquidity:     ${:.1} USD", min_liquidity);
        println!("  Max Top-10 Holder: {:.1}%", max_top_10_holder_rate * 100.0);
        println!("  Max Dev Team Hold: {:.1}%", max_dev_team_hold_rate * 100.0);
        println!("  Max Creator Hold:  {:.1}%", max_creator_balance_rate * 100.0);
        println!("  Max Insider Hold:  {:.1}%", max_suspected_insider_rate * 100.0);
        println!("  Require Socials:   {}", require_socials);
        println!("  Min Smart Degens:  {}", min_smart_degen_count);
        println!("  Max Rat Trader:    {:.1}%", max_rat_trader_rate * 100.0);
        println!("  Scan Progress:     {:.1}% - {:.1}%", min_progress_pct, max_progress_pct);
        println!("  Take Profit:       +{:.1}%", take_profit_pct);
        println!("  Stop Loss:         {:.1}%", stop_loss_pct);
        println!("  Breakeven Trigger: +{:.1}%", breakeven_trigger_pct);
        println!("  Breakeven SL:      +{:.1}%", breakeven_stop_loss_pct);
        println!("  Strategy:          {:?}", strategy);
        println!("  Grid Sizing (SOL): {:.4} SOL", grid_trade_size_sol as f64 / 1_000_000_000.0);
        println!("  Grid Spacing:      {:.1}%", grid_spacing_pct);
        println!("  Grid Profit:       {:.1}%", grid_profit_pct);
        println!("  Grid Max Levels:   {}", grid_max_levels);
        println!("  Jito Tip Lamports: {}", jito_tip_lamports);
        println!("  Max Price Impact:  {:.1}%", max_price_impact_pct);
        println!("  Dust Threshold:    {} base units", dust_threshold_base_units);

        if let Some(price) = limit_price_usdc {
            println!("  Limit Price:       {} USDC", price);
        }
        if let Some(sell) = sell_sol_target_price {
            println!("  Sell SOL Target:   ${:.2} USDC", sell);
        }
        if let Some(buy) = buy_sol_target_price {
            println!("  Buy SOL Target:    ${:.2} USDC", buy);
        }
        println!("  Trade Size (SOL):  {:.4} SOL", trade_amount_sol as f64 / 1_000_000_000.0);
        println!("  Trade Size (USDC): {:.2} USDC", trade_amount_usdc as f64 / 1_000_000.0);
        println!("  Max Parallel Trades: {}", max_parallel_positions);
        println!("  Moonbag Retain:    {}%", moonbag_retain_pct);
        println!("  Min Whale Rating:  {} points", min_whale_rating);
        println!("  Migration Cooldown: {}s", migration_cooldown_secs);
        println!("  Min v_delta Entry:  {}", min_v_delta_entry);
        println!("  Min LP/MC Entry:   {:.1}%", min_lp_mc_ratio_entry * 100.0);
        println!("  Min LP/MC Exit:    {:.1}%", min_lp_mc_ratio_exit * 100.0);

        Ok(Self {
            helius_api_key,
            keypair: Arc::new(keypair),
            input_mint,
            output_mint,
            trade_amount,
            slippage_bps,
            check_interval_secs,
            dry_run,
            limit_price_usdc,
            sell_sol_target_price,
            buy_sol_target_price,
            trade_amount_sol,
            trade_amount_usdc,
            max_hold_time_minutes,
            min_liquidity,
            max_top_10_holder_rate,
            max_dev_team_hold_rate,
            max_creator_balance_rate,
            max_suspected_insider_rate,
            require_socials,
            min_smart_degen_count,
            max_rat_trader_rate,
            min_progress_pct,
            max_progress_pct,
            take_profit_pct,
            stop_loss_pct,
            breakeven_trigger_pct,
            breakeven_stop_loss_pct,
            strategy,
            grid_trade_size_sol,
            grid_spacing_pct,
            grid_profit_pct,
            grid_max_levels,
            webhook_port,
            webhook_auth_token,
            jito_tip_lamports,
            max_parallel_positions,
            moonbag_retain_pct,
            min_whale_rating,
            migration_cooldown_secs,
            min_v_delta_entry,
            min_lp_mc_ratio_entry,
            min_lp_mc_ratio_exit,
            coingecko_api_key,
            max_price_impact_pct,
            dust_threshold_base_units,
        })
    }

    /// Reload configuration at runtime **without** mutating the process
    /// environment (H1).
    ///
    /// Parses `.env` into an in-memory `HashMap` and feeds it to
    /// [`load_from_env_with`].  No `std::env::set_var` is used, so this is
    /// safe to call from any thread.  Errors (malformed `.env`, missing
    /// critical vars) are returned as `Err` instead of panicking.
    pub fn reload_from_env() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut overrides = HashMap::new();
        if let Ok(iter) = dotenvy::from_filename_iter(".env") {
            for item in iter {
                if let Ok((key, value)) = item {
                    overrides.insert(key, value);
                }
            }
        }
        Self::load_from_env_with(&overrides).map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            e.into()
        })
    }
}

fn parse_keypair(s: &str) -> Option<Keypair> {
    // 1. Try to parse as JSON byte array (e.g., [12,34,56...])
    if s.trim().starts_with('[') && s.trim().ends_with(']') {
        if let Ok(bytes) = serde_json::from_str::<Vec<u8>>(s) {
            if let Ok(keypair) = Keypair::try_from(bytes.as_slice()) {
                return Some(keypair);
            }
        }
    }

    // 2. Try to parse as Base58 string
    if let Ok(bytes) = bs58::decode(s).into_vec() {
        if let Ok(keypair) = Keypair::try_from(bytes.as_slice()) {
            return Some(keypair);
        }
    }

    None
}

impl Clone for BotConfig {
    fn clone(&self) -> Self {
        // Clone the Arc — cheap refcount bump, no key-material copy and no
        // panic-prone keypair reconstruction (H2, L5).
        Self {
            helius_api_key: self.helius_api_key.clone(),
            keypair: Arc::clone(&self.keypair),
            input_mint: self.input_mint,
            output_mint: self.output_mint,
            trade_amount: self.trade_amount,
            slippage_bps: self.slippage_bps,
            check_interval_secs: self.check_interval_secs,
            dry_run: self.dry_run,
            limit_price_usdc: self.limit_price_usdc,
            sell_sol_target_price: self.sell_sol_target_price,
            buy_sol_target_price: self.buy_sol_target_price,
            trade_amount_sol: self.trade_amount_sol,
            trade_amount_usdc: self.trade_amount_usdc,
            max_hold_time_minutes: self.max_hold_time_minutes,
            min_liquidity: self.min_liquidity,
            max_top_10_holder_rate: self.max_top_10_holder_rate,
            max_dev_team_hold_rate: self.max_dev_team_hold_rate,
            max_creator_balance_rate: self.max_creator_balance_rate,
            max_suspected_insider_rate: self.max_suspected_insider_rate,
            require_socials: self.require_socials,
            min_smart_degen_count: self.min_smart_degen_count,
            max_rat_trader_rate: self.max_rat_trader_rate,
            min_progress_pct: self.min_progress_pct,
            max_progress_pct: self.max_progress_pct,
            take_profit_pct: self.take_profit_pct,
            stop_loss_pct: self.stop_loss_pct,
            breakeven_trigger_pct: self.breakeven_trigger_pct,
            breakeven_stop_loss_pct: self.breakeven_stop_loss_pct,
            strategy: self.strategy,
            grid_trade_size_sol: self.grid_trade_size_sol,
            grid_spacing_pct: self.grid_spacing_pct,
            grid_profit_pct: self.grid_profit_pct,
            grid_max_levels: self.grid_max_levels,
            webhook_port: self.webhook_port,
            webhook_auth_token: self.webhook_auth_token.clone(),
            jito_tip_lamports: self.jito_tip_lamports,
            max_parallel_positions: self.max_parallel_positions,
            moonbag_retain_pct: self.moonbag_retain_pct,
            min_whale_rating: self.min_whale_rating,
            migration_cooldown_secs: self.migration_cooldown_secs,
            min_v_delta_entry: self.min_v_delta_entry,
            min_lp_mc_ratio_entry: self.min_lp_mc_ratio_entry,
            min_lp_mc_ratio_exit: self.min_lp_mc_ratio_exit,
            coingecko_api_key: self.coingecko_api_key.clone(),
            max_price_impact_pct: self.max_price_impact_pct,
            dust_threshold_base_units: self.dust_threshold_base_units,
        }
    }
}
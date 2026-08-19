use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Instant, Duration};
use tokio::sync::RwLock;
use serde_json::Value;
use std::error::Error;
use std::process::Command;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use crate::validation;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SwapEvent {
    pub timestamp_ms: u64,
    pub sol_amount: f64,
    pub is_buy: bool,
}

#[derive(Clone, Debug)]
pub struct TokenMetrics {
    pub swaps: VecDeque<SwapEvent>,
    pub last_update: Instant,
}

pub struct LocalIsolateTracker {
    pub metrics: Arc<RwLock<HashMap<String, TokenMetrics>>>,
}

impl LocalIsolateTracker {
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Record a swap event in-memory for a token and prune older swaps
    pub async fn record_swap(&self, token_address: &str, sol_amount: f64, is_buy: bool) {
        let mut map = self.metrics.write().await;
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;

        let token_metrics = map.entry(token_address.to_string()).or_insert_with(|| TokenMetrics {
            swaps: VecDeque::new(),
            last_update: Instant::now(),
        });

        token_metrics.swaps.push_back(SwapEvent {
            timestamp_ms: now_ms,
            sol_amount,
            is_buy,
        });

        token_metrics.last_update = Instant::now();

        // Keep last 3 minutes (180,000 ms) of history
        let cutoff = now_ms.saturating_sub(180_000);
        while let Some(front) = token_metrics.swaps.front() {
            if front.timestamp_ms < cutoff {
                token_metrics.swaps.pop_front();
            } else {
                break;
            }
        }
    }

    /// Calculate v_delta over the last 60 seconds compared to the 60s before that
    pub async fn get_v_delta(&self, token_address: &str) -> f64 {
        let map = self.metrics.read().await;
        let now_ms = chrono::Utc::now().timestamp_millis() as u64;

        if let Some(metrics) = map.get(token_address) {
            let current_window_start = now_ms.saturating_sub(60_000);
            let previous_window_start = now_ms.saturating_sub(120_000);

            let mut v_current = 0.0;
            let mut v_previous = 0.0;

            for swap in &metrics.swaps {
                if swap.timestamp_ms >= current_window_start {
                    // Current 60s volume
                    v_current += swap.sol_amount;
                } else if swap.timestamp_ms >= previous_window_start {
                    // Previous 60s volume
                    v_previous += swap.sol_amount;
                }
            }

            v_current - v_previous
        } else {
            0.0
        }
    }

    /// Prune old tokens from tracker to avoid memory growth
    pub async fn prune_inactive_tokens(&self) {
        let mut map = self.metrics.write().await;
        let now = Instant::now();
        
        // Remove tokens that have had no swaps for 10 minutes
        map.retain(|_, v| now.duration_since(v.last_update) < Duration::from_secs(600));
    }
}

/// Helper to query GMGN API using local gmgn-cli and extract liquidity / market cap parameters
///
/// H5: validates the token address as a 32-byte base58 pubkey before
/// invoking the subprocess.
pub fn get_gmgn_token_info(token_address: &str) -> Result<(f64, f64, String, String), Box<dyn Error + Send + Sync>> {
    if !validation::is_valid_solana_pubkey(token_address) {
        return Err(format!("invalid token address passed to get_gmgn_token_info: {}", token_address).into());
    }
    let output = Command::new("gmgn-cli")
        .args(["token", "info", "--address", token_address, "--chain", "sol", "--raw"])
        .output()?;
        
    if !output.status.success() {
        return Err(format!("gmgn-cli failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    
    let res_json: Value = serde_json::from_slice(&output.stdout)?;
    
    let circulating_supply = res_json["circulating_supply"]
        .as_str()
        .unwrap_or("0")
        .parse::<f64>()
        .unwrap_or(0.0);
        
    let price = res_json["price"]["price"]
        .as_str()
        .unwrap_or("0")
        .parse::<f64>()
        .unwrap_or(0.0);
        
    let liquidity = res_json["liquidity"]
        .as_str()
        .unwrap_or("0")
        .parse::<f64>()
        .unwrap_or(0.0);
        
    let market_cap = circulating_supply * price;
    let lp_to_mc_ratio = if market_cap > 0.0 { liquidity / market_cap } else { 0.0 };
    
    let symbol = res_json["symbol"].as_str().unwrap_or("UNKNOWN").to_string();
    let name = res_json["name"].as_str().unwrap_or("UNKNOWN").to_string();
    
    Ok((market_cap, lp_to_mc_ratio, symbol, name))
}

/// L4: Typed representation of the SPL Token `COption<Pubkey>` authority
/// field as stored on-chain.  Replaces the previous fragile raw byte-offset
/// reads (`data[0..4]`, `data[46..50]`) with a named struct so the layout is
/// explicit.  The full `Mint` layout is 82 bytes: mint_authority (36),
/// supply (8), decimals (1), is_initialized (1), freeze_authority (36).
#[repr(C)]
struct COptionPubkey {
    option_tag: [u8; 4],
    pubkey: [u8; 32],
}

impl COptionPubkey {
    fn is_none(&self) -> bool {
        self.option_tag == [0, 0, 0, 0]
    }
}

/// Check if mint and freeze authorities are renounced on-chain via client.
///
/// L4: parses the account data through the typed [`MintAccount`] struct
/// instead of scattered raw byte offsets, making the field layout explicit.
pub fn is_mint_renounced_on_chain(
    connection: &solana_client::rpc_client::RpcClient,
    token_address: &str,
) -> bool {
    let pubkey = match Pubkey::from_str(token_address) {
        Ok(pk) => pk,
        Err(_) => return false,
    };
    
    match connection.get_account(&pubkey) {
        Ok(account) => {
            // Verify the owner is either standard SPL Token or SPL Token-2022 program
            let owner_str = account.owner.to_string();
            if owner_str != "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" 
                && owner_str != "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb" 
            {
                return false;
            }

            if account.data.len() >= 82 {
                // L4: parse through the typed [`MintAccount`] layout so the field
                // offsets are explicit and named rather than scattered magic
                // numbers.  The base mint layout is exactly 82 bytes; any extra
                // bytes are Token-2022 extensions which do not move the
                // authority offsets.
                let bytes = &account.data[..82];
                let mint_authority = COptionPubkey {
                    option_tag: [bytes[0], bytes[1], bytes[2], bytes[3]],
                    pubkey: [
                        bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9],
                        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
                        bytes[16], bytes[17], bytes[18], bytes[19], bytes[20], bytes[21],
                        bytes[22], bytes[23], bytes[24], bytes[25], bytes[26], bytes[27],
                        bytes[28], bytes[29], bytes[30], bytes[31], bytes[32], bytes[33],
                        bytes[34], bytes[35],
                    ],
                };
                let freeze_authority = COptionPubkey {
                    option_tag: [bytes[46], bytes[47], bytes[48], bytes[49]],
                    pubkey: [
                        bytes[50], bytes[51], bytes[52], bytes[53], bytes[54], bytes[55],
                        bytes[56], bytes[57], bytes[58], bytes[59], bytes[60], bytes[61],
                        bytes[62], bytes[63], bytes[64], bytes[65], bytes[66], bytes[67],
                        bytes[68], bytes[69], bytes[70], bytes[71], bytes[72], bytes[73],
                        bytes[74], bytes[75], bytes[76], bytes[77], bytes[78], bytes[79],
                        bytes[80], bytes[81],
                    ],
                };
                mint_authority.is_none() && freeze_authority.is_none()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

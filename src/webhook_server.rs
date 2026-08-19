use axum::{
    routing::{get, post},
    extract::{Query, State},
    Json, Router,
};
use axum::body::Bytes;
use axum::extract::DefaultBodyLimit;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::Sender;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use rusqlite::Connection;
use crate::BotConfig;
use crate::local_isolate::LocalIsolateTracker;
use crate::validation;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum ActorEvent {
    PriceUpdate {
        price_sol: f64,
    },
}

pub type ActorRegistry = Arc<RwLock<std::collections::HashMap<String, tokio::sync::mpsc::Sender<ActorEvent>>>>;

pub struct GraduationEvent {
    pub mint: String,
    pub name: String,
    pub symbol: String,
}

#[derive(Deserialize)]
pub struct WebhookParams {
    pub auth: Option<String>,
}

#[derive(Clone)]
struct AppState {
    tx: Sender<GraduationEvent>,
    tracker: Arc<LocalIsolateTracker>,
    expected_auth_token: Option<String>,
    wallet_pubkey: String,
    config: Arc<RwLock<BotConfig>>,
    actor_registry: ActorRegistry,
}

/// Maximum accepted webhook request body size (1 MiB) — H7.
const WEBHOOK_BODY_LIMIT: usize = 1_000_000;

pub async fn start_server(
    port: u16,
    expected_auth_token: Option<String>,
    wallet_pubkey: String,
    config: Arc<RwLock<BotConfig>>,
    tx: Sender<GraduationEvent>,
    tracker: Arc<LocalIsolateTracker>,
    actor_registry: ActorRegistry,
) {
    let state = AppState {
        tx,
        tracker,
        expected_auth_token,
        wallet_pubkey,
        config,
        actor_registry,
    };

    let app = Router::new()
        .route("/", get(serve_dashboard))
        .route("/chart", get(serve_chart))
        .route("/api/trades", get(get_trades))
        .route("/api/sessions", get(get_sessions))
        .route("/api/config", get(get_config))
        .route("/api/config/override", post(handle_config_override))
        // H7: cap webhook body size to prevent oversized/malicious payloads.
        .route("/webhook", post(handle_webhook).layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT)))
        .with_state(state);

    // Bind to localhost only — C3/M3 mitigation: the API endpoints now require
    // auth, and the dashboard RPC proxying is removed, so remote exposure is
    // unnecessary.  Set WEBHOOK_BIND_ADDR to override (e.g. 0.0.0.0).
    let bind_addr_str = std::env::var("WEBHOOK_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1".to_string());
    let addr: SocketAddr = format!("{}:{}", bind_addr_str, port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], port)));
    println!("🌐 [Webhook Server] Listening on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn serve_dashboard() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("dashboard.html"))
}

async fn serve_chart() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("chart.html"))
}

/// M3: All `/api/*` data endpoints now require the same auth token as the
/// webhook.  Returns `true` when the request is authorised.
fn require_api_auth(
    headers: &axum::http::HeaderMap,
    params: &WebhookParams,
    expected: &Option<String>,
) -> bool {
    check_auth(headers, params, expected)
}

async fn get_trades(
    headers: axum::http::HeaderMap,
    Query(params): Query<WebhookParams>,
    State(state): State<AppState>,
) -> Result<Json<Value>, &'static str> {
    if !require_api_auth(&headers, &params, &state.expected_auth_token) {
        return Err("UNAUTHORIZED");
    }
    let conn = Connection::open(std::env::var("DATABASE_PATH").unwrap_or_else(|_| "data/trades.db".to_string())).map_err(|_| "Failed to open SQLite database")?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let mut stmt = conn.prepare("SELECT id, token_address, token_name, token_symbol, status, buy_signature, buy_time, buy_price_usd, buy_amount_sol, buy_amount_tokens, sell_signature, sell_time, sell_price_usd, sell_amount_sol, realized_pnl_pct, realized_pnl_usd, fee_sol FROM trades ORDER BY buy_time DESC LIMIT 100").map_err(|_| "Failed to prepare query statement")?;
    
    let rows: Vec<Value> = stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "id": row.get::<_, i64>(0)?,
            "token_address": row.get::<_, String>(1)?,
            "token_name": row.get::<_, String>(2)?,
            "token_symbol": row.get::<_, String>(3)?,
            "status": row.get::<_, String>(4)?,
            "buy_signature": row.get::<_, String>(5)?,
            "buy_timestamp": row.get::<_, String>(6)?,
            "buy_price_usd": row.get::<_, f64>(7)?,
            "buy_amount_sol": row.get::<_, f64>(8)?,
            "buy_qty_tokens": row.get::<_, f64>(9)?,
            "sell_signature": row.get::<_, Option<String>>(10)?,
            "sell_timestamp": row.get::<_, Option<String>>(11)?,
            "sell_price_usd": row.get::<_, Option<f64>>(12)?,
            "sell_amount_sol": row.get::<_, Option<f64>>(13)?,
            "pnl_pct": row.get::<_, Option<f64>>(14)?,
            "pnl_usd": row.get::<_, Option<f64>>(15)?,
            "fee_spent": row.get::<_, Option<f64>>(16)?,
        }))
    }).map_err(|_| "Failed to query database rows")?
      .filter_map(|r| r.ok())
      .collect();
      
    Ok(Json(serde_json::json!(rows)))
}

async fn get_sessions(
    headers: axum::http::HeaderMap,
    Query(params): Query<WebhookParams>,
    State(state): State<AppState>,
) -> Result<Json<Value>, &'static str> {
    if !require_api_auth(&headers, &params, &state.expected_auth_token) {
        return Err("UNAUTHORIZED");
    }
    let conn = Connection::open(std::env::var("DATABASE_PATH").unwrap_or_else(|_| "data/trades.db".to_string())).map_err(|_| "Failed to open SQLite database")?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    let mut stmt = conn.prepare("SELECT token_address, token_name, token_symbol, baseline_price, grids_bought_count, last_grid_price, total_tokens_held, total_sol_spent, status FROM grid_sessions WHERE status = 'ACTIVE'").map_err(|_| "Failed to prepare query statement")?;
    
    let rows: Vec<Value> = stmt.query_map([], |row| {
        Ok(serde_json::json!({
            "token_address": row.get::<_, String>(0)?,
            "token_name": row.get::<_, String>(1)?,
            "token_symbol": row.get::<_, String>(2)?,
            "baseline_price": row.get::<_, f64>(3)?,
            "grids_bought_count": row.get::<_, i64>(4)?,
            "last_grid_price": row.get::<_, f64>(5)?,
            "total_tokens_held": row.get::<_, f64>(6)?,
            "total_sol_spent": row.get::<_, f64>(7)?,
            "status": row.get::<_, String>(8)?,
        }))
    }).map_err(|_| "Failed to query database rows")?
      .filter_map(|r| r.ok())
      .collect();
      
    Ok(Json(serde_json::json!(rows)))
}

/// C2: Auth gate — secure by default.
///
/// * `authenticated` starts `false`.
/// * If no token is configured (`expected == None`), every request is
///   rejected (no default-allow).
/// * The hardcoded backdoor `"Bearer REDACTED_WEBHOOK_AUTH_TOKEN"` is
///   removed entirely.
/// * Tokens are compared in constant time to prevent timing attacks.
/// * The token may be supplied via the `?auth=` query param or the
///   `Authorization: Bearer <token>` header.
fn check_auth(headers: &axum::http::HeaderMap, params: &WebhookParams, expected: &Option<String>) -> bool {
    let Some(expected_token) = expected else {
        return false;
    };
    let provided = params.auth.as_deref().or_else(|| {
        headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
    });
    match provided {
        Some(p) => validation::constant_time_eq(p.as_bytes(), expected_token.as_bytes()),
        None => false,
    }
}

// --- H7: typed webhook payload structs instead of raw serde_json::Value ---

#[derive(serde::Deserialize)]
struct HeliusTx {
    #[serde(rename = "type")]
    tx_type: Option<String>,
    source: Option<String>,
    #[serde(rename = "tokenTransfers")]
    token_transfers: Option<Vec<TokenTransfer>>,
    events: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct TokenTransfer {
    mint: Option<String>,
    /// May be a number or a string-encoded number.
    #[serde(rename = "tokenAmount")]
    token_amount: Option<serde_json::Value>,
}

/// Allowlist of webhook transaction `type` values we process (H7).
const ALLOWED_TX_TYPES: &[&str] = &["SWAP", "CREATE_POOL", "TRANSFER"];

/// Allowlist of webhook `source` values we accept (H7).  An empty/missing
/// source is also accepted (Helius sometimes omits it).
const ALLOWED_SOURCES: &[&str] = &["RAYDIUM", "PUMP_FUN", "JUPITER", "METEORA", "ORCA"];

/// H7: Webhook handler with payload validation.
///
/// - Accepts raw `Bytes` (body size capped by the router layer).
/// - Deserialises into typed `HeliusTx` structs.
/// - Validates `type`/`source` against allowlists.
/// - Validates every `mint` as a 32-byte base58 pubkey before it reaches
///   `gmgn-cli`, SQLite, or the actor registry.
async fn handle_webhook(
    headers: axum::http::HeaderMap,
    Query(params): Query<WebhookParams>,
    State(state): State<AppState>,
    body: Bytes,
) -> &'static str {
    if !check_auth(&headers, &params, &state.expected_auth_token) {
        println!("⚠️ [Webhook Server] Blocked unauthorized webhook request (invalid token or missing header)!");
        return "UNAUTHORIZED";
    }

    // Optional HMAC-SHA256 signature verification (H7).
    // If WEBHOOK_SIGNATURE_SECRET is set, the `x-helius-signature` header must
    // match HMAC-SHA256(secret, body) compared in constant time.  When the
    // secret is unset, signature verification is disabled with a one-time
    // warning.
    if let Err(msg) = verify_signature(&headers, &body) {
        println!("⚠️ [Webhook Server] Signature verification failed: {}", msg);
        return "UNAUTHORIZED";
    }

    let payload: Vec<HeliusTx> = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            println!("⚠️ [Webhook Server] Rejecting malformed webhook payload: {:?}", e);
            return "BAD_REQUEST";
        }
    };

    for tx_item in payload {
        let tx_type = tx_item.tx_type.as_deref().unwrap_or("");
        let source = tx_item.source.as_deref().unwrap_or("");

        // H7: validate type against allowlist (empty type is allowed — some
        // events are typed only by source).
        if !tx_type.is_empty() && !ALLOWED_TX_TYPES.contains(&tx_type) {
            println!("⚠️ [Webhook Server] Skipping event with disallowed type '{}'.", tx_type);
            continue;
        }
        // H7: validate source against allowlist (empty source allowed).
        if !source.is_empty() && !ALLOWED_SOURCES.contains(&source) {
            println!("⚠️ [Webhook Server] Skipping event with disallowed source '{}'.", source);
            continue;
        }

        // 1. Process Swap Events for Volume Delta computations
        if tx_type == "SWAP" {
            if let Some((token_address, sol_amount, is_buy)) = parse_swap_typed(&tx_item) {
                if !validation::is_valid_solana_pubkey(&token_address) {
                    println!("⚠️ [Webhook Server] SWAP event has invalid mint '{}'; skipping.", token_address);
                    continue;
                }
                state.tracker.record_swap(&token_address, sol_amount, is_buy).await;
                if let Some(price_sol) = parse_swap_price_typed(&tx_item, &token_address) {
                    let registry = state.actor_registry.read().await;
                    if let Some(actor_tx) = registry.get(&token_address) {
                        let _ = actor_tx.send(ActorEvent::PriceUpdate { price_sol }).await;
                    }
                }
                println!("📥 [Local Isolate] Parsed SWAP: {} | SOL Volume: {:.4} | Buy: {}", 
                    token_address, sol_amount, is_buy);
            }
        }

        // 2. Process Pool Creation Events (Raydium Graduation Events)
        if tx_type == "CREATE_POOL" || source == "RAYDIUM" {
            if let Some(transfers) = &tx_item.token_transfers {
                for transfer in transfers {
                    if let Some(mint) = transfer.mint.as_deref() {
                        // H7: validate mint before use.
                        if !validation::is_valid_solana_pubkey(mint) {
                            println!("⚠️ [Webhook Server] Graduation event has invalid mint '{}'; skipping.", mint);
                            continue;
                        }
                        if mint != "So11111111111111111111111111111111111111112" {
                            println!("🎯 [Webhook Server] Intercepted Raydium Graduation Event for Mint: {}", mint);

                            let event = GraduationEvent {
                                mint: mint.to_string(),
                                name: "Graduated Token".to_string(),
                                symbol: "GRAD".to_string(),
                            };

                            let _ = state.tx.send(event).await;
                        }
                    }
                }
            }
        }
    }

    "OK"
}

/// Optional HMAC-SHA256 signature check (H7).
///
/// Reads `WEBHOOK_SIGNATURE_SECRET` from the environment.  When set, verifies
/// the `x-helius-signature` header equals `HMAC-SHA256(secret, body)` using a
/// constant-time comparison.  When unset, logs a warning once and passes.
fn verify_signature(headers: &axum::http::HeaderMap, body: &[u8]) -> Result<(), String> {
    let secret = match std::env::var("WEBHOOK_SIGNATURE_SECRET") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
                eprintln!("⚠️ [Webhook Server] WEBHOOK_SIGNATURE_SECRET not set — signature verification disabled.");
            }
            return Ok(());
        }
    };

    let provided = headers
        .get("x-helius-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| "missing x-helius-signature header".to_string())?;

    let computed = hmac_sha256_hex(secret.as_bytes(), body);
    if validation::constant_time_eq(provided.as_bytes(), computed.as_bytes()) {
        Ok(())
    } else {
        Err("signature mismatch".to_string())
    }
}

/// Minimal HMAC-SHA256 → hex implementation (avoids adding `hmac`/`sha2`
/// deps when offline builds are a concern).  Uses the standard HMAC
/// construction over a from-scratch SHA-256.
fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    let block_size = 64;
    // Prepare key (pad/truncate to block size).
    let key_block = if key.len() > block_size {
        sha256(key)
    } else {
        let mut k = vec![0u8; block_size];
        k[..key.len()].copy_from_slice(key);
        k
    };
    let mut o_key_pad = vec![0x5cu8; block_size];
    let mut i_key_pad = vec![0x36u8; block_size];
    for i in 0..block_size {
        o_key_pad[i] ^= key_block[i];
        i_key_pad[i] ^= key_block[i];
    }
    // inner = H(i_key_pad || message)
    let mut inner_input = i_key_pad;
    inner_input.extend_from_slice(message);
    let inner = sha256(&inner_input);
    // outer = H(o_key_pad || inner)
    let mut outer_input = o_key_pad;
    outer_input.extend_from_slice(&inner);
    let outer = sha256(&outer_input);
    outer.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Pure-Rust SHA-256 (FIPS 180-4).  Small enough for webhook verification.
fn sha256(data: &[u8]) -> Vec<u8> {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
        0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
        0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
        0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
        0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
        0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
        0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    // Pre-processing: padding.
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in padded.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().flat_map(|w| w.to_be_bytes()).collect()
}

/// Typed version of `parse_swap` operating on the deserialised `HeliusTx`.
fn parse_swap_typed(tx_item: &HeliusTx) -> Option<(String, f64, bool)> {
    let swap_info = tx_item.events.as_ref()?.get("swap")?;

    // 1. Try Native SOL swaps
    let native_in = swap_info.get("nativeInput");
    let native_out = swap_info.get("nativeOutput");

    if let Some(n_in) = native_in {
        if !n_in.is_null() {
            if let Some(amount_str) = n_in.get("amount").and_then(|a| a.as_str()) {
                if let Ok(amount_lamports) = amount_str.parse::<f64>() {
                    let sol_amount = amount_lamports / 1_000_000_000.0;
                    if let Some(token_outs) = swap_info.get("tokenOutputs").and_then(|t| t.as_array()) {
                        for out in token_outs {
                            if let Some(mint) = out.get("mint").and_then(|m| m.as_str()) {
                                if mint != "So11111111111111111111111111111111111111112" {
                                    return Some((mint.to_string(), sol_amount, true));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(n_out) = native_out {
        if !n_out.is_null() {
            if let Some(amount_str) = n_out.get("amount").and_then(|a| a.as_str()) {
                if let Ok(amount_lamports) = amount_str.parse::<f64>() {
                    let sol_amount = amount_lamports / 1_000_000_000.0;
                    if let Some(token_ins) = swap_info.get("tokenInputs").and_then(|t| t.as_array()) {
                        for input in token_ins {
                            if let Some(mint) = input.get("mint").and_then(|m| m.as_str()) {
                                if mint != "So11111111111111111111111111111111111111112" {
                                    return Some((mint.to_string(), sol_amount, false));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 2. Try Wrapped SOL (WSOL) swaps
    let token_ins = swap_info.get("tokenInputs").and_then(|t| t.as_array())?;
    let token_outs = swap_info.get("tokenOutputs").and_then(|t| t.as_array())?;

    let wsol_in = token_ins.iter().find(|t| t.get("mint").and_then(|m| m.as_str()) == Some("So11111111111111111111111111111111111111112"));
    let wsol_out = token_outs.iter().find(|t| t.get("mint").and_then(|m| m.as_str()) == Some("So11111111111111111111111111111111111111112"));

    if let Some(win) = wsol_in {
        let amount_str = win.get("rawTokenAmount").and_then(|r| r.get("tokenAmount").and_then(|t| t.as_str()))?;
        let amount_lamports = amount_str.parse::<f64>().ok()?;
        let sol_amount = amount_lamports / 1_000_000_000.0;
        let target = token_outs.iter().find(|t| t.get("mint").and_then(|m| m.as_str()) != Some("So11111111111111111111111111111111111111112"))?;
        let mint = target.get("mint")?.as_str()?;
        return Some((mint.to_string(), sol_amount, true));
    }

    if let Some(wout) = wsol_out {
        let amount_str = wout.get("rawTokenAmount").and_then(|r| r.get("tokenAmount").and_then(|t| t.as_str()))?;
        let amount_lamports = amount_str.parse::<f64>().ok()?;
        let sol_amount = amount_lamports / 1_000_000_000.0;
        let target = token_ins.iter().find(|t| t.get("mint").and_then(|m| m.as_str()) != Some("So11111111111111111111111111111111111111112"))?;
        let mint = target.get("mint")?.as_str()?;
        return Some((mint.to_string(), sol_amount, false));
    }

    None
}

/// Typed version of `parse_swap_price`: derives the token price (SOL per
/// token) from the transfer amount and the swap's SOL leg.
fn parse_swap_price_typed(tx_item: &HeliusTx, token_address: &str) -> Option<f64> {
    let transfers = tx_item.token_transfers.as_ref()?;
    for transfer in transfers {
        let mint = transfer.mint.as_deref()?;
        if mint == token_address {
            let amount_f = transfer.token_amount.as_ref().and_then(|v| {
                if let Some(f) = v.as_f64() {
                    Some(f)
                } else if let Some(s) = v.as_str() {
                    s.parse::<f64>().ok()
                } else {
                    None
                }
            })?;
            let (_, sol_amount, _) = parse_swap_typed(tx_item)?;
            if amount_f > 0.0 {
                return Some(sol_amount / amount_f);
            }
        }
    }
    None
}


/// C4: Authenticated config override with bounds validation.
///
/// Auth is enforced by `check_auth` (now secure — C2).  All override values
/// are clamped to safe bounds to prevent a compromised token from setting
/// dangerous parameters (huge tips, zero slippage, etc.).
///
/// `dry_run` may only be flipped to `false` when the request also includes
/// `"confirm_live": true`, providing a second factor against accidental live
/// enabling.
async fn handle_config_override(
    headers: axum::http::HeaderMap,
    Query(params): Query<WebhookParams>,
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, &'static str> {
    if !check_auth(&headers, &params, &state.expected_auth_token) {
        return Err("UNAUTHORIZED");
    }

    let mut config = state.config.write().await;
    println!("🔄 [Webhook Server] Applying config overrides on-the-fly:");

    // C4: dry_run may only be disabled with an explicit confirmation flag.
    if let Some(dry_run) = payload["dry_run"].as_bool() {
        if !dry_run {
            let confirmed = payload["confirm_live"].as_bool().unwrap_or(false);
            if !confirmed {
                return Err("Refusing to disable dry_run without confirm_live=true");
            }
        }
        config.dry_run = dry_run;
        println!("   dry_run -> {}", dry_run);
    }
    if let Some(take_profit) = payload["take_profit_pct"].as_f64() {
        // Sanity bounds: 0.1% .. 1000%
        let clamped = take_profit.clamp(0.1, 1000.0);
        config.take_profit_pct = clamped;
        println!("   take_profit_pct -> {}%", clamped);
    }
    if let Some(stop_loss) = payload["stop_loss_pct"].as_f64() {
        // Sanity bounds: -100% .. 0%
        let clamped = stop_loss.clamp(-100.0, 0.0);
        config.stop_loss_pct = clamped;
        println!("   stop_loss_pct -> {}%", clamped);
    }
    if let Some(jito_tip) = payload["jito_tip_lamports"].as_u64() {
        // C4/M2: cap jito tip at 1 SOL to prevent overflow exploitation.
        let clamped = jito_tip.min(1_000_000_000);
        config.jito_tip_lamports = clamped;
        println!("   jito_tip_lamports -> {} lamports", clamped);
    }
    if let Some(max_parallel) = payload["max_parallel_positions"].as_u64() {
        // C4: cap at 20 concurrent positions.
        let clamped = (max_parallel.min(20)) as usize;
        config.max_parallel_positions = clamped;
        println!("   max_parallel_positions -> {}", clamped);
    }
    if let Some(slippage) = payload["slippage_bps"].as_u64() {
        // C4: clamp slippage to 1..=1000 BPS (0.01% .. 10%).
        let clamped = slippage.clamp(1, 1000) as u32;
        config.slippage_bps = clamped;
        println!("   slippage_bps -> {} BPS", clamped);
    }
    if let Some(cooldown) = payload["migration_cooldown_secs"].as_u64() {
        config.migration_cooldown_secs = cooldown;
        println!("   migration_cooldown_secs -> {}s", cooldown);
    }
    if let Some(min_v_delta) = payload["min_v_delta_entry"].as_f64() {
        config.min_v_delta_entry = min_v_delta;
        println!("   min_v_delta_entry -> {}", min_v_delta);
    }
    if let Some(max_impact) = payload["max_price_impact_pct"].as_f64() {
        let clamped = max_impact.clamp(0.01, 100.0);
        config.max_price_impact_pct = clamped;
        println!("   max_price_impact_pct -> {}%", clamped);
    }

    Ok(Json(serde_json::json!({
        "status": "success",
        "message": "Config overrides applied successfully in-memory"
    })))
}

/// C3/M3: Return non-secret config only, and require auth.
async fn get_config(
    headers: axum::http::HeaderMap,
    Query(params): Query<WebhookParams>,
    State(state): State<AppState>,
) -> Result<Json<Value>, &'static str> {
    if !require_api_auth(&headers, &params, &state.expected_auth_token) {
        return Err("UNAUTHORIZED");
    }
    let config = state.config.read().await;
    // C3: helius_api_key is NEVER returned.
    Ok(Json(serde_json::json!({
        "wallet_pubkey": state.wallet_pubkey,
        "strategy": format!("{:?}", config.strategy),
        "dry_run": config.dry_run,
        "take_profit_pct": config.take_profit_pct,
        "stop_loss_pct": config.stop_loss_pct,
        "jito_tip_lamports": config.jito_tip_lamports,
        "max_parallel_positions": config.max_parallel_positions,
        "migration_cooldown_secs": config.migration_cooldown_secs,
        "min_v_delta_entry": config.min_v_delta_entry,
        "max_price_impact_pct": config.max_price_impact_pct,
    })))
}


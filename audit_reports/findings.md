# Security Audit Findings — Solana Memecoin Trading Bot

**Date:** 2025-06-18  
**Scope:** All Rust source files under `src/` (16 files, ~5,326 lines), plus `src/dashboard.html`, `.env`, and `.gitignore`  
**Auditor:** Automated security review with manual verification of every `file:line` reference  

---

## Summary

| Severity | Count |
|----------|-------|
| CRITICAL | 5     |
| HIGH     | 8     |
| MEDIUM   | 6     |
| LOW      | 5     |
| **Total** | **24** |

---

## CRITICAL

### C1. Discord Webhook URL Hardcoded in Source

- **File:** `src/discord_notify.rs:8`
- **Description:** The full Discord webhook URL — including the secret signing token — is compiled into the binary as a `static &str`. Anyone with read access to the repository (or the compiled binary via `strings`) can post arbitrary messages to the Discord channel, inject misleading trade alerts, or spam the channel.
- **Code:**
  ```rust
  static DISCORD_WEBHOOK_URL: &str = "https://discord.com/api/webhooks/REDACTED_WEBHOOK_ID/REDACTED_TOKEN";
  ```
- **Recommended fix:** Move the webhook URL to an environment variable (`DISCORD_WEBHOOK_URL`), load it at startup via `env::var`, and degrade gracefully (skip notifications) if unset. Rotate the exposed webhook token immediately.

---

### C2. Webhook Auth Bypass — Hardcoded Backdoor Token + Logic Bug

- **File:** `src/webhook_server.rs:149-164`
- **Description:** The `check_auth` function has two critical flaws:
  1. **Default-allow logic bug:** `authenticated` is initialized to `true`. When `WEBHOOK_AUTH_TOKEN` is unset (i.e. `expected == None`), the `if let Some(expected_token)` block is skipped entirely, so `authenticated` remains `true`. **Any unauthenticated request is accepted.**
  2. **Hardcoded backdoor token:** Even if the expected token is set and the request fails the check, the function unconditionally sets `authenticated = true` when the `Authorization` header equals the hardcoded string `"Bearer REDACTED_WEBHOOK_AUTH_TOKEN"`. This is a universal override that bypasses any configured token.
- **Code:**
  ```rust
  fn check_auth(headers: &axum::http::HeaderMap, params: &WebhookParams, expected: &Option<String>) -> bool {
      let mut authenticated = true;          // ← defaults to true
      
      if let Some(expected_token) = expected {
          if params.auth.as_ref() != Some(expected_token) {
              authenticated = false;
          }
      }
      // ↑ skipped entirely when expected == None → stays true
      
      if let Some(auth_header) = headers.get("authorization") {
          if auth_header.to_str().unwrap_or("") == "Bearer REDACTED_WEBHOOK_AUTH_TOKEN" {
              authenticated = true;            // ← hardcoded backdoor override
          }
      }

      authenticated
  }
  ```
- **Recommended fix:** Initialize `authenticated = false`. Require `expected` to be `Some`; reject when no token is configured. Remove the hardcoded Bearer string entirely. Compare tokens using constant-time equality (e.g. `subtle::ConstantTimeEq`) to prevent timing attacks. Return `false` for all requests if `expected` is `None`.

---

### C3. Helius API Key Exposed via Unauthenticated `/api/config` Endpoint

- **File:** `src/webhook_server.rs:381` (server returns the key); `src/dashboard.html:648-673` (client uses it)
- **Description:** The `GET /api/config` endpoint returns the `helius_api_key` in plaintext JSON with no authentication. The server binds to `0.0.0.0` (`webhook_server.rs:78`), making it reachable from any network. The dashboard HTML reads the key client-side and constructs RPC URLs containing the key in the query string:
  ```javascript
  const apiKey = config.helius_api_key;
  const rpcUrl = `https://mainnet.helius-rpc.com/?api-key=${apiKey}`;
  const wssUrl = `wss://mainnet.helius-rpc.com/?api-key=${apiKey}`;
  ```
  An attacker who can reach the port can extract the Helius API key and use it for their own RPC calls, potentially exhausting rate limits or reading wallet data.
- **Code:**
  ```rust
  async fn get_config(State(state): State<AppState>) -> Json<Value> {
      let config = state.config.read().await;
      Json(serde_json::json!({
          "helius_api_key": state.helius_api_key,   // ← plaintext API key exposed
          "wallet_pubkey": state.wallet_pubkey,
          // ...
      }))
  }
  ```
- **Recommended fix:** Never return the API key in any response. Proxy all RPC calls server-side through authenticated endpoints. Require authentication on all `/api/*` routes. Bind to `127.0.0.1` unless remote access is explicitly required (and then use TLS + auth).

---

### C4. Unauthenticated Config Override Can Disable `dry_run` and Trigger Live Trades

- **File:** `src/webhook_server.rs:328-377`
- **Description:** The `POST /api/config/override` endpoint allows changing `dry_run`, `slippage_bps`, `jito_tip_lamports`, `max_parallel_positions`, and other critical trading parameters at runtime. Combined with the auth bypass (C2), an attacker who can reach the webhook port can set `dry_run: false`, increase `max_parallel_positions`, and raise `jito_tip_lamports` — then trigger graduation events via the webhook endpoint to execute live trades with the bot's funded wallet.
- **Code:**
  ```rust
  async fn handle_config_override(
      headers: axum::http::HeaderMap,
      Query(params): Query<WebhookParams>,
      State(state): State<AppState>,
      Json(payload): Json<Value>,
  ) -> Result<Json<Value>, &'static str> {
      if !check_auth(&headers, &params, &state.expected_auth_token) {
          return Err("UNAUTHORIZED");    // ← bypassed due to C2
      }
      let mut config = state.config.write().await;
      if let Some(dry_run) = payload["dry_run"].as_bool() {
          config.dry_run = dry_run;       // ← can flip to false
      }
      // ... slippage, jito_tip, max_parallel_positions ...
  }
  ```
- **Recommended fix:** Fix the auth check (C2). Restrict override to non-dangerous fields. Require explicit confirmation or a separate lock flag for `dry_run` transitions. Validate all override values against safe bounds. Consider removing the override endpoint entirely in production.

---

### C5. `.env` Tracked in Git — Secret Storage Risk

- **File:** `.env` (git-tracked); no `.gitignore` exists
- **Description:** The `.env` file is tracked by git (`git ls-files -- .env` confirms it). There is no `.gitignore` file in the repository. Currently the file only contains `OLLAMA_API_KEY=ollama`, but when production secrets (`WALLET_PRIVATE_KEY`, `HELIUS_API_KEY`, `DISCORD_WEBHOOK_URL`, `COINGECKO_API_KEY`) are added, they will be committed to git history — potentially leaking wallet private keys that control real funds.
- **Recommended fix:** Add a `.gitignore` with at minimum: `.env`, `data/`, `*.db`, `*.bak`. Run `git rm --cached .env` to untrack it. Rotate any secrets that may have been committed. Provide a `.env.example` template with placeholder values. Consider `git-secrets` or similar pre-commit hooks.

---

## HIGH

### H1. Unsafe `std::env::set_var` in Multithreaded Config Reload

- **File:** `src/config.rs:378-380`
- **Description:** `reload_from_env()` is called every loop iteration (hunter, grid, and autonomous loops) while multiple tokio tasks are running. It calls `std::env::set_var` inside an `unsafe` block to overwrite environment variables at runtime. In Rust 2024 edition, mutating the process environment while other threads may read it is undefined behavior. Even in earlier editions, this is not thread-safe and can cause data races, corrupted reads, or panics.
- **Code:**
  ```rust
  if let Ok(iter) = dotenvy::from_filename_iter(".env") {
      for item in iter {
          if let Ok((key, value)) = item {
              unsafe {
                  std::env::set_var(key, value);
              }
          }
      }
  }
  ```
- **Recommended fix:** Parse `.env` into a `HashMap<String, String>` and pass values explicitly to `load_from_env` without mutating the process environment. Never call `set_var` after startup.

---

### H2. Keypair `unwrap()` Panic in `Clone` Implementation

- **File:** `src/config.rs:417`
- **Description:** The `Clone` impl for `BotConfig` reconstructs the `Keypair` from bytes and calls `.unwrap()`. If reconstruction fails (extremely unlikely but possible with corrupted state), this panics and crashes the bot. `BotConfig::clone()` is called on every loop iteration in the autonomous strategy (`ref_config.read().await.clone()`).
- **Code:**
  ```rust
  impl Clone for BotConfig {
      fn clone(&self) -> Self {
          let keypair_bytes = self.keypair.to_bytes();
          let keypair = Keypair::try_from(&keypair_bytes[..]).unwrap();  // ← panic risk
  ```
- **Recommended fix:** Use `expect("Failed to reconstruct keypair during clone")` for better diagnostics, or better yet, wrap the keypair in `Arc<Keypair>` and clone the `Arc` (avoids re-creating the keypair entirely and eliminates the panic surface).

---

### H3. Ephemeral Keypair Fallback During Reload — Silent Wallet Switch

- **File:** `src/config.rs:67-76`
- **Description:** If `WALLET_PRIVATE_KEY` is unset or temporarily removed (e.g., during `.env` editing), `load_from_env` silently generates a new ephemeral keypair and continues. In non-dry-run mode, this means the bot would trade with a different wallet — potentially an empty one that can't cover fees, or worse, if the ephemeral wallet receives funds, the bot's real wallet loses access to them.
- **Code:**
  ```rust
  let keypair = match env::var("WALLET_PRIVATE_KEY") {
      Ok(private_key_str) => {
          parse_keypair(&private_key_str)
              .expect("Failed to parse WALLET_PRIVATE_KEY...")
      }
      Err(_) => {
          println!("⚠️  WALLET_PRIVATE_KEY not set in .env. Generating a temporary ephemeral keypair...");
          Keypair::new()  // ← silent fallback, different wallet
      }
  };
  ```
- **Recommended fix:** When `dry_run == false`, fail hard if `WALLET_PRIVATE_KEY` is missing or unparseable. Only allow ephemeral fallback when `DRY_RUN=true`.

---

### H4. No Price Impact Check Before Executing Swaps

- **File:** `src/jup_api.rs:50` (field deserialized); `src/main.rs:237` and `src/main.rs:300` (buy/sell swap execution)
- **Description:** The Jupiter `QuoteResponse` struct includes a `price_impact_pct` field, but it is never read or compared against any threshold. Both `execute_buy_swap` and `execute_sell_swap` fetch a quote and immediately proceed to build and submit the transaction without checking price impact. A quote with 50%+ price impact (indicating extremely low liquidity or a sandwich attack) would be executed blindly.
- **Code:**
  ```rust
  // jup_api.rs:50 — field exists but never validated:
  pub price_impact_pct: String,

  // main.rs execute_buy_swap — no impact check after quote:
  let quote = jup_client.get_quote(...).await?;
  // ← should check quote.price_impact_pct here
  let swap_tx_b64 = jup_client.get_swap_transaction(quote, ...).await?;
  ```
- **Recommended fix:** After fetching a quote, parse `price_impact_pct` and reject the swap if it exceeds a configurable threshold (e.g. 5-10%). Log the impact for every trade. Add a `max_price_impact_pct` config parameter.

---

### H5. `gmgn-cli` Subprocess Execution with Unsanitized Input

- **File:** `src/hunter.rs:18-30` (`run_gmgn_cli`), `src/hunter.rs:268` (`get_whale_rating`), `src/hunter.rs:296` (`audit_detailed_token_safety`), `src/local_isolate.rs:105` (`get_gmgn_token_info`)
- **Description:** The bot executes `gmgn-cli` as a subprocess, passing token addresses received from untrusted webhook payloads as command arguments. There is no timeout on the subprocess, no output size cap, no PATH/binary validation, and no validation that the token address is a valid base58 pubkey before it's passed as an argument. While Rust's `Command` API is not vulnerable to shell injection (args are passed directly), a malicious or malformed mint string from a webhook could cause the subprocess to hang (no timeout), consume memory (no output cap), or fail silently.
- **Code:**
  ```rust
  // hunter.rs:18-30
  pub fn run_gmgn_cli(args: &[&str]) -> Result<Value, Box<dyn Error + Send + Sync>> {
      let mut cmd = Command::new("gmgn-cli");
      cmd.args(args);
      // no timeout, no output cap
      let output = cmd.output()?;
      // ...
  }

  // hunter.rs:268 — token_address from webhook payload
  let output = std::process::Command::new("gmgn-cli")
      .args(["token", "info", "--address", token_address, "--chain", "sol", "--raw"])
      .output()?;
  ```
- **Recommended fix:** Validate `token_address` as a valid base58 Solana pubkey (32 bytes) before passing to subprocess. Resolve the full path to `gmgn-cli` at startup rather than relying on PATH. Add a timeout (e.g. `gtimeout` or spawn with `tokio::process::Command::timeout`). Cap stdout/stderr size. Consider using the GMGN HTTP API directly instead of a subprocess.

---

### H6. Helius API Key in URL Query Strings

- **File:** `src/main.rs:2121` (`fetch_helius_webhook_id`), `src/main.rs:2147` (`update_helius_webhook`), `src/bin/nuke_dust.rs:31`, `src/bin/sell_prop.rs:61`, `src/bin/check_prop_curve.rs:69`
- **Description:** The Helius API key is embedded in URL query parameters (`?api-key={}`). These URLs appear in HTTP request logs, proxy logs, reqwest debug output, and potentially in error messages. The API key grants access to the Helius RPC and enhanced endpoints.
- **Code:**
  ```rust
  // main.rs:2121
  let url = format!("https://api-mainnet.helius-rpc.com/v0/webhooks?api-key={}", helius_api_key);

  // nuke_dust.rs:31
  let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
  ```
- **Recommended fix:** Where the Helius API supports it, send the key via a header instead of the URL. Otherwise, ensure debug logging is disabled for reqwest and never log URLs containing the key. Use a custom reqwest middleware to redact `api-key` from any logged URLs.

---

### H7. Weak Webhook Payload Validation

- **File:** `src/webhook_server.rs:190-240`
- **Description:** The `handle_webhook` handler accepts arbitrary JSON with no schema validation, no body size limit, and no mint format validation. Mint strings extracted from the payload are passed directly to `gmgn-cli`, stored in SQLite, and used in the actor registry. A malicious webhook payload could inject arbitrary strings as mint addresses.
- **Code:**
  ```rust
  async fn handle_webhook(
      headers: axum::http::HeaderMap,
      Query(params): Query<WebhookParams>,
      State(state): State<AppState>,
      Json(payload): Json<Value>,       // ← no schema validation, no size limit
  ) -> &'static str {
      // ...
      if let Some(mint) = transfer["mint"].as_str() {
          // ← no base58/pubkey validation on mint
          // mint flows to gmgn-cli, SQLite, actor registry
      }
  ```
- **Recommended fix:** Validate every `mint` field as a valid base58 Solana pubkey (32 bytes decoded). Cap request body size (e.g. 1 MB). Validate `type` and `source` against an allowlist. Use a typed deserialization struct instead of raw `serde_json::Value`.

---

### H8. `expect()` Panics on Malformed Environment Variables

- **File:** `src/config.rs:66,72,84,90,96,102,107,113,119,125`
- **Description:** The initial `load_from_env()` at startup uses `.expect()` for critical environment variables. Any malformed value (non-integer `TRADE_AMOUNT_LAMPORTS`, invalid pubkey, etc.) causes an unrecoverable panic. While `reload_from_env()` wraps this in `catch_unwind`, the initial startup call in `main()` does not, so the bot crashes immediately.
- **Code:**
  ```rust
  let helius_api_key = env::var("HELIUS_API_KEY")
      .expect("HELIUS_API_KEY is not set in the environment or .env file");
  let trade_amount = trade_amount_str.parse::<u64>()
      .expect("Invalid TRADE_AMOUNT_LAMPORTS integer");
  // ... 9 more .expect() calls
  ```
- **Recommended fix:** Return `Result<BotConfig, String>` from `load_from_env` and propagate a clear error message to `main()`. Use `unwrap_or` with documented defaults for non-critical values, and fail with a human-readable message for critical ones.

---

## MEDIUM

### M1. No 429 Rate Limit Backoff in DexScreener and CoinGecko Clients

- **File:** `src/dexscreener.rs:138-148` (`get_token_pairs`), `src/gecko_api.rs:22-40` (`fetch_token_pool_metrics`)
- **Description:** Only `jup_api.rs` implements 429 retry logic (3 attempts with linear backoff). The DexScreener client (300 req/min limit) and CoinGecko Pro client have no retry, no backoff, and no `Retry-After` header handling. When rate-limited, they return an error immediately, which the calling code treats as a safety check failure — potentially causing the bot to skip valid tokens or abort safety audits.
- **Code:**
  ```rust
  // dexscreener.rs:138-148 — no retry
  pub async fn get_token_pairs(token_address: &str) -> Result<Vec<DexPair>, Box<dyn Error + Send + Sync>> {
      let url = format!("{}/token-pairs/v1/solana/{}", DEXSCREENER_BASE, token_address);
      let client = reqwest::Client::builder().timeout(...).build()?;
      let resp = client.get(&url).send().await?;
      if !resp.status().is_success() {
          return Err(format!("DexScreener API error: {}", resp.status()).into());
      }
      // ...
  }
  ```
- **Recommended fix:** Implement a shared retry-with-backoff helper that handles 429 responses and respects the `Retry-After` header. Apply it to DexScreener and CoinGecko clients.

---

### M2. Integer Overflow / Lossy Casts in Trade Math

- **File:** `src/main.rs:568,572,795,822,839,897,1453` (multiple `as u64` from f64), `src/config.rs:224`, `src/main.rs:1458` (`jito_tip_lamports * 3 / 2`)
- **Description:** Trade amounts are computed using f64 arithmetic and then cast to `u64` with `as u64`. If the f64 value is NaN or negative (e.g. from a division by zero or an API error), `as u64` saturates to 0, producing a zero-size sell order. The expression `config.jito_tip_lamports * 3 / 2` can overflow `u64` if the config override endpoint sets an extremely large tip value, wrapping around to a small number.
- **Code:**
  ```rust
  // main.rs:568
  sell_amount_base_units = ((config.trade_amount_sol as f64 / 1_000_000_000.0) / entry_price
      * 10f64.powi(get_decimals_by_mint(...) as i32)).round() as u64;  // ← NaN → 0

  // main.rs:1458
  let exit_tip = if is_abort {
      config.jito_tip_lamports * 3 / 2  // ← overflow if jito_tip_lamports > u64::MAX/3
  } else { ... };
  ```
- **Recommended fix:** Use `saturating_mul`/`checked_mul` for integer arithmetic. Validate f64 values with `is_finite() && value >= 0.0` before casting. Clamp `jito_tip_lamports` to a reasonable maximum in the override endpoint. Use `u128` intermediates or `checked_mul` for the `* 3 / 2` computation.

---

### M3. Unauthenticated Trade/Session Data Disclosure

- **File:** `src/webhook_server.rs:93-123` (`get_trades`), `src/webhook_server.rs:125-158` (`get_sessions`)
- **Description:** The `GET /api/trades` and `GET /api/sessions` endpoints expose full trade history — token addresses, buy/sell signatures, P&L, amounts — with no authentication. Combined with the `0.0.0.0` bind address, anyone who can reach the port can read the bot's complete trading activity, which could be used for copy-trading or competitive intelligence.
- **Code:**
  ```rust
  async fn get_trades() -> Result<Json<Value>, &'static str> {
      // ← no auth check
      let conn = Connection::open(...).map_err(...)?;
      // ... returns full trade history including signatures
  }
  ```
- **Recommended fix:** Require authentication on all `/api/*` routes. Consider returning only aggregated/summarized data rather than raw signatures.

---

### M4. Swallowed Database Errors Throughout Trade Logic

- **File:** `src/main.rs:448,525,580,591,637,783,800,804,808,826,833,844,846,851,853,880,903,1049,1052` (20+ instances)
- **Description:** The bot frequently uses `let _ = db_conn.execute(...)` or `let _ = db::finalize_sell_trade(...)` throughout the trading loops, silently discarding database errors. If a trade is sold successfully on-chain but the DB update fails, the trade remains `OPEN` in the database and may be re-sold or double-counted. Similarly, grid session updates that fail silently lead to inconsistent state.
- **Code:**
  ```rust
  // main.rs:591 — sell finalized on-chain but DB error is swallowed
  let _ = db::finalize_sell_trade(&db_conn, &candidate.address, &sig,
      candidate.price, sell_amount_sol, pnl_pct, realized_pnl_usd, 0.001);
  ```
- **Recommended fix:** Log all DB errors at minimum. For critical operations (trade finalization, session status changes), retry or halt the bot to prevent state divergence. Consider using the existing DB actor pattern (as in `run_autonomous_loop`) consistently across all strategies.

---

### M5. `std::process::exit(0)` — Abrupt Termination in Autonomous Loop

- **File:** `src/main.rs:1726`
- **Description:** When `max_parallel_positions == 0` and all active sessions are completed, the bot calls `std::process::exit(0)` immediately. This terminates the process without gracefully shutting down the DB actor, closing token accounts, or flushing any pending state. Open positions in the database may be left in an inconsistent state.
- **Code:**
  ```rust
  if config.max_parallel_positions == 0 && active_sessions_db.is_empty() {
      println!("🛑  ... Shutting down bot process...");
      std::process::exit(0);
  }
  ```
- **Recommended fix:** Return from the loop and shut down gracefully: cancel spawned tasks, flush the DB actor channel, close connections, and exit from `main()` normally.

---

### M6. Race Conditions in Parallel Trade Execution and Copycat Detection

- **File:** `src/main.rs` (hunter/grid/autonomous loop position counting), `src/hunter.rs:232-254` (`check_copycat_scam`)
- **Description:** `max_parallel_positions` is enforced by counting DB rows or local `active_count`, which can race across concurrent loop iterations and webhook events. In `check_copycat_scam`, `SYMBOL_REGISTRY` and `NAME_REGISTRY` are locked separately — the check-then-insert is not atomic across the two maps, so a token with the same symbol but different name could pass the symbol check and fail the name check, leaving the symbol registered but the token rejected (or vice versa).
- **Code:**
  ```rust
  // hunter.rs:232-254 — two separate locks, not atomic
  if let Ok(mut reg) = SYMBOL_REGISTRY.lock() {
      // check & insert symbol
  }
  if let Ok(mut reg) = NAME_REGISTRY.lock() {
      // check & insert name — not atomic with the above
  }
  ```
- **Recommended fix:** Centralize position-slot accounting behind a single `AtomicUsize` or `Semaphore`. For copycat detection, acquire both locks in a single scope (or use a single combined registry map) to make the check-then-insert atomic.

---

## LOW

### L1. Hardcoded Token Addresses and Balances in Operational Bin Scripts

- **File:** `src/bin/nuke_token.rs:38,45,51`, `src/bin/nuke_dust.rs:45-46`, `src/bin/sell_prop.rs:83`, `src/bin/check_prop_curve.rs:69`
- **Description:** Operational scripts hardcode token mint addresses, token account addresses, and balance amounts. While not secrets, these are fragile: running the wrong script with the wrong hardcoded values could burn tokens or send them to the wrong destination.
- **Code:**
  ```rust
  // nuke_token.rs:38,45,51
  let mint_pubkey = "GyzmAHqaujH5nwngLcTucAB8JAyNaL55e16kHyvEpump".parse::<Pubkey>()?;
  let token_account_pubkey = "Fv4oXedcv8tGZQ31qWXcFu56JqNQrVgQYfbhBtGBYo8N".parse::<Pubkey>()?;
  let balance_amount = 7366694314595u64;
  ```
- **Recommended fix:** Parameterize all scripts via CLI arguments or environment variables. Add confirmation prompts before executing destructive operations.

---

### L2. Arbitrary Dust Threshold in Auto-Nuke Safety Gate

- **File:** `src/main.rs:127`
- **Description:** The auto-nuke function refuses to burn/close token accounts with balance > 50,000 base units. This threshold is in raw base units, not USD value. A high-value token with < 50,000 base units (e.g. a token with 2 decimals and a $1 price) could be burned, while a low-value token with > 50,000 base units would be retained unnecessarily.
- **Code:**
  ```rust
  if balance_amount > 50000 {
      println!("⚠️  [Auto-Nuke Safety] Refusing to burn/close account...");
      continue;
  }
  ```
- **Recommended fix:** Calculate the USD value of the balance using the current price and decimals, and gate on a USD threshold (e.g. > $1.00) instead of raw base units.

---

### L3. Heuristic Decimals Fallback Based on Mint Suffix

- **File:** `src/main.rs:51-62`
- **Description:** When on-chain token supply lookup fails, `get_decimals_by_mint` falls back to 6 decimals if the mint address ends with "pump" and 9 decimals otherwise. Wrong decimals produce wrong order sizes (off by orders of magnitude), leading to incorrect trade amounts or failed swaps.
- **Code:**
  ```rust
  if mint.to_lowercase().ends_with("pump") {
      6
  } else {
      9
  }
  ```
- **Recommended fix:** Always fetch decimals on-chain from the mint account data (the SPL Token `Mint` struct has a `decimals` field at offset 44). Cache with a TTL. Fail the trade rather than guessing.

---

### L4. Fragile Manual Byte-Offset Parsing for Mint Authority Check

- **File:** `src/local_isolate.rs:117-125`
- **Description:** `is_mint_renounced_on_chain` checks mint/freeze authority by reading raw byte offsets (0..4 and 46..50) from the account data. While these offsets are correct for the standard SPL Token and Token-2022 mint layout, the parsing is unversioned and fragile — a future token program change would silently break this check, potentially causing the bot to trade tokens with active mint authority.
- **Code:**
  ```rust
  let mint_authority_none = account.data[0..4] == [0, 0, 0, 0];
  let freeze_authority_none = account.data[46..50] == [0, 0, 0, 0];
  ```
- **Recommended fix:** Use the `spl-token` crate's `Pack`/`IsInitialized` traits or a typed deserializer to parse the mint account. This provides compile-time field offsets and handles extension layouts for Token-2022.

---

### L5. `Clone` for `BotConfig` Duplicates Keypair Material Every Iteration

- **File:** `src/config.rs:414-417`
- **Description:** Cloning `BotConfig` copies the 64-byte private key, reconstructs a new `Keypair` from it, and allocates a new `Keypair` struct. This happens on every loop iteration and every `ref_config.read().await.clone()` call. While not a direct security vulnerability, the private key material is copied more frequently than necessary, increasing the attack surface for memory dumps.
- **Code:**
  ```rust
  impl Clone for BotConfig {
      fn clone(&self) -> Self {
          let keypair_bytes = self.keypair.to_bytes();
          let keypair = Keypair::try_from(&keypair_bytes[..]).unwrap();
  ```
- **Recommended fix:** Wrap the keypair in `Arc<Keypair>` and clone the `Arc` (cheap reference bump, no key material copy). This also eliminates the `unwrap()` panic risk (H2).
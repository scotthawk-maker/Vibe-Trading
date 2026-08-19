// discord_notify.rs — Discord webhook notifications for trade events
// Posts to #trades channel via webhook (fire-and-forget, never blocks trading)

use reqwest::Client;
use serde_json::json;
use std::sync::LazyLock;

// C1: The webhook URL (including its secret token) is now loaded from the
// `DISCORD_WEBHOOK_URL` environment variable at first use.  When unset,
// notifications are silently skipped — no hardcoded secret in the binary.
// NOTE: the previously hardcoded webhook token must be rotated in Discord.
static DISCORD_WEBHOOK_URL: LazyLock<Option<String>> =
    LazyLock::new(|| std::env::var("DISCORD_WEBHOOK_URL").ok());

static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(|| {
    Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_default()
});

pub async fn post_trade_embed(
    title: &str,
    description: &str,
    color: u32,
    fields: &[(&str, &str)],
) {
    let mut embed_fields: Vec<serde_json::Value> = Vec::new();
    for (name, value) in fields {
        embed_fields.push(json!({
            "name": name,
            "value": value,
            "inline": true,
        }));
    }

    // Skip silently when the webhook URL is not configured (C1).
    let Some(url) = DISCORD_WEBHOOK_URL.as_deref() else {
        return;
    };

    let payload = json!({
        "embeds": [{
            "title": title,
            "description": description,
            "color": color,
            "fields": embed_fields,
            "footer": {
                "text": "ANTIGRAVITY // Grid Trading Console"
            },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });

    // Fire and forget — never block trading for a Discord notification
    let _ = HTTP_CLIENT
        .post(url)
        .json(&payload)
        .send()
        .await;
}

pub async fn notify_buy(
    token_symbol: &str,
    token_name: &str,
    amount_sol: f64,
    price_usd: f64,
    grid_level: i32,
    signature: &str,
) {
    post_trade_embed(
        &format!("BUY | {} | Grid {}", token_symbol, grid_level),
        &format!("**{}** — bought on Raydium", token_name),
        0x22c55e, // green
        &[
            ("Amount", &format!("{:.4} SOL", amount_sol)),
            ("Price", &format!("${:.8}", price_usd)),
            ("Grid Level", &format!("{}", grid_level)),
            ("Sig", &format!("`{}...`", &signature[..8.min(signature.len())])),
        ],
    )
    .await;
}

pub async fn notify_sell(
    token_symbol: &str,
    exit_reason: &str,
    pnl_pct: f64,
    pnl_usd: f64,
    amount_sol: f64,
    signature: &str,
) {
    let color = if pnl_pct >= 0.0 { 0x22c55e } else { 0xef4444 }; // green or red
    let emoji = if pnl_pct >= 0.0 { "PROFIT" } else { "LOSS" };

    post_trade_embed(
        &format!("SELL | {} | {}", token_symbol, emoji),
        &format!("Exit: {}", exit_reason),
        color,
        &[
            ("P&L %", &format!("{:+.2}%", pnl_pct)),
            ("P&L USD", &format!("${:+.2}", pnl_usd)),
            ("SOL Reclaimed", &format!("{:.4} SOL", amount_sol)),
            ("Sig", &format!("`{}...`", &signature[..8.min(signature.len())])),
        ],
    )
    .await;
}

pub async fn notify_grid_buy(
    token_symbol: &str,
    grid_level: i32,
    amount_sol: f64,
    pnl_pct: f64,
) {
    post_trade_embed(
        &format!("GRID BUY | {} | Level {}", token_symbol, grid_level),
        &format!("Dip entry — price dropped to grid {}", grid_level),
        0xf59e0b, // amber
        &[
            ("Amount", &format!("{:.4} SOL", amount_sol)),
            ("Current P&L", &format!("{:+.2}%", pnl_pct)),
        ],
    )
    .await;
}
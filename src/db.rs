use rusqlite::{params, Connection, Result};
use chrono::Utc;

pub fn init_db() -> Result<Connection> {
    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "data/trades.db".to_string()); if let Some(parent) = std::path::Path::new(&db_path).parent() { let _ = std::fs::create_dir_all(parent); } let conn = Connection::open(&db_path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;

    // Create trades table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            token_address TEXT NOT NULL,
            token_name TEXT,
            token_symbol TEXT,
            status TEXT NOT NULL,
            buy_signature TEXT,
            buy_time TEXT,
            buy_price_usd REAL,
            buy_amount_sol REAL,
            buy_amount_tokens REAL,
            sell_signature TEXT,
            sell_time TEXT,
            sell_price_usd REAL,
            sell_amount_sol REAL,
            realized_pnl_pct REAL,
            realized_pnl_usd REAL,
            fee_sol REAL
        )",
        [],
    )?;

    // Create bot_metrics table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS bot_metrics (
            timestamp TEXT PRIMARY KEY,
            sol_balance REAL,
            token_positions_count INTEGER,
            total_portfolio_value_usd REAL
        )",
        [],
    )?;

    // Create grid_sessions table
    conn.execute(
        "CREATE TABLE IF NOT EXISTS grid_sessions (
            token_address TEXT PRIMARY KEY,
            token_name TEXT,
            token_symbol TEXT,
            baseline_price REAL,
            grids_bought_count INTEGER,
            last_grid_price REAL,
            total_tokens_held REAL,
            total_sol_spent REAL,
            status TEXT
        )",
        [],
    )?;

    Ok(conn)
}

pub fn insert_buy_trade(
    conn: &Connection,
    token_address: &str,
    token_name: &str,
    token_symbol: &str,
    buy_signature: &str,
    buy_price_usd: f64,
    buy_amount_sol: f64,
    buy_amount_tokens: f64,
) -> Result<()> {
    let buy_time = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT INTO trades (
            token_address, token_name, token_symbol, status,
            buy_signature, buy_time, buy_price_usd, buy_amount_sol, buy_amount_tokens
        ) VALUES (?1, ?2, ?3, 'OPEN', ?4, ?5, ?6, ?7, ?8)",
        params![
            token_address,
            token_name,
            token_symbol,
            buy_signature,
            buy_time,
            buy_price_usd,
            buy_amount_sol,
            buy_amount_tokens
        ],
    )?;
    Ok(())
}

pub fn finalize_sell_trade(
    conn: &Connection,
    token_address: &str,
    sell_signature: &str,
    sell_price_usd: f64,
    sell_amount_sol: f64,
    pnl_pct: f64,
    pnl_usd: f64,
    fee_sol: f64,
) -> Result<()> {
    let sell_time = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "UPDATE trades SET 
            status = 'CLOSED',
            sell_signature = ?1,
            sell_time = ?2,
            sell_price_usd = ?3,
            sell_amount_sol = ?4,
            realized_pnl_pct = ?5,
            realized_pnl_usd = ?6,
            fee_sol = ?7
        WHERE token_address = ?8 AND status = 'OPEN'",
        params![
            sell_signature,
            sell_time,
            sell_price_usd,
            sell_amount_sol,
            pnl_pct,
            pnl_usd,
            fee_sol,
            token_address
        ],
    )?;
    Ok(())
}

#[allow(dead_code)]
pub fn get_cooldown_list(conn: &Connection, cooldown_hours: i64) -> Result<Vec<String>> {
    let cutoff = (Utc::now() - chrono::Duration::hours(cooldown_hours))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    let mut stmt = conn.prepare(
        "SELECT DISTINCT token_address FROM trades 
         WHERE buy_time >= ?1 OR sell_time >= ?1",
    )?;
    let rows = stmt.query_map([cutoff], |row| row.get(0))?;

    let mut addresses = Vec::new();
    for addr in rows {
        addresses.push(addr?);
    }
    Ok(addresses)
}

#[allow(dead_code)]
pub fn log_metrics(
    conn: &Connection,
    sol_balance: f64,
    token_positions_count: i64,
    total_portfolio_value_usd: f64,
) -> Result<()> {
    let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    conn.execute(
        "INSERT INTO bot_metrics (timestamp, sol_balance, token_positions_count, total_portfolio_value_usd)
         VALUES (?1, ?2, ?3, ?4)",
        params![timestamp, sol_balance, token_positions_count, total_portfolio_value_usd],
    )?;
    Ok(())
}

use std::env;
use std::error::Error;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
    instruction::{Instruction, AccountMeta},
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use rusqlite::Connection;

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

fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    
    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    let wallet_private_key = env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY missing");
    
    let keypair = parse_keypair(&wallet_private_key).expect("Failed to parse private key");
    let wallet_pubkey = keypair.pubkey();
    
    // H6: RPC URL carries the key as a query param (RpcClient limitation);
    // it is never logged.
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    
    let db_conn = Connection::open("trades.db")?;
    
    // L1: targets are configurable via the NUKE_DUST_TARGETS env var using the
    // format `mint,name,program,balance,is2022` joined by `|`.  Falls back to
    // the documented defaults when unset.
    let default_targets = "6mPxtzzMBYxWpfDaaCX5XZApk9ePRLXWa8ACHCZCLj2y,Glazrad Beach,TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA,1615326987,false|BZ5Ny9j9bG1xWATHfkHkJJNXxM1e5cZCgvM85yndpump,Cicada 3301,TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb,10000,true";
    let targets_raw = env::var("NUKE_DUST_TARGETS").unwrap_or_else(|_| default_targets.to_string());
    let targets: Vec<(String, String, String, u64, bool)> = targets_raw
        .split('|')
        .filter_map(|entry| {
            let parts: Vec<&str> = entry.split(',').collect();
            if parts.len() != 5 {
                return None;
            }
            Some((
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2].to_string(),
                parts[3].parse::<u64>().ok()?,
                parts[4].eq_ignore_ascii_case("true"),
            ))
        })
        .collect();
    
    println!("=== STARTING EMERGENCY RECLAMATION OF BLOCKED TOKENS ===");
    
    for (mint_str, name, prog_str, balance, is_2022) in targets {
        let mint_pubkey = mint_str.parse::<Pubkey>()?;
        let prog_id = prog_str.parse::<Pubkey>()?;
        let ata_program_id = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse::<Pubkey>()?;
        
        // Derive ATA
        let (ata_pubkey, _) = Pubkey::find_program_address(
            &[wallet_pubkey.as_ref(), prog_id.as_ref(), mint_pubkey.as_ref()],
            &ata_program_id,
        );
        
        println!("\n🧹  Nuking and closing account for {}...", name);
        let mut ixs = Vec::new();
        
        if balance > 0 {
            println!("  Found on-chain balance of {} base units. Building Burn...", balance);
            let mut burn_data = vec![if is_2022 { 8 } else { 7 }];
            burn_data.extend_from_slice(&balance.to_le_bytes());
            
            ixs.push(Instruction {
                program_id: prog_id,
                accounts: vec![
                    AccountMeta::new(ata_pubkey, false),
                    AccountMeta::new(mint_pubkey, false),
                    AccountMeta::new_readonly(wallet_pubkey, true),
                ],
                data: burn_data,
            });
        }
        
        println!("  Building CloseAccount instruction...");
        ixs.push(Instruction {
            program_id: prog_id,
            accounts: vec![
                AccountMeta::new(ata_pubkey, false),
                AccountMeta::new(wallet_pubkey, false),
                AccountMeta::new_readonly(wallet_pubkey, true),
            ],
            data: vec![9],
        });
        
        let recent_blockhash = rpc_client.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&wallet_pubkey),
            &[&keypair],
            recent_blockhash,
        );
        
        match rpc_client.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                println!("🎉  SUCCESSFULLY NUKED & CLOSED {}!", name);
                println!("   Signature: https://orbmarkets.io/tx/{}", sig);
                
                db_conn.execute(
                    "UPDATE trades SET status = 'CLOSED', sell_signature = 'NUKED_ON_EMERGENCY_EXIT' WHERE token_address = ?1 AND status = 'OPEN'",
                    [mint_str],
                )?;
            }
            Err(e) => {
                println!("⚠️   Failed to close {}: {:?}", name, e);
            }
        }
    }
    
    println!("\n=== EMERGENCY RECLAMATION COMPLETE ===");
    Ok(())
}

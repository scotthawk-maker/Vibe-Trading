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

fn run() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    
    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    let wallet_private_key = env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY missing");
    
    let keypair = parse_keypair(&wallet_private_key).expect("Failed to parse private key");
    let wallet_pubkey = keypair.pubkey();
    
    println!("Parsing mint_pubkey...");
    // L1: parameterize previously hardcoded mint/token-account/balance via env
    // vars (with the original values as defaults) so the script is not
    // hardwired to a single token.
    let mint_str = env::var("NUKE_MINT").unwrap_or_else(|_| "GyzmAHqaujH5nwngLcTucAB8JAyNaL55e16kHyvEpump".to_string());
    let token_account_str = env::var("NUKE_TOKEN_ACCOUNT").unwrap_or_else(|_| "Fv4oXedcv8tGZQ31qWXcFu56JqNQrVgQYfbhBtGBYo8N".to_string());
    let balance_amount: u64 = env::var("NUKE_BALANCE").unwrap_or_else(|_| "7366694314595".to_string()).parse().expect("Invalid NUKE_BALANCE integer");

    let mint_pubkey = mint_str.parse::<Pubkey>().map_err(|_| format!("Invalid mint pubkey: {}", mint_str))?;
    
    println!("Parsing token_program_id...");
    // Correct Token-2022 program ID
    let token_program_id = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse::<Pubkey>()?;
    
    println!("Parsing token_account_pubkey...");
    let token_account_pubkey = token_account_str.parse::<Pubkey>().map_err(|_| format!("Invalid token account pubkey: {}", token_account_str))?;
    
    println!("Wallet:             {}", wallet_pubkey);
    println!("Token Mint:         {}", mint_pubkey);
    println!("Token Account:      {}", token_account_pubkey);
    
    println!("On-chain Balance:   {} base units", balance_amount);
    
    let mut ixs = Vec::new();
    
    if balance_amount > 0 {
        println!("Building Burn instruction...");
        // SPL Token-2022 Burn discriminant is 8, followed by u64 amount
        let mut burn_data = vec![8];
        burn_data.extend_from_slice(&balance_amount.to_le_bytes());
        
        let burn_ix = Instruction {
            program_id: token_program_id,
            accounts: vec![
                AccountMeta::new(token_account_pubkey, false),
                AccountMeta::new(mint_pubkey, false),
                AccountMeta::new_readonly(wallet_pubkey, true),
            ],
            data: burn_data,
        };
        ixs.push(burn_ix);
    }
    
    println!("Building CloseAccount instruction...");
    // SPL Token CloseAccount discriminant is 9
    let close_data = vec![9];
    let close_ix = Instruction {
        program_id: token_program_id,
        accounts: vec![
            AccountMeta::new(token_account_pubkey, false),
            AccountMeta::new(wallet_pubkey, false),
            AccountMeta::new_readonly(wallet_pubkey, true),
        ],
        data: close_data,
    };
    ixs.push(close_ix);
    
    // H6: build the RPC URL with the API key.  The solana RpcClient only
    // accepts a URL (no header injection), so the query param is retained
    // here; the URL is never printed/logged.
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    
    // L1: confirmation prompt before destructive burn/close.
    let auto_yes = env::var("NUKE_CONFIRM_YES").unwrap_or_default() == "1";
    if !auto_yes {
        println!("⚠️  About to BURN {} base units of {} and close account {}. Proceed? [y/N]", balance_amount, mint_pubkey, token_account_pubkey);
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted by user.");
            return Ok(());
        }
    }

    // Build & sign transaction
    let recent_blockhash = rpc_client.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&wallet_pubkey),
        &[&keypair],
        recent_blockhash,
    );
    
    println!("Submitting transaction to burn and close PROP...");
    let sig = rpc_client.send_and_confirm_transaction(&tx)?;
    println!("🎉 NUKE TRANSACTION EXECUTED SUCCESSFULLY!");
    println!("   Signature: https://orbmarkets.io/tx/{}", sig);
    
    Ok(())
}

fn main() {
    match run() {
        Ok(_) => println!("Nuke execution complete!"),
        Err(e) => eprintln!("Detailed Debug Error: {:?}", e),
    }
}

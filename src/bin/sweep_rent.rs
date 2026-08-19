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
use solana_client::rpc_request::TokenAccountsFilter;

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

fn sweep_by_program(
    rpc_client: &RpcClient,
    wallet_pubkey: &Pubkey,
    keypair: &Keypair,
    program_id: Pubkey,
) -> Result<usize, Box<dyn Error>> {
    println!("Fetching accounts for program {}...", program_id);
    let accounts = rpc_client.get_token_accounts_by_owner(
        wallet_pubkey,
        TokenAccountsFilter::ProgramId(program_id),
    )?;

    let mut empty_accounts = Vec::new();

    for keyed_acc in accounts {
        let ata_pubkey = keyed_acc.pubkey.parse::<Pubkey>()?;
        let parsed_data = match keyed_acc.account.data {
            solana_client::rpc_response::UiAccountData::Json(parsed) => parsed,
            _ => continue,
        };

        if parsed_data.program == "spl-token" || parsed_data.program == "spl-token-2022" {
            if let Some(info) = parsed_data.parsed.get("info") {
                if let Some(token_amount) = info.get("tokenAmount") {
                    if let Some(amount_val) = token_amount.get("amount") {
                        if let Some(amount_str) = amount_val.as_str() {
                            let amount = amount_str.parse::<u64>().unwrap_or(0);
                            if amount == 0 {
                                empty_accounts.push(ata_pubkey);
                            }
                        } else if let Some(amount_num) = amount_val.as_u64() {
                            if amount_num == 0 {
                                empty_accounts.push(ata_pubkey);
                            }
                        }
                    }
                }
            }
        }
    }

    println!("Found {} empty accounts under program {}", empty_accounts.len(), program_id);
    if empty_accounts.is_empty() {
        return Ok(0);
    }

    let mut closed_count = 0;
    let chunk_size = 8;
    for chunk in empty_accounts.chunks(chunk_size) {
        let mut ixs = Vec::new();
        for &ata in chunk {
            let ix = Instruction {
                program_id,
                accounts: vec![
                    AccountMeta::new(ata, false),
                    AccountMeta::new(*wallet_pubkey, false),
                    AccountMeta::new_readonly(*wallet_pubkey, true),
                ],
                data: vec![9],
            };
            ixs.push(ix);
        }

        let recent_blockhash = rpc_client.get_latest_blockhash()?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(wallet_pubkey),
            &[keypair],
            recent_blockhash,
        );

        match rpc_client.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                closed_count += chunk.len();
                println!("🎉 Successfully closed batch of {} accounts. Sig: https://orbmarkets.io/tx/{}", chunk.len(), sig);
            }
            Err(e) => {
                println!("⚠️ Failed to close batch: {:?}", e);
            }
        }
    }

    Ok(closed_count)
}

fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    let wallet_private_key = env::var("WALLET_PRIVATE_KEY").expect("WALLET_PRIVATE_KEY missing");

    let keypair = parse_keypair(&wallet_private_key).expect("Failed to parse private key");
    let wallet_pubkey = keypair.pubkey();

    // H6: RPC URL carries the key as a query param (RpcClient limitation); never logged.
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    println!("=== STARTING EMPTY ACCOUNT RENT RECLAMATION SWEEP ===");
    println!("Wallet: {}", wallet_pubkey);

    let token_program_id = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse::<Pubkey>()?;
    let token_2022_program_id = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse::<Pubkey>()?;

    let closed_token = sweep_by_program(&rpc_client, &wallet_pubkey, &keypair, token_program_id)?;
    let closed_2022 = sweep_by_program(&rpc_client, &wallet_pubkey, &keypair, token_2022_program_id)?;

    let total_closed = closed_token + closed_2022;
    println!("\n=== SWEEP COMPLETE ===");
    println!("Total accounts closed: {}", total_closed);
    println!("SOL reclaimed: {:.6} SOL", total_closed as f64 * 0.002039);

    Ok(())
}

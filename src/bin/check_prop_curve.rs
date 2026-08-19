use std::env;
use std::error::Error;
use solana_sdk::{
    pubkey::Pubkey,
};
use solana_client::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;

#[derive(Debug)]
struct BondingCurve {
    virtual_token_reserves: u64,
    virtual_sol_reserves: u64,
    real_token_reserves: u64,
    real_sol_reserves: u64,
    token_total_supply: u64,
    complete: bool,
}

impl BondingCurve {
    fn deserialize(data: &[u8]) -> Result<Self, Box<dyn Error>> {
        if data.len() < 49 {
            return Err("Data too short".into());
        }
        // Offset 8 for Anchor discriminator
        let mut offset = 8;
        
        let virtual_token_reserves = u64::from_le_bytes(data[offset..offset+8].try_into()?);
        offset += 8;
        
        let virtual_sol_reserves = u64::from_le_bytes(data[offset..offset+8].try_into()?);
        offset += 8;
        
        let real_token_reserves = u64::from_le_bytes(data[offset..offset+8].try_into()?);
        offset += 8;
        
        let real_sol_reserves = u64::from_le_bytes(data[offset..offset+8].try_into()?);
        offset += 8;
        
        let token_total_supply = u64::from_le_bytes(data[offset..offset+8].try_into()?);
        offset += 8;
        
        let complete = data[offset] != 0;
        
        Ok(Self {
            virtual_token_reserves,
            virtual_sol_reserves,
            real_token_reserves,
            real_sol_reserves,
            token_total_supply,
            complete,
        })
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();
    let helius_api_key = env::var("HELIUS_API_KEY").expect("HELIUS_API_KEY missing");
    // H6: RPC URL carries the key as a query param (RpcClient limitation);
    // it is never logged.
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={}", helius_api_key);
    let rpc_client = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    
    // L1: parameterize the previously hardcoded mint via env var.
    let mint_str = env::var("CHECK_MINT").unwrap_or_else(|_| "GyzmAHqaujH5nwngLcTucAB8JAyNaL55e16kHyvEpump".to_string());
    let mint_pubkey = mint_str.parse::<Pubkey>().map_err(|_| format!("Invalid mint pubkey: {}", mint_str))?;
    let pump_program_id = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P".parse::<Pubkey>()?;
    
    // Derive bonding curve PDA
    let (bonding_curve_pda, _) = Pubkey::find_program_address(
        &[b"bonding-curve", mint_pubkey.as_ref()],
        &pump_program_id,
    );
    
    println!("Bonding Curve PDA for PROP: {}", bonding_curve_pda);
    
    let account_data = rpc_client.get_account_data(&bonding_curve_pda)?;
    let curve = BondingCurve::deserialize(&account_data)?;
    
    println!("Deserialized Bonding Curve State:");
    println!("  Virtual Token Reserves: {}", curve.virtual_token_reserves);
    println!("  Virtual SOL Reserves:   {}", curve.virtual_sol_reserves);
    println!("  Real Token Reserves:    {}", curve.real_token_reserves);
    println!("  Real SOL Reserves:      {}", curve.real_sol_reserves);
    println!("  Token Total Supply:     {}", curve.token_total_supply);
    println!("  Complete (Graduated):   {}", curve.complete);
    
    Ok(())
}

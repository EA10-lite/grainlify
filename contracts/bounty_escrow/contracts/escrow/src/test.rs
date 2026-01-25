#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Env, token};

fn setup() -> (Env, BountyEscrowContractClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    
    // Start at a non-zero timestamp to avoid 0 check issues
    env.ledger().set_timestamp(1000);
    
    let contract_id = env.register_contract(None, BountyEscrowContract);
    let client = BountyEscrowContractClient::new(&env, &contract_id);
    
    let admin = Address::generate(&env);
    
    let token_admin = Address::generate(&env);
    let token_id = env.register_stellar_asset_contract(token_admin.clone());
    
    client.init(&admin, &token_id);
    (env, client, admin, token_id)
}

#[test]
fn test_rate_limit() {
    let (env, client, _admin, token_id) = setup();
    let depositor = Address::generate(&env);
    token::StellarAssetClient::new(&env, &token_id).mint(&depositor, &1000);

    // Default: 10 ops per hour, 60s cooldown
    
    // First op at T=1000
    client.lock_funds(&depositor, &1, &100, &2000);

    // Second op immediately (still T=1000) -> CooldownActive
    let res = client.try_lock_funds(&depositor, &2, &100, &2000);
    assert_eq!(res, Err(Ok(Error::CooldownActive)));

    // Wait 61s -> T=1061
    env.ledger().set_timestamp(1061);
    client.lock_funds(&depositor, &2, &100, &2000);
}

#[test]
fn test_whitelist() {
    let (env, client, _admin, token_id) = setup();
    let depositor = Address::generate(&env);
    token::StellarAssetClient::new(&env, &token_id).mint(&depositor, &1000);

    client.set_whitelist_status(&depositor, &true);

    // Whitelisted addresses bypass cooldown
    client.lock_funds(&depositor, &1, &100, &2000);
    client.lock_funds(&depositor, &2, &100, &2000);
}

#[test]
fn test_config_update() {
    let (env, client, _admin, token_id) = setup();
    let depositor = Address::generate(&env);
    token::StellarAssetClient::new(&env, &token_id).mint(&depositor, &1000);
    
    // Update config to 1 op max
    client.set_rate_limit_config(&3600, &1, &0);
    
    client.lock_funds(&depositor, &1, &100, &2000);
    
    // Second op should fail with RateLimitExceeded (cooldown is 0)
    let res = client.try_lock_funds(&depositor, &2, &100, &2000);
    assert_eq!(res, Err(Ok(Error::RateLimitExceeded)));
}

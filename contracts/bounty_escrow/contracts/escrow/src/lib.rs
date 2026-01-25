#![no_std]
use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    BountyExists = 3,
    BountyNotFound = 4,
    FundsNotLocked = 5,
    DeadlineNotPassed = 6,
    Unauthorized = 7,
    RateLimitExceeded = 8,
    CooldownActive = 9,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EscrowStatus {
    Locked,
    Released,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Escrow {
    pub depositor: Address,
    pub amount: i128,
    pub status: EscrowStatus,
    pub deadline: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RateLimitConfig {
    pub window_seconds: u64,
    pub max_ops_per_window: u32,
    pub cooldown_seconds: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserStats {
    pub last_operation_time: u64,
    pub window_start_time: u64,
    pub op_count: u32,
}

#[contracttype]
pub enum DataKey {
    Admin,
    Token,
    Escrow(u64), // bounty_id
    RateLimitCfg,
    UserStats(Address),
    Whitelist(Address),
}

#[contract]
pub struct BountyEscrowContract;

#[contractimpl]
impl BountyEscrowContract {
    /// Initialize the contract with the admin address and the token address (XLM).
    pub fn init(env: Env, admin: Address, token: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Token, &token);

        // Default rate limit: 10 ops per hour, 60s cooldown
        let default_config = RateLimitConfig {
            window_seconds: 3600,
            max_ops_per_window: 10,
            cooldown_seconds: 60,
        };
        env.storage().instance().set(&DataKey::RateLimitCfg, &default_config);

        Ok(())
    }

    /// Set rate limit configuration. Only Admin.
    pub fn set_rate_limit_config(
        env: Env,
        window_seconds: u64,
        max_ops_per_window: u32,
        cooldown_seconds: u64,
    ) -> Result<(), Error> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).ok_or(Error::NotInitialized)?;
        admin.require_auth();

        let config = RateLimitConfig {
            window_seconds,
            max_ops_per_window,
            cooldown_seconds,
        };
        env.storage().instance().set(&DataKey::RateLimitCfg, &config);
        Ok(())
    }

    /// Set whitelist status for an address. Only Admin.
    pub fn set_whitelist_status(env: Env, address: Address, whitelisted: bool) -> Result<(), Error> {
        let admin: Address = env.storage().instance().get(&DataKey::Admin).ok_or(Error::NotInitialized)?;
        admin.require_auth();

        if whitelisted {
            env.storage().instance().set(&DataKey::Whitelist(address), &true);
        } else {
            env.storage().instance().remove(&DataKey::Whitelist(address));
        }
        Ok(())
    }

    fn check_rate_limit(env: &Env, user: &Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Whitelist(user.clone())) {
            return Ok(());
        }

        let config: RateLimitConfig = env
            .storage()
            .instance()
            .get(&DataKey::RateLimitCfg)
            .unwrap(); // Should exist after init

        let now = env.ledger().timestamp();
        let mut stats: UserStats = env
            .storage()
            .temporary()
            .get(&DataKey::UserStats(user.clone()))
            .unwrap_or(UserStats {
                last_operation_time: 0,
                window_start_time: now,
                op_count: 0,
            });

        // 1. Check cooldown
        if stats.last_operation_time > 0 && now < stats.last_operation_time + config.cooldown_seconds {
            env.events().publish((Symbol::new(&env, "rate_limit_violation"), user.clone()), (Symbol::new(&env, "cooldown"), now));
            return Err(Error::CooldownActive);
        }

        // 2. Check window
        if now >= stats.window_start_time + config.window_seconds {
            // New window
            stats.window_start_time = now;
            stats.op_count = 1;
        } else {
            // Same window
            if stats.op_count >= config.max_ops_per_window {
                env.events().publish((Symbol::new(&env, "rate_limit_violation"), user.clone()), (Symbol::new(&env, "max_ops"), now));
                return Err(Error::RateLimitExceeded);
            }
            stats.op_count += 1;
        }

        stats.last_operation_time = now;
        env.storage().temporary().set(&DataKey::UserStats(user.clone()), &stats);
        
        // Extend TTL for stats
        env.storage().temporary().extend_ttl(&DataKey::UserStats(user.clone()), 17280, 17280); // ~1 day

        Ok(())
    }

    /// Lock funds for a specific bounty.
    pub fn lock_funds(
        env: Env,
        depositor: Address,
        bounty_id: u64,
        amount: i128,
        deadline: u64,
    ) -> Result<(), Error> {
        depositor.require_auth();
        Self::check_rate_limit(&env, &depositor)?;

        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::NotInitialized);
        }

        if env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyExists);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds from depositor to contract
        client.transfer(&depositor, &env.current_contract_address(), &amount);

        let escrow = Escrow {
            depositor: depositor.clone(),
            amount,
            status: EscrowStatus::Locked,
            deadline,
        };

        // Extend the TTL of the storage entry to ensure it lives long enough
        env.storage().persistent().set(&DataKey::Escrow(bounty_id), &escrow);
        
        // Emit value allows for off-chain indexing
        env.events().publish(
            (Symbol::new(&env, "funds_locked"), bounty_id),
            (depositor, amount, deadline)
        );

        Ok(())
    }

    /// Release funds to the contributor.
    /// Only the admin (backend) can authorize this.
    pub fn release_funds(env: Env, bounty_id: u64, contributor: Address) -> Result<(), Error> {
        if !env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::NotInitialized);
        }

        let admin: Address = env.storage().instance().get(&DataKey::Admin).unwrap();
        admin.require_auth();

        if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }

        let mut escrow: Escrow = env.storage().persistent().get(&DataKey::Escrow(bounty_id)).unwrap();

        if escrow.status != EscrowStatus::Locked {
            return Err(Error::FundsNotLocked);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds to contributor
        client.transfer(&env.current_contract_address(), &contributor, &escrow.amount);

        escrow.status = EscrowStatus::Released;
        env.storage().persistent().set(&DataKey::Escrow(bounty_id), &escrow);

        env.events().publish(
            (Symbol::new(&env, "funds_released"), bounty_id),
            (contributor, escrow.amount)
        );

        Ok(())
    }

    /// Refund funds to the original depositor if the deadline has passed.
    pub fn refund(env: Env, bounty_id: u64) -> Result<(), Error> {
        // We'll allow anyone to trigger the refund if conditions are met, 
        // effectively making it permissionless but conditional.
        // OR we can require depositor auth. Let's make it permissionless to ensure funds aren't stuck if depositor key is lost,
        // but strictly logic bound.
        // However, usually refund is triggered by depositor. Let's stick to logic.
        
        if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }

        let mut escrow: Escrow = env.storage().persistent().get(&DataKey::Escrow(bounty_id)).unwrap();

        if escrow.status != EscrowStatus::Locked {
            return Err(Error::FundsNotLocked);
        }

        let now = env.ledger().timestamp();
        if now < escrow.deadline {
            return Err(Error::DeadlineNotPassed);
        }

        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);

        // Transfer funds back to depositor
        client.transfer(&env.current_contract_address(), &escrow.depositor, &escrow.amount);

        escrow.status = EscrowStatus::Refunded;
        env.storage().persistent().set(&DataKey::Escrow(bounty_id), &escrow);

        env.events().publish(
            (Symbol::new(&env, "funds_refunded"), bounty_id),
            (escrow.depositor, escrow.amount)
        );

        Ok(())
    }

    /// view function to get escrow info
    pub fn get_escrow_info(env: Env, bounty_id: u64) -> Result<Escrow, Error> {
         if !env.storage().persistent().has(&DataKey::Escrow(bounty_id)) {
            return Err(Error::BountyNotFound);
        }
        Ok(env.storage().persistent().get(&DataKey::Escrow(bounty_id)).unwrap())
    }

    /// view function to get contract balance of the token
    pub fn get_balance(env: Env) -> Result<i128, Error> {
         if !env.storage().instance().has(&DataKey::Token) {
            return Err(Error::NotInitialized);
        }
        let token_addr: Address = env.storage().instance().get(&DataKey::Token).unwrap();
        let client = token::Client::new(&env, &token_addr);
        Ok(client.balance(&env.current_contract_address()))
    }
}

#[cfg(test)]
mod test;


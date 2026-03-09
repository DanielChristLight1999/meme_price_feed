use anchor_lang::prelude::*;

declare_id!("6iaXp3NuE2cuh9D1KRRuiLSHNowsYYepiJUBarHMRTRE");

// ============================================================================
// Constants (aligned with oracle-price-main)
// ============================================================================

/// Pyth Receiver Program (owner of PriceUpdateV2 accounts)
const PYTH_RECEIVER: Pubkey = pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
/// Pyth Push Oracle Program (alternative owner)
const PYTH_PUSH_ORACLE: Pubkey = pubkey!("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");

/// SOL/USD Pyth feed ID (first 32 bytes at offset 42 in Pyth layout)
const SOL_USD_FEED_ID: [u8; 32] = [
    0xef, 0x0d, 0x8b, 0x6f, 0xda, 0x2c, 0xeb, 0xa4, 0x1d, 0xa1, 0x5d, 0x40, 0x95, 0xd1, 0xda,
    0x39, 0x2a, 0x0d, 0x2f, 0x8e, 0xd0, 0xc6, 0xc7, 0xbc, 0x0f, 0x4c, 0xfa, 0xc8, 0xc2, 0x80,
    0xb5, 0x6d,
];

const SPL_TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SPL_TOKEN_2022_PROGRAM: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Allowed pool program owners (PumpSwap, Raydium, Orca, etc.)
const PUMP_SWAP_PROGRAM: Pubkey = pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

// Account data offsets (oracle-price-main)
const SPL_AMOUNT_OFFSET: usize = 64;
const POOL_BASE_TOKEN_OFFSET: usize = 139;
const POOL_QUOTE_TOKEN_OFFSET: usize = 171;
const PYTH_FEED_ID_OFFSET: usize = 41;
const PYTH_PRICE_OFFSET: usize = 73;
const PYTH_CONF_OFFSET: usize = 81;
const PYTH_EXPO_OFFSET: usize = 89;
const PYTH_PUBLISH_TIME_OFFSET: usize = 93;

const MAX_PYTH_STALENESS: i64 = 60;
const MAX_CONF_RATIO_X20: u64 = 20;
/// Minimum quote reserve (SOL side) in lamports to accept a pool (10 SOL)
const MIN_POOL_LIQUIDITY_SOL: u64 = 10_000_000_000;
/// Minimum base token reserve (raw units) to avoid extremely shallow pools
const MIN_POOL_BASE_RESERVE: u64 = 1_000_000;
const TWAP_SIZE: usize = 30;

/// Target scale exponent for TOKEN/USD (consumer expects ×10^6)
const TARGET_EXPONENT: i32 = -6;

// ============================================================================
// Program
// ============================================================================

#[program]
pub mod meme_price_feed {
    use super::*;

    /// Creates a synthetic Pyth-layout feed PDA for one or more pools.
    /// Seeds: ["feed", pool_0.key()]. Configure up to 3 pools via remaining_accounts (extra pool pubkeys).
    /// Access: permissionless (anyone can create a feed for any pool).
    pub fn initialize_feed(ctx: Context<InitializeFeed>) -> Result<()> {
        let feed = &mut ctx.accounts.feed;
        let pool_0 = ctx.accounts.pool.key();
        feed.pool_0 = pool_0;
        let extra = ctx.remaining_accounts;
        let pool_count = 1u8.saturating_add(extra.len() as u8).min(3);
        feed.pool_count = pool_count;
        feed.bump = ctx.bumps.feed;
        feed.admin = ctx.accounts.payer.key();
        feed.exponent = TARGET_EXPONENT;
        feed.pools_1_2[0] = if extra.len() >= 1 { extra[0].key() } else { Pubkey::default() };
        feed.pools_1_2[1] = if extra.len() >= 2 { extra[1].key() } else { Pubkey::default() };
        Ok(())
    }

    /// Updates the feed: reads up to 3 pools + Pyth SOL/USD, median TOKEN/USD ×10^6,
    /// TWAP over 30 samples, writes Pyth layout at offsets 73, 89, 93.
    /// Permissionless after warm-up (first 30 updates admin-only).
    pub fn update_price(ctx: Context<UpdatePrice>) -> Result<()> {
        let feed = &mut ctx.accounts.feed;
        let clock = Clock::get()?;

        require!(feed.last_slot < clock.slot, MemePriceFeedError::SlotAlreadyUpdated);
        require!(
            ctx.accounts.pool.key() == feed.pool_0,
            MemePriceFeedError::PoolMismatch
        );

        if feed.history_count < TWAP_SIZE as u8 {
            require!(
                ctx.accounts.caller.key() == feed.admin,
                MemePriceFeedError::WarmupAdminOnly
            );
        }

        let pool_count = feed.pool_count as usize;
        require!(
            ctx.remaining_accounts.len() >= (pool_count.saturating_sub(1)) * 3,
            MemePriceFeedError::AccountDataTooSmall
        );

        let (pyth_price, pyth_expo) = validate_pyth(&ctx.accounts.pyth_sol_usd, &clock)?;
        let mut prices: [u64; 3] = [0; 3];
        let mut n: usize = 0;

        if pool_count >= 1 {
            if let Some(p) = try_single_pool_price(
                &ctx.accounts.pool,
                &ctx.accounts.pool_base_token,
                &ctx.accounts.pool_quote_token,
                feed.pool_0,
                pyth_price,
                pyth_expo,
            )? {
                prices[n] = p;
                n += 1;
            }
        }
        if pool_count >= 2 && ctx.remaining_accounts.len() >= 3 {
            let p1 = &ctx.remaining_accounts[0];
            let b1 = &ctx.remaining_accounts[1];
            let q1 = &ctx.remaining_accounts[2];
            if let Some(p) = try_single_pool_price(
                p1,
                b1,
                q1,
                feed.pools_1_2[0],
                pyth_price,
                pyth_expo,
            )? {
                prices[n] = p;
                n += 1;
            }
        }
        if pool_count >= 3 && ctx.remaining_accounts.len() >= 6 {
            let p2 = &ctx.remaining_accounts[3];
            let b2 = &ctx.remaining_accounts[4];
            let q2 = &ctx.remaining_accounts[5];
            if let Some(p) = try_single_pool_price(
                p2,
                b2,
                q2,
                feed.pools_1_2[1],
                pyth_price,
                pyth_expo,
            )? {
                prices[n] = p;
                n += 1;
            }
        }

        require!(n >= 2, MemePriceFeedError::InsufficientLiquidity);

        let price_e6 = median_of_3(&prices[..n]);

        update_feed_state(feed, price_e6, &clock);

        Ok(())
    }
}

// ============================================================================
// Shared logic (from oracle-price-main)
// ============================================================================

fn read_u64_le(data: &[u8], offset: usize) -> Result<u64> {
    require!(
        data.len() >= offset + 8,
        MemePriceFeedError::AccountDataTooSmall
    );
    Ok(u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()))
}

fn read_i64_le(data: &[u8], offset: usize) -> Result<i64> {
    require!(
        data.len() >= offset + 8,
        MemePriceFeedError::AccountDataTooSmall
    );
    Ok(i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()))
}

fn read_i32_le(data: &[u8], offset: usize) -> Result<i32> {
    require!(
        data.len() >= offset + 4,
        MemePriceFeedError::AccountDataTooSmall
    );
    Ok(i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()))
}

fn read_pubkey(data: &[u8], offset: usize) -> Result<Pubkey> {
    require!(
        data.len() >= offset + 32,
        MemePriceFeedError::AccountDataTooSmall
    );
    Ok(Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap()))
}

/// Validates Pyth SOL/USD feed and returns (price, exponent) for TOKEN/USD conversion.
fn validate_pyth<'info>(
    pyth_price_feed: &AccountInfo<'info>,
    clock: &Clock,
) -> Result<(u64, i32)> {
    let pyth_owner = pyth_price_feed.owner;
    require!(
        *pyth_owner == PYTH_RECEIVER || *pyth_owner == PYTH_PUSH_ORACLE,
        MemePriceFeedError::InvalidPythOwner
    );
    let pyth_data = pyth_price_feed.try_borrow_data()?;
    require!(pyth_data.len() >= 102, MemePriceFeedError::AccountDataTooSmall);

    let feed_id: &[u8] = &pyth_data[PYTH_FEED_ID_OFFSET..PYTH_FEED_ID_OFFSET + 32];
    require!(
        feed_id == SOL_USD_FEED_ID,
        MemePriceFeedError::InvalidPythFeedId
    );

    let pyth_price_raw = read_i64_le(&pyth_data, PYTH_PRICE_OFFSET)?;
    let pyth_conf = read_u64_le(&pyth_data, PYTH_CONF_OFFSET)?;
    let pyth_expo = read_i32_le(&pyth_data, PYTH_EXPO_OFFSET)?;
    let pyth_publish_time = read_i64_le(&pyth_data, PYTH_PUBLISH_TIME_OFFSET)?;

    require!(pyth_price_raw > 0, MemePriceFeedError::InvalidPythPrice);
    let age = clock.unix_timestamp.saturating_sub(pyth_publish_time);
    require!(
        age >= 0 && age <= MAX_PYTH_STALENESS,
        MemePriceFeedError::PythStale
    );
    let pyth_price = pyth_price_raw as u64;
    require!(
        pyth_conf.saturating_mul(MAX_CONF_RATIO_X20) < pyth_price,
        MemePriceFeedError::PythConfidenceTooWide
    );
    Ok((pyth_price, pyth_expo))
}

/// Compute TOKEN/USD ×10^6 for one pool. Returns None if pool should be skipped (e.g. low liquidity).
fn try_single_pool_price<'info>(
    pool: &AccountInfo<'info>,
    pool_base_token: &AccountInfo<'info>,
    pool_quote_token: &AccountInfo<'info>,
    expected_pool: Pubkey,
    pyth_price: u64,
    pyth_expo: i32,
) -> Result<Option<u64>> {
    if pool.key() != expected_pool || expected_pool == Pubkey::default() {
        return Ok(None);
    }
    require!(
        *pool.owner == PUMP_SWAP_PROGRAM,
        MemePriceFeedError::InvalidPoolOwner
    );
    let pool_data = pool.try_borrow_data()?;
    if pool_data.len() < 203 {
        return Ok(None);
    }
    let expected_base = read_pubkey(&pool_data, POOL_BASE_TOKEN_OFFSET)?;
    let expected_quote = read_pubkey(&pool_data, POOL_QUOTE_TOKEN_OFFSET)?;
    require!(
        pool_base_token.key() == expected_base,
        MemePriceFeedError::PoolTokenMismatch
    );
    require!(
        pool_quote_token.key() == expected_quote,
        MemePriceFeedError::PoolTokenMismatch
    );
    require!(
        *pool_base_token.owner == SPL_TOKEN_PROGRAM
            || *pool_base_token.owner == SPL_TOKEN_2022_PROGRAM,
        MemePriceFeedError::InvalidTokenOwner
    );
    require!(
        *pool_quote_token.owner == SPL_TOKEN_PROGRAM
            || *pool_quote_token.owner == SPL_TOKEN_2022_PROGRAM,
        MemePriceFeedError::InvalidTokenOwner
    );

    let base_data = pool_base_token.try_borrow_data()?;
    let quote_data = pool_quote_token.try_borrow_data()?;
    let base_reserve = read_u64_le(&base_data, SPL_AMOUNT_OFFSET)?;
    let quote_reserve = read_u64_le(&quote_data, SPL_AMOUNT_OFFSET)?;

    if base_reserve < MIN_POOL_BASE_RESERVE || quote_reserve < MIN_POOL_LIQUIDITY_SOL {
        return Ok(None);
    }

    let expo_sum: i32 = 3 + pyth_expo;
    let (num, den) = if expo_sum >= 0 {
        (
            (quote_reserve as u128) * (pyth_price as u128) * 10u128.pow(expo_sum as u32),
            base_reserve as u128,
        )
    } else {
        (
            (quote_reserve as u128) * (pyth_price as u128),
            (base_reserve as u128) * 10u128.pow((-expo_sum) as u32),
        )
    };
    if den == 0 {
        return Ok(None);
    }
    let price_e6 = (num / den) as u64;
    if price_e6 == 0 {
        return Ok(None);
    }
    Ok(Some(price_e6))
}

/// Constant-time median for 2 or 3 values (no sort). Uses u128 for overflow safety.
#[inline(always)]
fn median_of_3(prices: &[u64]) -> u64 {
    match prices.len() {
        2 => ((prices[0] as u128 + prices[1] as u128) / 2) as u64,
        3 => {
            let a = prices[0] as u128;
            let b = prices[1] as u128;
            let c = prices[2] as u128;
            let min_p = a.min(b).min(c);
            let max_p = a.max(b).max(c);
            (a + b + c - min_p - max_p) as u64
        }
        _ => prices[0],
    }
}

fn update_feed_state(
    feed: &mut Account<SyntheticFeed>,
    price_e6: u64,
    clock: &Clock,
) {
    let idx = feed.history_idx as usize;
    feed.price_history[idx] = price_e6;
    feed.history_idx = ((idx + 1) % TWAP_SIZE) as u8;
    if feed.history_count < TWAP_SIZE as u8 {
        feed.history_count += 1;
    }

    let count = feed.history_count as usize;
    let sum: u128 = feed.price_history[..count].iter().map(|&p| p as u128).sum();
    let twap_e6 = (sum / count as u128) as u64;

    feed.price = twap_e6 as i64;
    feed.exponent = TARGET_EXPONENT;
    feed.publish_time = clock.unix_timestamp;
    feed.last_slot = clock.slot;
}

// ============================================================================
// Accounts
// ============================================================================

#[derive(Accounts)]
pub struct InitializeFeed<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// Feed PDA. Seeds: ["feed", pool.key()]. Pyth-layout compatible (price at 73, exponent 89, publish_time 93).
    #[account(
        init,
        payer = payer,
        space = 8 + SyntheticFeed::INIT_SPACE,
        seeds = [b"feed", pool.key().as_ref()],
        bump,
    )]
    pub feed: Account<'info, SyntheticFeed>,

    /// CHECK: First pool (PumpSwap/Raydium/Orca); used for PDA seeds. Validated on update_price.
    pub pool: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdatePrice<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"feed", feed.pool_0.as_ref()],
        bump = feed.bump,
    )]
    pub feed: Account<'info, SyntheticFeed>,

    /// CHECK: First pool; validated against feed.pool_0
    pub pool: AccountInfo<'info>,

    /// CHECK: Pool base token account; validated against pool data
    pub pool_base_token: AccountInfo<'info>,

    /// CHECK: Pool quote token account; validated against pool data
    pub pool_quote_token: AccountInfo<'info>,

    /// CHECK: Pyth SOL/USD price feed; validated by owner + feed id
    pub pyth_sol_usd: AccountInfo<'info>,
}

// ============================================================================
// State (layout chosen so consumer sees price at 73, exponent at 89, publish_time at 93)
// ============================================================================

#[account]
#[derive(InitSpace)]
pub struct SyntheticFeed {
    // Header: 65 bytes so that price starts at byte 73 (8 discriminator + 65 = 73)
    /// First pool (also used for PDA seeds)
    pub pool_0: Pubkey,          // 32
    pub pool_count: u8,          // 1  (1..=3)
    pub bump: u8,                // 1
    pub last_slot: u64,          // 8
    pub history_idx: u8,         // 1
    pub history_count: u8,       // 1
    pub _header_pad: [u8; 21],   // 21 -> total 65

    // Pyth layout (when_meme_prediction reads these offsets)
    pub price: i64,     // 8  @ 73  = TOKEN/USD × 10^6 (TWAP)
    pub _pad1: [u8; 8], // 8  @ 81
    pub exponent: i32,     // 4  @ 89  = -6
    pub publish_time: i64, // 8  @ 93  = unix timestamp

    /// Pools 1 and 2 (pool_0 is in header)
    pub pools_1_2: [Pubkey; 2],  // 64
    pub price_history: [u64; 30], // 240  TWAP ring buffer
    pub admin: Pubkey,            // 32
}

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum MemePriceFeedError {
    #[msg("Price already updated this slot")]
    SlotAlreadyUpdated,
    #[msg("Only admin can update during warm-up (first 30 updates)")]
    WarmupAdminOnly,
    #[msg("Pool address does not match feed PDA")]
    PoolMismatch,
    #[msg("Pool token accounts do not match pool data")]
    PoolTokenMismatch,
    #[msg("Token account not owned by SPL Token Program")]
    InvalidTokenOwner,
    #[msg("Pyth account not owned by Pyth Receiver or Push Oracle")]
    InvalidPythOwner,
    #[msg("Pyth feed ID does not match SOL/USD")]
    InvalidPythFeedId,
    #[msg("Pyth price is not positive")]
    InvalidPythPrice,
    #[msg("Pyth price is stale (>60s)")]
    PythStale,
    #[msg("Pyth confidence interval too wide (>5%)")]
    PythConfidenceTooWide,
    #[msg("Pool reserve is zero")]
    ZeroReserve,
    #[msg("Calculated price is zero")]
    ZeroPrice,
    #[msg("Account data too small for expected read")]
    AccountDataTooSmall,
    #[msg("At least two valid pools required (liquidity/min reserve not met)")]
    InsufficientLiquidity,
    #[msg("Pool account not owned by allowed AMM program")]
    InvalidPoolOwner,
}

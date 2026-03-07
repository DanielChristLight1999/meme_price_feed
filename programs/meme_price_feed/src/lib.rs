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
const TWAP_SIZE: usize = 10;

/// Target scale exponent for TOKEN/USD (consumer expects ×10^6)
const TARGET_EXPONENT: i32 = -6;

// ============================================================================
// Program
// ============================================================================

#[program]
pub mod meme_price_feed {
    use super::*;

    /// Creates a synthetic Pyth-layout feed PDA for a pool.
    /// Seeds: ["feed", pool.key()]. One feed per pool.
    /// Access: permissionless (anyone can create a feed for any pool).
    pub fn initialize_feed(ctx: Context<InitializeFeed>) -> Result<()> {
        let feed = &mut ctx.accounts.feed;
        feed.pool = ctx.accounts.pool.key();
        feed.bump = ctx.bumps.feed;
        feed.admin = ctx.accounts.payer.key();
        feed.exponent = TARGET_EXPONENT;
        // last_slot, history_*, price, publish_time zeroed by init
        Ok(())
    }

    /// Updates the feed: reads pool + Pyth SOL/USD, computes TOKEN/USD ×10^6,
    /// optionally TWAP, writes Pyth layout at offsets 73, 89, 93.
    /// Permissionless after warm-up (first 10 updates admin-only).
    pub fn update_price(ctx: Context<UpdatePrice>) -> Result<()> {
        let feed = &mut ctx.accounts.feed;
        let clock = Clock::get()?;

        require!(feed.last_slot < clock.slot, MemePriceFeedError::SlotAlreadyUpdated);

        if feed.history_count < TWAP_SIZE as u8 {
            require!(
                ctx.accounts.caller.key() == feed.admin,
                MemePriceFeedError::WarmupAdminOnly
            );
        }

        let (price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve) = calculate_price(
            &ctx.accounts.pool,
            &ctx.accounts.pool_base_token,
            &ctx.accounts.pool_quote_token,
            &ctx.accounts.pyth_sol_usd,
            &clock,
            feed.pool,
        )?;

        update_feed_state(
            feed,
            price_e6,
            token_sol_e9,
            sol_usd_e6,
            base_reserve,
            quote_reserve,
            &clock,
        );

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

/// Calculate TOKEN/USD ×10^6 from PumpSwap pool reserves and Pyth SOL/USD.
/// Returns (price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve).
fn calculate_price<'info>(
    pool: &AccountInfo<'info>,
    pool_base_token: &AccountInfo<'info>,
    pool_quote_token: &AccountInfo<'info>,
    pyth_price_feed: &AccountInfo<'info>,
    clock: &Clock,
    expected_pool: Pubkey,
) -> Result<(u64, u64, u64, u64, u64)> {
    require!(pool.key() == expected_pool, MemePriceFeedError::PoolMismatch);
    let pool_data = pool.try_borrow_data()?;
    require!(
        pool_data.len() >= 203,
        MemePriceFeedError::AccountDataTooSmall
    );

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
    require!(base_reserve > 0, MemePriceFeedError::ZeroReserve);
    require!(quote_reserve > 0, MemePriceFeedError::ZeroReserve);

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

    let token_sol_e9 =
        ((quote_reserve as u128) * 1_000_000_000u128 / (base_reserve as u128)) as u64;

    let sol_usd_expo_sum: i32 = 6 + pyth_expo;
    let sol_usd_e6: u64 = if sol_usd_expo_sum >= 0 {
        (pyth_price as u128 * 10u128.pow(sol_usd_expo_sum as u32)) as u64
    } else {
        (pyth_price as u128 / 10u128.pow((-sol_usd_expo_sum) as u32)) as u64
    };

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
    require!(den > 0, MemePriceFeedError::ZeroReserve);
    let price_e6 = (num / den) as u64;
    require!(price_e6 > 0, MemePriceFeedError::ZeroPrice);

    Ok((price_e6, token_sol_e9, sol_usd_e6, base_reserve, quote_reserve))
}

fn update_feed_state(
    feed: &mut Account<SyntheticFeed>,
    price_e6: u64,
    _token_sol_e9: u64,
    _sol_usd_e6: u64,
    _base_reserve: u64,
    _quote_reserve: u64,
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

    /// CHECK: PumpSwap pool; validated on update_price
    pub pool: AccountInfo<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdatePrice<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        mut,
        seeds = [b"feed", feed.pool.as_ref()],
        bump = feed.bump,
    )]
    pub feed: Account<'info, SyntheticFeed>,

    /// CHECK: PumpSwap pool; validated against feed.pool
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
    pub pool: Pubkey,           // 32
    pub bump: u8,               // 1
    pub last_slot: u64,         // 8
    pub history_idx: u8,        // 1
    pub history_count: u8,      // 1
    pub _header_pad: [u8; 22], // 22 -> total 65

    // Pyth layout (when_meme_prediction reads these offsets)
    pub price: i64,     // 8  @ 73  = TOKEN/USD × 10^6
    pub _pad1: [u8; 8], // 8  @ 81
    pub exponent: i32,     // 4  @ 89  = -6
    pub publish_time: i64, // 8  @ 93  = unix timestamp

    pub price_history: [u64; 10], // 80  TWAP ring buffer
    pub admin: Pubkey,            // 32
}

// ============================================================================
// Errors
// ============================================================================

#[error_code]
pub enum MemePriceFeedError {
    #[msg("Price already updated this slot")]
    SlotAlreadyUpdated,
    #[msg("Only admin can update during warm-up (first 10 updates)")]
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
}

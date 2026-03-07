# Meme Price Feed (Synthetic Pyth-Layout Writer)

**Program ID:** `95Hb6hGB5S9j6WeyKN9SDbmiL7mJ64Zhn1uGunLUfyyd`

## Overview

This Solana program computes **TOKEN/USD** from an AMM pool (e.g. PumpSwap TOKEN/SOL) and Pyth’s SOL/USD feed, then writes the result into an account that uses the **same byte layout as a Pyth price account**. A consumer that expects a Pyth price feed (e.g. `when_meme_prediction`) can use this feed PDA as `pyth_price_feed` with no changes: it reads price at offset 73, exponent at 89, and publish_time at 93.

- **No CPI** to percolator or any other protocol.
- **Fully on-chain:** pool reserves + Pyth SOL/USD only.
- **One feed per pool:** PDA seeds `["feed", pool_pubkey]`.
- **Pyth-compatible layout:** TOKEN/USD × 10^6, exponent -6, 60s staleness semantics.

Validation and price math are aligned with the reference program `oracle-price-main` in this repo (pool parsing, Pyth checks, u128 math, optional TWAP).

## How It Works

```
PumpSwap AMM (TOKEN/SOL)     Pyth SOL/USD
         |                            |
         v                            v
  +------------------------------------------+
  |         meme_price_feed program          |
  |  1. Validate pool + token accounts       |
  |  2. Validate Pyth (owner, feed id,       |
  |     staleness ≤60s, confidence ≤5%)      |
  |  3. TOKEN/USD = (quote/base)*SOL/USD      |
  |  4. TWAP over last 10 updates (optional)  |
  |  5. Write Pyth layout into feed PDA      |
  +------------------------------------------+
                    |
                    v
         Feed PDA (Pyth-layout)
         → use as pyth_price_feed in when_meme_prediction
```

## Feed PDA Derivation

- **Seeds:** `["feed", pool_pubkey]`
- **Program ID:** `95Hb6hGB5S9j6WeyKN9SDbmiL7mJ64Zhn1uGunLUfyyd`

Example (TypeScript):

```ts
import { PublicKey } from "@solana/web3.js";

const PROGRAM_ID = new PublicKey("95Hb6hGB5S9j6WeyKN9SDbmiL7mJ64Zhn1uGunLUfyyd");

function getFeedPda(poolPubkey: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("feed"), poolPubkey.toBuffer()],
    PROGRAM_ID
  );
}
```

One feed per pool; the token is implied by the pool (base = token, quote = wSOL for PumpSwap).

## Pyth Layout (Consumer Compatibility)

The feed account is written so that a Pyth-reading consumer sees:

| Offset | Size | Type   | Value / meaning                    |
|--------|------|--------|------------------------------------|
| 73     | 8    | i64 LE | **Price** = TOKEN/USD × 10^6       |
| 89     | 4    | i32 LE | **Exponent** = -6                  |
| 93     | 8    | i64 LE | **Publish time** = `Clock::unix_timestamp` |

- **Scale:** TOKEN/USD × 10^6 (exponent -6).
- **Staleness:** Consumer typically rejects if publish_time is older than 60 seconds; this program updates on-chain so you must call `update_price` at least every ~60s to keep the feed valid.

Min account length for this layout is **101 bytes**; the program uses a larger account (222+ bytes) to store pool, TWAP state, and admin.

## Instructions

### `initialize_feed`

Creates the synthetic feed PDA for a given pool. Permissionless (anyone can create a feed for any pool).

| Account    | Type     | Description                          |
|-----------|----------|--------------------------------------|
| `payer`   | Signer, Mut | Pays rent; becomes `admin` for warm-up |
| `feed`    | PDA, Mut   | Seeds: `["feed", pool.key()]`        |
| `pool`    | Readonly   | PumpSwap pool (validated on update) |
| `system_program` | Program | System Program                  |

### `update_price`

Reads pool + Pyth SOL/USD, computes TOKEN/USD (with TWAP), writes Pyth layout into the feed. **One update per slot**; first **10 updates** are admin-only (warm-up), then permissionless.

| Account           | Type     | Description                          |
|-------------------|----------|--------------------------------------|
| `caller`          | Signer, Mut | Must be admin during warm-up        |
| `feed`            | PDA, Mut   | Feed to update                       |
| `pool`            | Readonly   | PumpSwap pool (must match `feed.pool`) |
| `pool_base_token` | Readonly   | Pool base token ATA                  |
| `pool_quote_token`| Readonly   | Pool quote token ATA (e.g. wSOL)     |
| `pyth_sol_usd`    | Readonly   | Pyth SOL/USD price feed account     |

## Validation (aligned with oracle-price-main)

- **Pool:** Pool key must match feed’s stored `pool`; base/quote token accounts must match pool data at offsets 139 and 171; token accounts must be owned by SPL Token or Token-2022.
- **Pyth:** Account owner must be Pyth Receiver or Pyth Push Oracle; feed ID must be SOL/USD; staleness ≤ 60s; confidence × 20 &lt; price (≤5%).
- **Rate limit:** At most one `update_price` per feed per slot.
- **Warm-up:** First 10 updates require `caller == feed.admin`.

## Using This Feed in when_meme_prediction

Pass the **feed PDA** as the `pyth_price_feed` account. The prediction market will read:

- `price` at byte 73 (i64 LE), scale TOKEN/USD × 10^6  
- `exponent` at byte 89 (i32 LE) = -6  
- `publish_time` at byte 93 (i64 LE)

No changes are required in when_meme_prediction; the account is Pyth-layout compatible.

## Build & Test

```bash
anchor build
anchor test
```

## Reference

- **oracle-price-main** in this repo: same pool/Pyth validation and `calculate_price`-style math; used as reference only (no CPI dependency).
- **when_meme_prediction:** consumer that expects Pyth layout at 73, 89, 93; 10^6 scale; 60s staleness.

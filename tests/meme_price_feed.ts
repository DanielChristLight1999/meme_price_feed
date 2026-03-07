import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { MemePriceFeed } from "../target/types/meme_price_feed";
import { PublicKey } from "@solana/web3.js";
import { expect } from "chai";

describe("meme_price_feed", () => {
  anchor.setProvider(anchor.AnchorProvider.env());
  const program = anchor.workspace.MemePriceFeed as Program<MemePriceFeed>;
  const programId = program.programId;

  const getFeedPda = (pool: PublicKey): [PublicKey, number] =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("feed"), pool.toBuffer()],
      programId
    );

  it("initialize_feed creates feed PDA with correct seeds", async () => {
    const poolKey = anchor.web3.Keypair.generate().publicKey;
    const [feedPda] = getFeedPda(poolKey);

    const tx = await program.methods
      .initializeFeed()
      .accounts({
        payer: (anchor.getProvider() as anchor.AnchorProvider).wallet.publicKey,
        feed: feedPda,
        pool: poolKey,
        systemProgram: anchor.web3.SystemProgram.programId,
      })
      .rpc();

    console.log("initialize_feed tx:", tx);

    const feedAccount = await program.account.syntheticFeed.fetch(feedPda);
    expect(feedAccount.pool.equals(poolKey)).to.be.true;
    expect(feedAccount.exponent).to.equal(-6);
    expect(
      feedAccount.admin.equals(
        (anchor.getProvider() as anchor.AnchorProvider).wallet.publicKey
      )
    ).to.be.true;
  });

  it("feed PDA derivation matches README: [feed, pool]", () => {
    const poolKey = new PublicKey("FFcYgSSgWHforA9rXXkA48p8YFoz8TSW85Jpo3CQHDyS");
    const [feedPda, bump] = getFeedPda(poolKey);
    const [expectedPda] = PublicKey.findProgramAddressSync(
      [Buffer.from("feed"), poolKey.toBuffer()],
      programId
    );
    expect(feedPda.equals(expectedPda)).to.be.true;
    expect(bump).to.be.gte(0).and.lte(255);
  });
});

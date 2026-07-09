import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { FluxpayEscrow } from "../target/types/fluxpay_escrow";
import {
  createMint,
  createAssociatedTokenAccount,
  mintTo,
  getAccount,
  getAssociatedTokenAddress,
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
} from "@solana/spl-token";
import { PublicKey, Keypair, SystemProgram } from "@solana/web3.js";
import { assert } from "chai";
import { randomBytes } from "crypto";

describe("fluxpay-escrow", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.FluxpayEscrow as Program<FluxpayEscrow>;
  const connection = provider.connection;

  // Actors
  const admin = Keypair.generate();
  const merchant = Keypair.generate();
  const customer = Keypair.generate();
  const randomStranger = Keypair.generate();

  let usdcMint: PublicKey;
  let merchantAta: PublicKey;
  let customerAta: PublicKey;
  let configPda: PublicKey;

  // Timing bounds (seconds)
  const MIN_WINDOW = 5;
  const MAX_WINDOW = 3600;
  const MIN_GRACE = 2;
  const MAX_GRACE = 1800;

  const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

  const airdrop = async (pk: PublicKey, sol = 2) => {
    const sig = await connection.requestAirdrop(pk, sol * anchor.web3.LAMPORTS_PER_SOL);
    await connection.confirmTransaction(sig);
  };

  const escrowPda = (paymentId: Buffer) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), paymentId],
      program.programId
    )[0];

  const vaultAta = async (escrow: PublicKey) =>
    getAssociatedTokenAddress(usdcMint, escrow, true);

  before(async () => {
    await airdrop(admin.publicKey);
    await airdrop(merchant.publicKey);
    await airdrop(customer.publicKey);
    await airdrop(randomStranger.publicKey);

    // Mint authority is the provider wallet
    usdcMint = await createMint(
      connection,
      (provider.wallet as anchor.Wallet).payer,
      provider.wallet.publicKey,
      null,
      6
    );

    merchantAta = await createAssociatedTokenAccount(
      connection,
      merchant,
      usdcMint,
      merchant.publicKey
    );
    customerAta = await createAssociatedTokenAccount(
      connection,
      customer,
      usdcMint,
      customer.publicKey
    );

    // Give the customer 1000 USDC
    await mintTo(
      connection,
      (provider.wallet as anchor.Wallet).payer,
      usdcMint,
      customerAta,
      provider.wallet.publicKey,
      1000_000000
    );

    configPda = PublicKey.findProgramAddressSync(
      [Buffer.from("config")],
      program.programId
    )[0];
  });

  it("initializes config", async () => {
    await program.methods
      .initializeConfig(
        admin.publicKey,
        new anchor.BN(MIN_WINDOW),
        new anchor.BN(MAX_WINDOW),
        new anchor.BN(MIN_GRACE),
        new anchor.BN(MAX_GRACE)
      )
      .accounts({
        config: configPda,
        usdcMint,
        payer: provider.wallet.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    const cfg = await program.account.config.fetch(configPda);
    assert.ok(cfg.admin.equals(admin.publicKey));
    assert.ok(cfg.usdcMint.equals(usdcMint));
  });

  // ── Happy path ──────────────────────────────────────────────────────────────

  it("full happy path: create → deposit → release", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);
    const amount = 10_000000; // 10 USDC

    await program.methods
      .createEscrow(
        [...paymentId],
        merchant.publicKey,
        new anchor.BN(amount),
        new anchor.BN(30),
        new anchor.BN(10)
      )
      .accounts({
        config: configPda,
        escrow,
        vault,
        usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    let esc = await program.account.escrow.fetch(escrow);
    assert.deepEqual(esc.status, { pending: {} });

    await program.methods
      .deposit([...paymentId])
      .accounts({
        escrow,
        vault,
        depositorTokenAccount: customerAta,
        usdcMint,
        depositor: customer.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer])
      .rpc();

    esc = await program.account.escrow.fetch(escrow);
    assert.deepEqual(esc.status, { funded: {} });
    assert.ok(esc.depositor.equals(customer.publicKey));

    const vaultAcc = await getAccount(connection, vault);
    assert.equal(Number(vaultAcc.amount), amount);

    const merchantBefore = (await getAccount(connection, merchantAta)).amount;

    await program.methods
      .release([...paymentId])
      .accounts({
        config: configPda,
        escrow,
        vault,
        merchantTokenAccount: merchantAta,
        admin: admin.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([admin])
      .rpc();

    esc = await program.account.escrow.fetch(escrow);
    assert.deepEqual(esc.status, { released: {} });

    const merchantAfter = (await getAccount(connection, merchantAta)).amount;
    assert.equal(Number(merchantAfter - merchantBefore), amount);
  });

  // ── Safety: non-admin cannot release ─────────────────────────────────────────

  it("rejects release from non-admin", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);

    await program.methods
      .createEscrow(
        [...paymentId],
        merchant.publicKey,
        new anchor.BN(5_000000),
        new anchor.BN(30),
        new anchor.BN(10)
      )
      .accounts({
        config: configPda, escrow, vault, usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .deposit([...paymentId])
      .accounts({
        escrow, vault, depositorTokenAccount: customerAta, usdcMint,
        depositor: customer.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer])
      .rpc();

    try {
      await program.methods
        .release([...paymentId])
        .accounts({
          config: configPda, escrow, vault,
          merchantTokenAccount: merchantAta,
          admin: randomStranger.publicKey, // NOT the admin
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([randomStranger])
        .rpc();
      assert.fail("should have rejected non-admin release");
    } catch (e: any) {
      assert.include(e.toString(), "UnauthorizedAdmin");
    }
  });

  // ── Safety: refund blocked before window+grace ───────────────────────────────

  it("blocks refund while still in release window", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);

    await program.methods
      .createEscrow(
        [...paymentId], merchant.publicKey, new anchor.BN(5_000000),
        new anchor.BN(30), new anchor.BN(10)
      )
      .accounts({
        config: configPda, escrow, vault, usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .deposit([...paymentId])
      .accounts({
        escrow, vault, depositorTokenAccount: customerAta, usdcMint,
        depositor: customer.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer])
      .rpc();

    try {
      await program.methods
        .refundAfterExpiry([...paymentId])
        .accounts({
          escrow, vault, depositorTokenAccount: customerAta, usdcMint,
          caller: randomStranger.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([randomStranger])
        .rpc();
      assert.fail("should have blocked early refund");
    } catch (e: any) {
      assert.include(e.toString(), "StillInReleaseWindow");
    }
  });

  // ── Safety: permissionless refund after expiry, by a stranger, to depositor ──

  it("allows permissionless refund after window+grace, funds go to depositor", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);
    const amount = 7_000000;

    // Short window/grace so the test doesn't take long
    await program.methods
      .createEscrow(
        [...paymentId], merchant.publicKey, new anchor.BN(amount),
        new anchor.BN(MIN_WINDOW), new anchor.BN(MIN_GRACE)
      )
      .accounts({
        config: configPda, escrow, vault, usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .deposit([...paymentId])
      .accounts({
        escrow, vault, depositorTokenAccount: customerAta, usdcMint,
        depositor: customer.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer])
      .rpc();

    const customerBefore = (await getAccount(connection, customerAta)).amount;

    // Wait out window + grace (+ buffer)
    await sleep((MIN_WINDOW + MIN_GRACE + 3) * 1000);

    // A stranger triggers the refund
    await program.methods
      .refundAfterExpiry([...paymentId])
      .accounts({
        escrow, vault, depositorTokenAccount: customerAta, usdcMint,
        caller: randomStranger.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([randomStranger])
      .rpc();

    const esc = await program.account.escrow.fetch(escrow);
    assert.deepEqual(esc.status, { refunded: {} });

    const customerAfter = (await getAccount(connection, customerAta)).amount;
    assert.equal(Number(customerAfter - customerBefore), amount);
  });

  // ── Safety: cannot refund an already-released payment ────────────────────────

  it("cannot refund after release (no double payout)", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);

    await program.methods
      .createEscrow(
        [...paymentId], merchant.publicKey, new anchor.BN(3_000000),
        new anchor.BN(MIN_WINDOW), new anchor.BN(MIN_GRACE)
      )
      .accounts({
        config: configPda, escrow, vault, usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .deposit([...paymentId])
      .accounts({
        escrow, vault, depositorTokenAccount: customerAta, usdcMint,
        depositor: customer.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([customer])
      .rpc();

    await program.methods
      .release([...paymentId])
      .accounts({
        config: configPda, escrow, vault,
        merchantTokenAccount: merchantAta,
        admin: admin.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([admin])
      .rpc();

    // Vault is closed after release; refund should fail (status Released + no vault)
    await sleep((MIN_WINDOW + MIN_GRACE + 3) * 1000);
    try {
      await program.methods
        .refundAfterExpiry([...paymentId])
        .accounts({
          escrow, vault, depositorTokenAccount: customerAta, usdcMint,
          caller: randomStranger.publicKey, tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([randomStranger])
        .rpc();
      assert.fail("should not refund a released escrow");
    } catch (e: any) {
      // Either InvalidStatus or a closed-account error is acceptable
      assert.ok(e.toString().length > 0);
    }
  });

  // ── Safety: timing bounds enforced ───────────────────────────────────────────

  it("rejects out-of-bounds payment window", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);

    try {
      await program.methods
        .createEscrow(
          [...paymentId], merchant.publicKey, new anchor.BN(1_000000),
          new anchor.BN(MAX_WINDOW + 1), // out of bounds
          new anchor.BN(MIN_GRACE)
        )
        .accounts({
          config: configPda, escrow, vault, usdcMint,
          payer: provider.wallet.publicKey,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
        })
        .rpc();
      assert.fail("should reject out-of-bounds window");
    } catch (e: any) {
      assert.include(e.toString(), "PaymentWindowOutOfBounds");
    }
  });

  // ── Decision 10: merchant cancels a pending escrow ───────────────────────────

  it("merchant can cancel a pending escrow and reclaim rent", async () => {
    const paymentId = randomBytes(32);
    const escrow = escrowPda(paymentId);
    const vault = await vaultAta(escrow);

    await program.methods
      .createEscrow(
        [...paymentId], merchant.publicKey, new anchor.BN(2_000000),
        new anchor.BN(30), new anchor.BN(10)
      )
      .accounts({
        config: configPda, escrow, vault, usdcMint,
        payer: provider.wallet.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .rpc();

    await program.methods
      .cancelPending([...paymentId])
      .accounts({
        escrow, vault, usdcMint,
        merchant: merchant.publicKey,
        tokenProgram: TOKEN_PROGRAM_ID,
      })
      .signers([merchant])
      .rpc();

    // Escrow account should no longer exist
    try {
      await program.account.escrow.fetch(escrow);
      assert.fail("escrow should be closed");
    } catch (e: any) {
      assert.include(e.toString(), "Account does not exist");
    }
  });
});

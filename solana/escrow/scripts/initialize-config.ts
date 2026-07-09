/**
 * One-time script: initialize the Config account for the FluxPay escrow
 * program on Devnet.
 *
 * Run once, after `anchor deploy --provider.cluster devnet`. Re-running this
 * against an already-initialized Config will fail (the `init` constraint on
 * the Config PDA rejects re-initialization) — that's expected and safe.
 *
 * Usage:
 *   cd solana/escrow
 *   npx ts-node scripts/initialize-config.ts
 */

import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram, Connection, clusterApiUrl } from "@solana/web3.js";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";

// ── Config for this run ─────────────────────────────────────────────────────

// Admin = your current Solana CLI wallet (release authority for the program).
// Loaded from the default CLI keypair path, same one `solana address` shows.
const WALLET_PATH = path.join(os.homedir(), ".config/solana/id.json");

// Devnet USDC mint already used by the fluxpay backend.
const USDC_MINT = new PublicKey("Gh9ZwEmdLJ8DscKNTkTqPbNwLNNBjuSzaG9Vp2KGtKJr");

// Sensible default timing bounds (seconds).
const MIN_PAYMENT_WINDOW = 5 * 60;        // 5 minutes
const MAX_PAYMENT_WINDOW = 24 * 60 * 60;  // 24 hours
const MIN_GRACE_PERIOD = 2 * 60;          // 2 minutes
const MAX_GRACE_PERIOD = 60 * 60;         // 1 hour

async function main() {
  const connection = new Connection(clusterApiUrl("devnet"), "confirmed");

  const walletKeypair = anchor.web3.Keypair.fromSecretKey(
    Buffer.from(JSON.parse(fs.readFileSync(WALLET_PATH, "utf-8")))
  );
  const wallet = new anchor.Wallet(walletKeypair);

  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);

  // Load the IDL generated earlier by `anchor idl build`.
  const idlPath = path.join(__dirname, "..", "target", "idl", "fluxpay_escrow.json");
  const idl = JSON.parse(fs.readFileSync(idlPath, "utf-8"));

  // Anchor 0.31.x reads the program ID from idl.address (embedded during
  // `anchor idl build`), and the Program constructor is (idl, provider) —
  // no separate programId argument like in 0.29/0.30.
  const programId = new PublicKey(idl.address ?? "HvmBzCdbAgUN1j1WTxBJrdYXTPhrgrnaHY7ZfB17hpVN");
  const program = new Program(idl as anchor.Idl, provider) as Program<any>;

  const [configPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("config")],
    programId
  );

  console.log("Admin (this wallet):", wallet.publicKey.toBase58());
  console.log("USDC mint:", USDC_MINT.toBase58());
  console.log("Config PDA:", configPda.toBase58());
  console.log("Payment window bounds:", MIN_PAYMENT_WINDOW, "-", MAX_PAYMENT_WINDOW, "seconds");
  console.log("Grace period bounds:", MIN_GRACE_PERIOD, "-", MAX_GRACE_PERIOD, "seconds");

  // Guard: if Config already exists, don't try again — report its current
  // state instead of failing with a confusing on-chain error.
  const existing = await connection.getAccountInfo(configPda);
  if (existing) {
    console.log("\n⚠️  Config already initialized at this address. Fetching current state...");
    const cfg = await (program.account as any).config.fetch(configPda);
    console.log(cfg);
    return;
  }

  const txSig = await (program.methods as any)
    .initializeConfig(
      wallet.publicKey,
      new anchor.BN(MIN_PAYMENT_WINDOW),
      new anchor.BN(MAX_PAYMENT_WINDOW),
      new anchor.BN(MIN_GRACE_PERIOD),
      new anchor.BN(MAX_GRACE_PERIOD)
    )
    .accounts({
      config: configPda,
      usdcMint: USDC_MINT,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("\n✅ Config initialized.");
  console.log("Transaction:", txSig);
  console.log(`View on Solscan: https://explorer.solana.com/tx/${txSig}?cluster=devnet`);

  const cfg = await (program.account as any).config.fetch(configPda);
  console.log("\nOn-chain Config state:");
  console.log(cfg);
}

main().catch((err) => {
  console.error("Failed to initialize config:", err);
  process.exit(1);
});
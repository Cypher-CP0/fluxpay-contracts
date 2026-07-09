# FluxPay Escrow Program (V2-minimal)

On-chain Solana escrow that holds a payment's USDC until it is either **released** to the merchant (admin-confirmed) or **refunded** to the customer (permissionless, after expiry). Replaces V1's backend-custodied deposit-wallet model so customer funds can never be trapped by, or stolen through, the FluxPay backend.

See `../../EscrowDesign.md` (at the repo root) for the full finalized design and the rationale behind every decision.

## What's implemented in this pass (V2-minimal)

Core state machine and the decisions that define its safety:

- **Admin-controlled release** — only the `Config.admin` key can pay the merchant
- **Permissionless refund after expiry** — anyone can trigger it once the window + grace passes, but funds only ever go to the recorded depositor
- **Per-escrow immutable timing** — `payment_window` and `grace_period` are set per escrow, bounded by `Config`, and can't be changed after creation
- **Locked destinations** — release → recorded merchant; refund → actual deposit signer. Neither is caller-chosen.
- **Status enum as single source of truth** — `Pending → Funded → Released | Refunded`, atomic transitions
- **One escrow per payment** — isolated PDA + vault, no shared balance
- **Merchant cancel while Pending** — reclaims rent on unfunded escrows

## Deliberately deferred (documented, not yet built)

These are in the design but intentionally left for a second pass so the core state machine can be proven first:

- **Merchant invoice co-signing** (Decision 7) — needs a delegated-signing UX so merchants aren't signing every invoice by hand
- **Multisig admin** (Decision 11) — single admin key is fine for Devnet/MVP; multisig is a Mainnet requirement
- **Unguessable payment IDs** (Decision 12) — enforced off-chain by the backend for now

Do **not** deploy this to Mainnet with real funds until the deferred items and the open questions in `EscrowDesign.md` §9 (especially upgrade-authority policy) are resolved.

## Prerequisites

- Rust + `rustup`
- Solana CLI (`solana --version`)
- Anchor via `avm` (`anchor --version`, targeting 0.31.1)
- Node + Yarn

## Build

```bash
anchor build
```

This compiles the program and generates the IDL + TypeScript types at `target/types/fluxpay_escrow.ts` used by the tests.

## Test

Runs against a local validator automatically:

```bash
anchor test
```

The suite covers: happy path (create → deposit → release), non-admin release rejection, early-refund rejection, permissionless refund-to-depositor after expiry, no-double-payout after release, timing-bounds enforcement, and merchant cancel of a pending escrow.

Note: a couple of tests intentionally `sleep` through the (short) window + grace to exercise the refund path, so the suite takes ~30–40s.

## Deploy to Devnet

```bash
solana config set --url devnet
anchor build
anchor deploy --provider.cluster devnet
```

After deploying, update the `declare_id!` in `lib.rs` and the `[programs.devnet]` entry in `Anchor.toml` with the deployed program ID, then rebuild.

## Program layout

```
programs/fluxpay-escrow/src/lib.rs   # the whole program: instructions, accounts, events, errors
tests/fluxpay-escrow.ts              # full test suite incl. safety/attack cases
Anchor.toml                          # program IDs + provider config
```

## Instruction summary

| Instruction | Caller | When |
|---|---|---|
| `initialize_config` | deployer | once |
| `create_escrow` | backend | per payment |
| `deposit` | customer | while Pending |
| `release` | admin | Funded, within window + grace |
| `refund_after_expiry` | anyone | Funded, after window + grace |
| `cancel_pending` | merchant | while Pending |

## Next steps after this program is proven

Per the agreed plan, once the escrow is tested and deployed to Devnet, backend hardening follows: webhook idempotency + reconciliation, dead-letter queue, restart-safe payment processing, hot-standby signer, and structured logging/alerting.

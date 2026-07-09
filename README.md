# FluxPay Contracts

On-chain programs for FluxPay's payment infrastructure. Monorepo, organized by chain.

## Layout

```
fluxpay-contracts/
├── solana/
│   └── escrow/           # V2 escrow program (Anchor). Holds a payment's USDC
│                         # until released to merchant or refunded to customer.
├── evm/                  # (future) V3 cross-chain settlement — Ethereum/Base
├── EscrowDesign.md       # finalized escrow design + rationale for every decision
└── README.md
```

Each program under a chain directory is a self-contained project with its own build/test setup. See the README inside each (e.g. `solana/escrow/README.md`) for chain-specific build and deploy instructions.

## Programs

| Program | Chain | Status | Purpose |
|---|---|---|---|
| `escrow` | Solana | V2-minimal, Devnet-bound | Safe custody of payment funds between deposit and settlement |
| cross-chain settlement | Ethereum/Base | planned (V3) | Accept payments from EVM chains, settle to Solana |

## Design docs

`EscrowDesign.md` at the repo root is the finalized design for the escrow program — read it before touching `solana/escrow/`, it explains not just what the program does but why every decision was made (and what was rejected).

## Security posture

These programs handle real money. Nothing here should reach Mainnet with real funds until:

- The deferred design items are implemented (merchant co-signing, multisig admin, on-chain-enforced unguessable IDs)
- The upgrade-authority policy is decided (see `EscrowDesign.md` §9)
- External review / audit is complete

The Solana escrow is currently **Devnet-only** and intended for integration testing against the FluxPay backend.

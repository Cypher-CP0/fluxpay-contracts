use anchor_lang::prelude::*;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer, CloseAccount},
};

declare_id!("HvmBzCdbAgUN1j1WTxBJrdYXTPhrgrnaHY7ZfB17hpVN");
// ─────────────────────────────────────────────────────────────────────────────
// FluxPay Escrow — V2-minimal
//
// Implements the finalized design's core state machine:
//   Decision 1  — admin-controlled release (happy path)
//   Decision 2  — permissionless refund after expiry (failure path)
//   Decision 3  — per-escrow immutable timing, bounded by Config (bounds enforced)
//   Decision 4  — destinations locked on-chain (merchant / depositor), never caller-chosen
//   Decision 5  — status enum as single source of truth, atomic transitions
//   Decision 6  — depositor + timing stored from day one
//   Decision 8  — one escrow per payment (no shared vault)
//   Decision 10 — merchant cancellation while Pending (rent reclamation)
//
// Deliberately deferred to a later pass (documented in EscrowDesign.md):
//   Decision 7  — merchant co-signs invoice  (needs delegated-signing UX design)
//   Decision 11 — multisig admin             (single admin key for Devnet/MVP)
//   Decision 12 — unguessable payment_id      (enforced off-chain by backend for now)
//
// NOTE on expiry semantics: `expiry` is computed at DEPOSIT time
// (deposit_time + payment_window), i.e. the payment window measures
// time-to-confirm-after-deposit. Invoice-level expiry (time-to-pay-at-all)
// is handled off-chain by the backend for now; see EscrowDesign open question.
// ─────────────────────────────────────────────────────────────────────────────

#[program]
pub mod fluxpay_escrow {
    use super::*;

    /// One-time program setup. Sets the release authority (admin), the accepted
    /// stablecoin mint, and the bounds within which per-escrow timing values must fall.
    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        admin: Pubkey,
        min_payment_window: i64,
        max_payment_window: i64,
        min_grace_period: i64,
        max_grace_period: i64,
    ) -> Result<()> {
        require!(min_payment_window > 0, EscrowError::InvalidBounds);
        require!(max_payment_window >= min_payment_window, EscrowError::InvalidBounds);
        require!(min_grace_period > 0, EscrowError::InvalidBounds);
        require!(max_grace_period >= min_grace_period, EscrowError::InvalidBounds);

        let config = &mut ctx.accounts.config;
        config.admin = admin;
        config.usdc_mint = ctx.accounts.usdc_mint.key();
        config.min_payment_window = min_payment_window;
        config.max_payment_window = max_payment_window;
        config.min_grace_period = min_grace_period;
        config.max_grace_period = max_grace_period;
        config.bump = ctx.bumps.config;

        emit!(ConfigInitialized {
            admin,
            usdc_mint: config.usdc_mint,
        });
        Ok(())
    }

    /// Creates a new escrow for a payment. Called by the backend. Creates the
    /// escrow PDA and its owned token account (vault). No funds move here.
    ///
    /// `payment_id` is a 32-byte identifier; the backend is responsible for
    /// making it unguessable (Decision 12, enforced off-chain in this pass).
    pub fn create_escrow(
        ctx: Context<CreateEscrow>,
        payment_id: [u8; 32],
        merchant: Pubkey,
        amount: u64,
        payment_window: i64,
        grace_period: i64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;

        require!(amount > 0, EscrowError::InvalidAmount);
        require!(
            payment_window >= config.min_payment_window
                && payment_window <= config.max_payment_window,
            EscrowError::PaymentWindowOutOfBounds
        );
        require!(
            grace_period >= config.min_grace_period
                && grace_period <= config.max_grace_period,
            EscrowError::GracePeriodOutOfBounds
        );

        let escrow = &mut ctx.accounts.escrow;
        escrow.payment_id = payment_id;
        escrow.merchant = merchant;
        escrow.depositor = Pubkey::default(); // set at deposit
        escrow.amount = amount;
        escrow.payment_window = payment_window;
        escrow.grace_period = grace_period;
        escrow.expiry = 0; // computed at deposit
        escrow.status = EscrowStatus::Pending;
        escrow.bump = ctx.bumps.escrow;

        emit!(EscrowCreated {
            payment_id,
            merchant,
            amount,
        });
        Ok(())
    }

    /// Customer deposits USDC into the escrow vault. Records the depositor as the
    /// transaction signer (Decision 4 — refund destination is the actual signer,
    /// never caller-supplied), computes expiry, and flips status to Funded.
    pub fn deposit(ctx: Context<Deposit>, _payment_id: [u8; 32]) -> Result<()> {
        // Read what we need without holding a mutable borrow across the CPI.
        let (status, amount, payment_window, payment_id) = {
            let escrow = &ctx.accounts.escrow;
            (escrow.status, escrow.amount, escrow.payment_window, escrow.payment_id)
        };
        require!(status == EscrowStatus::Pending, EscrowError::InvalidStatus);

        // Pull USDC from the depositor's token account into the escrow vault.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.depositor_token_account.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
        )?;

        let now = Clock::get()?.unix_timestamp;
        let expiry = now
            .checked_add(payment_window)
            .ok_or(EscrowError::MathOverflow)?;
        let depositor = ctx.accounts.depositor.key();

        // Now take the mutable borrow and write the state transition.
        let escrow = &mut ctx.accounts.escrow;
        escrow.depositor = depositor;
        escrow.expiry = expiry;
        escrow.status = EscrowStatus::Funded;

        emit!(EscrowFunded {
            payment_id,
            depositor,
            expiry,
        });
        Ok(())
    }

    /// Admin releases funds to the merchant. Allowed only while Funded and within
    /// the release window (payment window + grace period). Destination is fixed to
    /// the merchant recorded at creation (Decision 4).
    pub fn release(ctx: Context<Release>, payment_id: [u8; 32]) -> Result<()> {
        // Copy everything we need into owned locals up front, so we don't hold
        // an immutable borrow of `escrow` across the later mutable status write.
        let (status, expiry, grace_period, amount, bump, merchant) = {
            let escrow = &ctx.accounts.escrow;
            (
                escrow.status,
                escrow.expiry,
                escrow.grace_period,
                escrow.amount,
                escrow.bump,
                escrow.merchant,
            )
        };

        require!(status == EscrowStatus::Funded, EscrowError::InvalidStatus);

        let now = Clock::get()?.unix_timestamp;
        let release_deadline = expiry
            .checked_add(grace_period)
            .ok_or(EscrowError::MathOverflow)?;
        require!(now <= release_deadline, EscrowError::ReleaseWindowPassed);

        // Vault is owned by the escrow PDA; sign the transfer with the PDA seeds.
        let seeds: &[&[u8]] = &[b"escrow", payment_id.as_ref(), &[bump]];
        let signer: &[&[&[u8]]] = &[seeds];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.merchant_token_account.to_account_info(),
                    authority: ctx.accounts.escrow.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;

        // Flip status AFTER the transfer CPI — the whole instruction is atomic,
        // so either both happen or neither does.
        ctx.accounts.escrow.status = EscrowStatus::Released;

        // Close the now-empty vault, returning its rent to the merchant.
        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.merchant_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            signer,
        ))?;

        emit!(EscrowReleased {
            payment_id,
            merchant,
            amount,
        });
        Ok(())
    }

    /// Permissionless refund after the release window closes. Anyone may call it,
    /// but funds can only go to the recorded depositor (Decision 2 + 4).
    pub fn refund_after_expiry(ctx: Context<RefundAfterExpiry>, payment_id: [u8; 32]) -> Result<()> {
        // Copy needed values into owned locals before the later mutable borrow.
        let (status, expiry, grace_period, amount, bump, depositor) = {
            let escrow = &ctx.accounts.escrow;
            (
                escrow.status,
                escrow.expiry,
                escrow.grace_period,
                escrow.amount,
                escrow.bump,
                escrow.depositor,
            )
        };

        require!(status == EscrowStatus::Funded, EscrowError::InvalidStatus);

        let now = Clock::get()?.unix_timestamp;
        let release_deadline = expiry
            .checked_add(grace_period)
            .ok_or(EscrowError::MathOverflow)?;
        require!(now > release_deadline, EscrowError::StillInReleaseWindow);

        // The provided depositor token account must belong to the recorded depositor.
        require!(
            ctx.accounts.depositor_token_account.owner == depositor,
            EscrowError::WrongDepositor
        );

        let seeds: &[&[u8]] = &[b"escrow", payment_id.as_ref(), &[bump]];
        let signer: &[&[&[u8]]] = &[seeds];

        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.vault.to_account_info(),
                    to: ctx.accounts.depositor_token_account.to_account_info(),
                    authority: ctx.accounts.escrow.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;

        ctx.accounts.escrow.status = EscrowStatus::Refunded;

        // Refund rent for the vault to whoever the depositor was.
        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.depositor_token_account.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            signer,
        ))?;

        emit!(EscrowRefunded {
            payment_id,
            depositor,
            amount,
        });
        Ok(())
    }

    /// Merchant cancels a still-Pending (unfunded) escrow, reclaiming base rent.
    /// Safe by construction: a Pending escrow holds no funds (Decision 10).
    pub fn cancel_pending(ctx: Context<CancelPending>, payment_id: [u8; 32]) -> Result<()> {
        let (status, merchant, bump) = {
            let escrow = &ctx.accounts.escrow;
            (escrow.status, escrow.merchant, escrow.bump)
        };

        require!(status == EscrowStatus::Pending, EscrowError::InvalidStatus);
        require!(
            ctx.accounts.merchant.key() == merchant,
            EscrowError::UnauthorizedMerchant
        );

        // Close the (empty) vault ATA first, returning its rent to the merchant.
        // The escrow PDA is the vault authority, so we sign with its seeds.
        // The escrow account itself is closed afterwards by the `close = merchant`
        // constraint once the instruction body returns.
        let seeds: &[&[u8]] = &[b"escrow", payment_id.as_ref(), &[bump]];
        let signer: &[&[&[u8]]] = &[seeds];

        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.merchant.to_account_info(),
                authority: ctx.accounts.escrow.to_account_info(),
            },
            signer,
        ))?;

        emit!(EscrowCancelled { payment_id });
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Accounts
// ─────────────────────────────────────────────────────────────────────────────

#[account]
pub struct Config {
    pub admin: Pubkey,
    pub usdc_mint: Pubkey,
    pub min_payment_window: i64,
    pub max_payment_window: i64,
    pub min_grace_period: i64,
    pub max_grace_period: i64,
    pub bump: u8,
}

impl Config {
    // discriminator(8) + admin(32) + mint(32) + 4*i64(32) + bump(1) = 105
    pub const LEN: usize = 8 + 32 + 32 + (4 * 8) + 1;
}

#[account]
pub struct Escrow {
    pub payment_id: [u8; 32],
    pub depositor: Pubkey,
    pub merchant: Pubkey,
    pub amount: u64,
    pub payment_window: i64,
    pub grace_period: i64,
    pub expiry: i64,
    pub status: EscrowStatus,
    pub bump: u8,
}

impl Escrow {
    // discriminator(8) + payment_id(32) + depositor(32) + merchant(32)
    // + amount(8) + payment_window(8) + grace_period(8) + expiry(8)
    // + status(1) + bump(1)
    pub const LEN: usize = 8 + 32 + 32 + 32 + 8 + 8 + 8 + 8 + 1 + 1;
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum EscrowStatus {
    Pending,
    Funded,
    Released,
    Refunded,
}

// ─────────────────────────────────────────────────────────────────────────────
// Instruction contexts
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(
        init,
        payer = payer,
        space = Config::LEN,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, Config>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(payment_id: [u8; 32])]
pub struct CreateEscrow<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = payer,
        space = Escrow::LEN,
        seeds = [b"escrow", payment_id.as_ref()],
        bump
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        init,
        payer = payer,
        associated_token::mint = usdc_mint,
        associated_token::authority = escrow,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(address = config.usdc_mint)]
    pub usdc_mint: Account<'info, Mint>,

    #[account(mut)]
    pub payer: Signer<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(payment_id: [u8; 32])]
pub struct Deposit<'info> {
    #[account(
        mut,
        seeds = [b"escrow", payment_id.as_ref()],
        bump = escrow.bump
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = escrow,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = depositor_token_account.mint == usdc_mint.key() @ EscrowError::WrongMint,
        constraint = depositor_token_account.owner == depositor.key() @ EscrowError::WrongDepositor,
    )]
    pub depositor_token_account: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(mut)]
    pub depositor: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(payment_id: [u8; 32])]
pub struct Release<'info> {
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [b"escrow", payment_id.as_ref()],
        bump = escrow.bump,
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        associated_token::mint = config.usdc_mint,
        associated_token::authority = escrow,
    )]
    pub vault: Account<'info, TokenAccount>,

    // Constraint lives here (declared after `escrow`) so Anchor can resolve
    // the reference to `escrow` in declaration order. Ensures funds can only
    // go to the merchant recorded at creation (Decision 4).
    #[account(
        mut,
        constraint = merchant_token_account.owner == escrow.merchant @ EscrowError::WrongMerchant,
    )]
    pub merchant_token_account: Account<'info, TokenAccount>,

    // Only the admin recorded in Config may release.
    #[account(constraint = admin.key() == config.admin @ EscrowError::UnauthorizedAdmin)]
    pub admin: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(payment_id: [u8; 32])]
pub struct RefundAfterExpiry<'info> {
    #[account(
        mut,
        seeds = [b"escrow", payment_id.as_ref()],
        bump = escrow.bump
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = escrow,
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub depositor_token_account: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,

    // Permissionless: anyone can pay the tx fee to trigger the refund.
    pub caller: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
#[instruction(payment_id: [u8; 32])]
pub struct CancelPending<'info> {
    #[account(
        mut,
        seeds = [b"escrow", payment_id.as_ref()],
        bump = escrow.bump,
        close = merchant,
    )]
    pub escrow: Account<'info, Escrow>,

    // The vault for a Pending escrow is empty; close it and return rent to merchant.
    #[account(
        mut,
        associated_token::mint = usdc_mint,
        associated_token::authority = escrow,
    )]
    pub vault: Account<'info, TokenAccount>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(mut)]
    pub merchant: Signer<'info>,

    pub token_program: Program<'info, Token>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────────────

#[event]
pub struct ConfigInitialized {
    pub admin: Pubkey,
    pub usdc_mint: Pubkey,
}

#[event]
pub struct EscrowCreated {
    pub payment_id: [u8; 32],
    pub merchant: Pubkey,
    pub amount: u64,
}

#[event]
pub struct EscrowFunded {
    pub payment_id: [u8; 32],
    pub depositor: Pubkey,
    pub expiry: i64,
}

#[event]
pub struct EscrowReleased {
    pub payment_id: [u8; 32],
    pub merchant: Pubkey,
    pub amount: u64,
}

#[event]
pub struct EscrowRefunded {
    pub payment_id: [u8; 32],
    pub depositor: Pubkey,
    pub amount: u64,
}

#[event]
pub struct EscrowCancelled {
    pub payment_id: [u8; 32],
}

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

#[error_code]
pub enum EscrowError {
    #[msg("Escrow is not in the required status for this action")]
    InvalidStatus,
    #[msg("Amount must be greater than zero")]
    InvalidAmount,
    #[msg("Configuration bounds are invalid")]
    InvalidBounds,
    #[msg("Payment window is outside the configured bounds")]
    PaymentWindowOutOfBounds,
    #[msg("Grace period is outside the configured bounds")]
    GracePeriodOutOfBounds,
    #[msg("The release window has passed; funds can only be refunded now")]
    ReleaseWindowPassed,
    #[msg("Still within the release window; refund not yet available")]
    StillInReleaseWindow,
    #[msg("Only the configured admin may release funds")]
    UnauthorizedAdmin,
    #[msg("Only the recorded merchant may perform this action")]
    UnauthorizedMerchant,
    #[msg("Provided merchant token account does not match the recorded merchant")]
    WrongMerchant,
    #[msg("Provided token account does not match the recorded depositor")]
    WrongDepositor,
    #[msg("Provided token account uses the wrong mint")]
    WrongMint,
    #[msg("Arithmetic overflow")]
    MathOverflow,
}

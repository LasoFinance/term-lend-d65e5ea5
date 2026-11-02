//! term-lend: minimal peer-to-peer, fixed-term lending.
//!
//! A lender posts an offer: `principal` tUSDC for one of four fixed terms, a flat
//! `rate_bps` interest for the whole term, and the tSOL `collateral` a borrower must
//! lock. The principal sits in a vault owned by the loan PDA until someone takes it.
//!
//!   create_offer -> (cancel_offer) | borrow -> repay | claim (after due date)
//!
//! A borrower locks the collateral and receives the principal at once. Repaying
//! principal + interest returns the collateral; if the loan is not repaid by the
//! due date, the lender can claim the collateral instead. No oracles, no pools.
use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount, Transfer};

declare_id!("24FQxqPUy6zNkAJgJ6xsHA4ZS6KaQRzBrA4Ns7aZ7e29");

/// Fixed terms in seconds: 2 minutes (demo), 7 days, 30 days, 90 days.
pub const TERMS: [i64; 4] = [120, 7 * 86_400, 30 * 86_400, 90 * 86_400];
pub const MAX_RATE_BPS: u16 = 5_000; // 50% per term at most
pub const FAUCET_USDC_CAP: u64 = 10_000 * 1_000_000; // 10k tUSDC (6 dp)
pub const FAUCET_SOL_CAP: u64 = 100 * 1_000_000_000; // 100 tSOL (9 dp)

pub const OPEN: u8 = 0;
pub const ACTIVE: u8 = 1;
pub const REPAID: u8 = 2;
pub const DEFAULTED: u8 = 3;
pub const CANCELLED: u8 = 4;

#[program]
pub mod term_lend {
    use super::*;

    /// One-time: create the test loan token (tUSDC) and collateral token (tSOL).
    pub fn init_mints(_ctx: Context<InitMints>) -> Result<()> {
        Ok(())
    }

    /// Devnet faucet: anyone can mint test tokens to themselves.
    pub fn faucet(ctx: Context<Faucet>, usdc: u64, sol: u64) -> Result<()> {
        require!(usdc <= FAUCET_USDC_CAP && sol <= FAUCET_SOL_CAP, LendError::FaucetCap);
        let seeds: &[&[u8]] = &[b"faucet", &[ctx.bumps.faucet]];
        for (mint, to, amt) in [
            (&ctx.accounts.usdc_mint, &ctx.accounts.user_usdc, usdc),
            (&ctx.accounts.sol_mint, &ctx.accounts.user_sol, sol),
        ] {
            if amt > 0 {
                token::mint_to(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.key(),
                        MintTo { mint: mint.to_account_info(), to: to.to_account_info(), authority: ctx.accounts.faucet.to_account_info() },
                        &[seeds],
                    ),
                    amt,
                )?;
            }
        }
        Ok(())
    }

    /// Lender posts a fixed-term offer and deposits the principal.
    pub fn create_offer(ctx: Context<CreateOffer>, id: u64, principal: u64, rate_bps: u16, term: u8, collateral: u64) -> Result<()> {
        require!(principal > 0 && collateral > 0, LendError::BadParams);
        require!((term as usize) < TERMS.len(), LendError::BadTerm);
        require!(rate_bps <= MAX_RATE_BPS, LendError::BadParams);
        let interest = (principal as u128 * rate_bps as u128 / 10_000) as u64;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.lender_usdc.to_account_info(),
                    to: ctx.accounts.usdc_vault.to_account_info(),
                    authority: ctx.accounts.lender.to_account_info(),
                },
            ),
            principal,
        )?;

        let l = &mut ctx.accounts.loan;
        l.lender = ctx.accounts.lender.key();
        l.borrower = Pubkey::default();
        l.id = id;
        l.principal = principal;
        l.interest = interest;
        l.rate_bps = rate_bps;
        l.collateral = collateral;
        l.term = term;
        l.state = OPEN;
        l.created_at = Clock::get()?.unix_timestamp;
        l.start_ts = 0;
        l.due_ts = 0;
        l.bump = ctx.bumps.loan;
        Ok(())
    }

    /// Lender withdraws an offer nobody has taken yet.
    pub fn cancel_offer(ctx: Context<CancelOffer>) -> Result<()> {
        require!(ctx.accounts.loan.state == OPEN, LendError::BadState);
        let (lender, id, bump, amount) = {
            let l = &ctx.accounts.loan;
            (l.lender, l.id.to_le_bytes(), l.bump, l.principal)
        };
        let seeds: &[&[u8]] = &[b"loan", lender.as_ref(), &id, &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.lender_usdc.to_account_info(),
                    authority: ctx.accounts.loan.to_account_info(),
                },
                &[seeds],
            ),
            amount,
        )?;
        ctx.accounts.loan.state = CANCELLED;
        Ok(())
    }

    /// Borrower locks the collateral and receives the principal; the term starts now.
    pub fn borrow(ctx: Context<Borrow>) -> Result<()> {
        require!(ctx.accounts.loan.state == OPEN, LendError::BadState);
        let (lender, id, bump, principal, collateral, term) = {
            let l = &ctx.accounts.loan;
            (l.lender, l.id.to_le_bytes(), l.bump, l.principal, l.collateral, l.term)
        };
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.borrower_sol.to_account_info(),
                    to: ctx.accounts.col_vault.to_account_info(),
                    authority: ctx.accounts.borrower.to_account_info(),
                },
            ),
            collateral,
        )?;
        let seeds: &[&[u8]] = &[b"loan", lender.as_ref(), &id, &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.usdc_vault.to_account_info(),
                    to: ctx.accounts.borrower_usdc.to_account_info(),
                    authority: ctx.accounts.loan.to_account_info(),
                },
                &[seeds],
            ),
            principal,
        )?;
        let now = Clock::get()?.unix_timestamp;
        let l = &mut ctx.accounts.loan;
        l.borrower = ctx.accounts.borrower.key();
        l.state = ACTIVE;
        l.start_ts = now;
        l.due_ts = now + TERMS[term as usize];
        Ok(())
    }

    /// Borrower pays principal + interest to the lender and gets the collateral back.
    /// Allowed any time until the lender claims a defaulted loan.
    pub fn repay(ctx: Context<Repay>) -> Result<()> {
        require!(ctx.accounts.loan.state == ACTIVE, LendError::BadState);
        let (lender, id, bump, owed, collateral) = {
            let l = &ctx.accounts.loan;
            (l.lender, l.id.to_le_bytes(), l.bump, l.principal + l.interest, l.collateral)
        };
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.borrower_usdc.to_account_info(),
                    to: ctx.accounts.lender_usdc.to_account_info(),
                    authority: ctx.accounts.borrower.to_account_info(),
                },
            ),
            owed,
        )?;
        let seeds: &[&[u8]] = &[b"loan", lender.as_ref(), &id, &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.col_vault.to_account_info(),
                    to: ctx.accounts.borrower_sol.to_account_info(),
                    authority: ctx.accounts.loan.to_account_info(),
                },
                &[seeds],
            ),
            collateral,
        )?;
        ctx.accounts.loan.state = REPAID;
        Ok(())
    }

    /// After the due date, the lender takes the collateral of an unpaid loan.
    pub fn claim(ctx: Context<Claim>) -> Result<()> {
        require!(ctx.accounts.loan.state == ACTIVE, LendError::BadState);
        require!(Clock::get()?.unix_timestamp > ctx.accounts.loan.due_ts, LendError::NotDue);
        let (lender, id, bump, collateral) = {
            let l = &ctx.accounts.loan;
            (l.lender, l.id.to_le_bytes(), l.bump, l.collateral)
        };
        let seeds: &[&[u8]] = &[b"loan", lender.as_ref(), &id, &[bump]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.col_vault.to_account_info(),
                    to: ctx.accounts.lender_sol.to_account_info(),
                    authority: ctx.accounts.loan.to_account_info(),
                },
                &[seeds],
            ),
            collateral,
        )?;
        ctx.accounts.loan.state = DEFAULTED;
        Ok(())
    }
}

// ---------------------------------------------------------------- state

#[account]
#[derive(InitSpace)]
pub struct Loan {
    pub lender: Pubkey,
    pub borrower: Pubkey,
    pub id: u64,
    pub principal: u64,
    pub interest: u64,
    pub rate_bps: u16,
    pub collateral: u64,
    pub term: u8,
    pub state: u8,
    pub created_at: i64,
    pub start_ts: i64,
    pub due_ts: i64,
    pub bump: u8,
}

// ---------------------------------------------------------------- accounts

#[derive(Accounts)]
pub struct InitMints<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// CHECK: PDA that is the mint authority of both test tokens.
    #[account(seeds = [b"faucet"], bump)]
    pub faucet: UncheckedAccount<'info>,
    #[account(init, payer = payer, seeds = [b"usdc"], bump, mint::decimals = 6, mint::authority = faucet)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(init, payer = payer, seeds = [b"sol"], bump, mint::decimals = 9, mint::authority = faucet)]
    pub sol_mint: Box<Account<'info, Mint>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Faucet<'info> {
    #[account(mut)]
    pub user: Signer<'info>,
    /// CHECK: PDA mint authority.
    #[account(seeds = [b"faucet"], bump)]
    pub faucet: UncheckedAccount<'info>,
    #[account(mut, seeds = [b"usdc"], bump)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"sol"], bump)]
    pub sol_mint: Box<Account<'info, Mint>>,
    #[account(init_if_needed, payer = user, associated_token::mint = usdc_mint, associated_token::authority = user)]
    pub user_usdc: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = user, associated_token::mint = sol_mint, associated_token::authority = user)]
    pub user_sol: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(id: u64)]
pub struct CreateOffer<'info> {
    #[account(mut)]
    pub lender: Signer<'info>,
    #[account(seeds = [b"usdc"], bump)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(seeds = [b"sol"], bump)]
    pub sol_mint: Box<Account<'info, Mint>>,
    #[account(init, payer = lender, space = 8 + Loan::INIT_SPACE,
        seeds = [b"loan", lender.key().as_ref(), &id.to_le_bytes()], bump)]
    pub loan: Box<Account<'info, Loan>>,
    #[account(init, payer = lender, seeds = [b"usdc_vault", loan.key().as_ref()], bump,
        token::mint = usdc_mint, token::authority = loan)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,
    #[account(init, payer = lender, seeds = [b"col_vault", loan.key().as_ref()], bump,
        token::mint = sol_mint, token::authority = loan)]
    pub col_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = usdc_mint, associated_token::authority = lender)]
    pub lender_usdc: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelOffer<'info> {
    #[account(mut)]
    pub lender: Signer<'info>,
    #[account(mut, has_one = lender)]
    pub loan: Box<Account<'info, Loan>>,
    #[account(seeds = [b"usdc"], bump)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"usdc_vault", loan.key().as_ref()], bump)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = usdc_mint, associated_token::authority = lender)]
    pub lender_usdc: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Borrow<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,
    #[account(mut)]
    pub loan: Box<Account<'info, Loan>>,
    #[account(seeds = [b"usdc"], bump)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(seeds = [b"sol"], bump)]
    pub sol_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"usdc_vault", loan.key().as_ref()], bump)]
    pub usdc_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, seeds = [b"col_vault", loan.key().as_ref()], bump)]
    pub col_vault: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = borrower, associated_token::mint = usdc_mint, associated_token::authority = borrower)]
    pub borrower_usdc: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = sol_mint, associated_token::authority = borrower)]
    pub borrower_sol: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Repay<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,
    /// CHECK: the loan's lender, checked by has_one; receives the repayment.
    pub lender: UncheckedAccount<'info>,
    #[account(mut, has_one = borrower, has_one = lender)]
    pub loan: Box<Account<'info, Loan>>,
    #[account(seeds = [b"usdc"], bump)]
    pub usdc_mint: Box<Account<'info, Mint>>,
    #[account(seeds = [b"sol"], bump)]
    pub sol_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"col_vault", loan.key().as_ref()], bump)]
    pub col_vault: Box<Account<'info, TokenAccount>>,
    #[account(mut, associated_token::mint = usdc_mint, associated_token::authority = borrower)]
    pub borrower_usdc: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = borrower, associated_token::mint = sol_mint, associated_token::authority = borrower)]
    pub borrower_sol: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = borrower, associated_token::mint = usdc_mint, associated_token::authority = lender)]
    pub lender_usdc: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct Claim<'info> {
    #[account(mut)]
    pub lender: Signer<'info>,
    #[account(mut, has_one = lender)]
    pub loan: Box<Account<'info, Loan>>,
    #[account(seeds = [b"sol"], bump)]
    pub sol_mint: Box<Account<'info, Mint>>,
    #[account(mut, seeds = [b"col_vault", loan.key().as_ref()], bump)]
    pub col_vault: Box<Account<'info, TokenAccount>>,
    #[account(init_if_needed, payer = lender, associated_token::mint = sol_mint, associated_token::authority = lender)]
    pub lender_sol: Box<Account<'info, TokenAccount>>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[error_code]
pub enum LendError {
    #[msg("Invalid amount or rate")]
    BadParams,
    #[msg("Term must be 0 (2 min demo), 1 (7d), 2 (30d) or 3 (90d)")]
    BadTerm,
    #[msg("Loan is not in the right state for this action")]
    BadState,
    #[msg("Loan is not past its due date yet")]
    NotDue,
    #[msg("Faucet limit is 10,000 tUSDC and 100 tSOL per call")]
    FaucetCap,
}

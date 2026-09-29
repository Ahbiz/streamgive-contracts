// SPDX-License-Identifier: Apache-2.0
#![no_std]
// soroban-sdk 27 deprecates Events::publish in favour of the
// #[contractevent] macro. Migrating is not a lint cleanup: #[contractevent]
// derives its own topic/data layout, and streamgive-backend's indexer
// decodes the current layout by hand (topic[0] = symbol, topic[1] = id),
// as does docs/EVENTS.md. Both repos have to move in the same change, so
// it is tracked as its own issue rather than done under -D warnings here.
#![allow(deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, BytesN, Env,
};

mod math;

use ngo_registry::NgoRegistryClient;

/// A single donor -> NGO streaming donation.
///
/// `balance` is the undrawn amount still deposited in the vault; `rate` is
/// how much of it accrues to the NGO per second. `created_at` is set once,
/// by `create_stream`, and never changes; `last_update` moves forward on
/// every checkpoint (withdraw, cancel, top-up, or rate change).
#[contracttype]
// Debug and PartialEq let tests assert_eq! on a try_* call’s full
// Result<Result<Stream, _>, _> rather than unwrapping it by hand first,
// and compare a whole stream at once instead of field by field.
#[derive(Clone, Debug, PartialEq)]
pub struct Stream {
    pub donor: Address,
    pub ngo: Address,
    pub token: Address,
    pub rate: i128,
    pub balance: i128,
    pub withdrawn: i128,
    pub created_at: u64,
    pub last_update: u64,
    /// Explicit lifecycle state. Set to Active at creation, Cancelled on
    /// cancel_stream, and Drained when withdraw brings balance to zero.
    pub status: StreamStatus,
    /// Set once by `cancel_stream`, never unset. Distinguishes a cancelled
    /// stream (`rate == 0`, `balance == 0`) from one that simply ran dry
    /// and was withdrawn in full (`balance == 0` but `rate` unchanged).
    pub cancelled: bool,
}

/// Explicit lifecycle state of a stream. Prior to this field a client had
/// to inspect `rate`, `balance`, and `withdrawn` together to infer state;
/// the enum makes it queryable directly. See issue #92.
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StreamStatus {
    /// Normal state: rate > 0, balance > 0.
    Active,
    /// Donor cancelled. Balance and rate are both zero.
    Cancelled,
    /// The stream ran to completion: the last withdrawal brought balance
    /// to zero without a cancel.
    Drained,
}

#[contracttype]
#[derive(Clone, Debug)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    NextStreamId,
    Stream(u64),
    Paused,
    Treasury,
    FeeBps,
    /// Admin-settable per-donor stream cap. See `set_max_streams_per_donor`.
    MaxStreamsPerDonor,
    /// Count of streams a donor currently has open. Incremented on
    /// `create_stream`, never decremented (streams are cancelled, not
    /// deleted) — so this is really a lifetime cap, not a live cap.
    DonorStreamCount(Address),
    MinDeposit,
    CancelGraceLedgers,
    /// Optional NGO registry contract used to verify NGOs before a stream
    /// is opened. Absent means "no registry check configured".
    Registry,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    StreamNotFound = 3,
    InvalidAmount = 4,
    NothingToWithdraw = 5,
    ContractPaused = 6,
    FeeTooHigh = 7,
    NoPendingAdmin = 8,
    /// A stream's `balance` or `withdrawn` (or the stream-id counter) would
    /// leave its type's range. Returned instead of letting the release
    /// profile's overflow checks panic and abort the transaction.
    ArithmeticOverflow = 9,
    /// `deposit` was below the configured `min_deposit`.
    DepositTooLow = 10,
    AlreadyPaused = 11,
    AlreadyUnpaused = 12,
    /// The donor and the NGO are the same address, so the stream would pay
    /// the donor back their own deposit. Rejected at creation: a stream that
    /// nets to zero still counts as a committed donation in the indexer and
    /// on impact pages, which is a way to inflate those totals for free.
    SelfStream = 13,
    /// The stream has already been cancelled and closed out.
    StreamCancelled = 14,
    /// The proposed administrator is not a valid replacement.
    InvalidAdmin = 15,
}

/// Fee cap of 10%, enforced by `set_fee_bps` so the admin can never take
/// an unreasonable cut of donations.
const MAX_FEE_BPS: u32 = 1_000;

/// Default per-donor stream cap applied when `set_max_streams_per_donor`
/// has not been called. Chosen so an ordinary donor can open hundreds of
/// streams across many NGOs, but a flood attack hits the ceiling long
/// before it can bloat persistent storage. See issue #94.
const DEFAULT_MAX_STREAMS_PER_DONOR: u64 = 100;

/// Approximate ledgers per day at a 5-second close time. Used to express
/// storage TTLs (which the network counts in ledgers, not wall time) in
/// human terms.
const DAY_IN_LEDGERS: u32 = 17_280;

const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;

const STREAM_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
const STREAM_LIFETIME_THRESHOLD: u32 = STREAM_BUMP_AMOUNT - DAY_IN_LEDGERS;

/// Keeps the contract instance (admin, config, next-id counter) from being
/// archived. Called on every state-changing entry point.
fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

/// Keeps a stream's persistent entry alive for 90 days past its last
/// touch, so a slow-draining stream doesn't get archived out from under
/// its donor and NGO between activity.
fn extend_stream_ttl(env: &Env, stream_id: u64) {
    env.storage().persistent().extend_ttl(
        &DataKey::Stream(stream_id),
        STREAM_LIFETIME_THRESHOLD,
        STREAM_BUMP_AMOUNT,
    );
}

/// Extends a cancelled stream beyond the normal retention period by the
/// configured indexing grace period.
fn extend_cancelled_stream_ttl(env: &Env, stream_id: u64, grace_ledgers: u32) -> Result<(), Error> {
    let bump_amount = STREAM_BUMP_AMOUNT
        .checked_add(grace_ledgers)
        .ok_or(Error::ArithmeticOverflow)?;
    env.storage().persistent().extend_ttl(
        &DataKey::Stream(stream_id),
        STREAM_LIFETIME_THRESHOLD,
        bump_amount,
    );
    Ok(())
}

/// Reads the configured admin and requires their auth, failing with
/// `Error::NotInitialized` if `init` hasn't been called yet. Shared by
/// every admin-gated entry point so the same three steps aren't repeated
/// at each call site.
fn require_admin(env: &Env) -> Result<Address, Error> {
    let admin: Address = env
        .storage()
        .instance()
        .get(&DataKey::Admin)
        .ok_or(Error::NotInitialized)?;
    admin.require_auth();
    Ok(admin)
}

/// Returns `Err(Error::ContractPaused)` if an admin has paused the vault.
/// Checked at the top of every fund-moving entry point.
fn require_not_paused(env: &Env) -> Result<(), Error> {
    let paused: bool = env
        .storage()
        .instance()
        .get(&DataKey::Paused)
        .unwrap_or(false);
    if paused {
        return Err(Error::ContractPaused);
    }
    Ok(())
}

/// Moves `amount` from a stream's `balance` into its `withdrawn` total,
/// failing with `Error::ArithmeticOverflow` rather than panicking if
/// either would leave i128's range.
fn record_payout(stream: &mut Stream, amount: i128) -> Result<(), Error> {
    stream.balance = stream
        .balance
        .checked_sub(amount)
        .ok_or(Error::ArithmeticOverflow)?;
    stream.withdrawn = stream
        .withdrawn
        .checked_add(amount)
        .ok_or(Error::ArithmeticOverflow)?;
    Ok(())
}

/// Returns the protocol fee that would be taken on `amount`, using the same
/// logic as `pay_ngo`. Zero when no treasury is configured, regardless of
/// `fee_bps` — there's nowhere to send a fee without a destination address.
/// Rounds toward zero (the NGO never loses a unit to rounding).
fn compute_fee(env: &Env, amount: i128) -> i128 {
    let treasury: Option<Address> = env.storage().instance().get(&DataKey::Treasury);
    match treasury {
        Some(_) => {
            let fee_bps: u32 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
            (amount.saturating_mul(fee_bps as i128) / 10_000).min(amount)
        }
        None => 0,
    }
}

/// Pays `amount` out to the NGO, skimming a protocol fee to the treasury
/// first if one is configured. With no treasury set, the full amount goes
/// to the NGO regardless of `fee_bps` — there's nowhere to send a fee.
///
/// Returns the net amount actually transferred to the NGO. This is the
/// single place the fee split is computed, so callers that report the
/// payout to their own callers (`withdraw`) can return exactly what the
/// NGO received rather than recomputing the fee and risking drift.
fn pay_ngo(env: &Env, token_client: &token::Client, ngo: &Address, amount: i128) -> i128 {
    if amount <= 0 {
        return 0;
    }

    let fee = compute_fee(env, amount);
    let net = amount - fee;

    if net > 0 {
        token_client.transfer(&env.current_contract_address(), ngo, &net);
    }
    if fee > 0 {
        let treasury: Option<Address> = env.storage().instance().get(&DataKey::Treasury);
        if let Some(treasury) = treasury {
            token_client.transfer(&env.current_contract_address(), &treasury, &fee);
        }
    }

    net
}

/// Settles the accrual accumulated since the stream's last checkpoint.
///
/// This is shared by every operation that changes a stream's balance or rate
/// so payout accounting, checked arithmetic, and the checkpoint timestamp
/// cannot drift between entry points.
fn settle(env: &Env, stream: &mut Stream, now: u64) -> Result<i128, Error> {
    let elapsed = now.saturating_sub(stream.last_update);
    let accrued = math::accrued(stream.rate, elapsed, stream.balance);
    let token_client = token::Client::new(env, &stream.token);

    if accrued > 0 {
        pay_ngo(env, &token_client, &stream.ngo, accrued);
        record_payout(stream, accrued)?;
    }
    stream.last_update = now;
    Ok(accrued)
}

#[contract]
pub struct DonationVault;

#[contractimpl]
impl DonationVault {
    /// Sets the vault admin and seeds the stream-id counter. Can only be called once.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// let env = Env::default();
    /// env.mock_all_auths();
    ///
    /// let contract_id = env.register(DonationVault, ());
    /// let client = DonationVaultClient::new(&env, &contract_id);
    ///
    /// let admin = Address::generate(&env);
    /// client.init(&admin);
    /// ```
    pub fn init(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::NextStreamId, &0u64);
        env.storage().instance().set(&DataKey::MinDeposit, &0i128);
        env.storage()
            .instance()
            .set(&DataKey::CancelGraceLedgers, &0u32);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Reads back the vault admin set by `init`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.admin(), admin);
    /// ```
    pub fn admin(env: Env) -> Result<Address, Error> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(Error::NotInitialized)
    }

    /// Reads back the address proposed by `propose_admin`, if any hasn't
    /// yet been accepted or cancelled.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.pending_admin(), None);
    ///
    /// let new_admin = Address::generate(&env);
    /// client.propose_admin(&new_admin);
    /// assert_eq!(client.pending_admin(), Some(new_admin));
    /// ```
    pub fn pending_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    /// Starts a two-step admin transfer by recording `new_admin` as pending.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let new_admin = Address::generate(&env);
    /// client.propose_admin(&new_admin);
    /// // The old admin is still in charge until accept_admin is called.
    /// assert_eq!(client.admin(), admin);
    /// ```
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), Error> {
        let current_admin = require_admin(&env)?;
        if new_admin == current_admin {
            return Err(Error::InvalidAdmin);
        }

        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        extend_instance_ttl(&env);

        env.events()
            .publish((symbol_short!("propadmin"),), new_admin);

        Ok(())
    }

    /// Completes a two-step admin transfer.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let new_admin = Address::generate(&env);
    /// client.propose_admin(&new_admin);
    /// client.accept_admin();
    /// assert_eq!(client.admin(), new_admin);
    /// ```
    pub fn accept_admin(env: Env) -> Result<(), Error> {
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(Error::NoPendingAdmin)?;
        pending.require_auth();

        env.storage().instance().set(&DataKey::Admin, &pending);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        extend_instance_ttl(&env);

        env.events().publish((symbol_short!("acptadmin"),), pending);

        Ok(())
    }

    /// Withdraws a pending admin proposal, leaving nothing pending.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let new_admin = Address::generate(&env);
    /// client.propose_admin(&new_admin);
    ///
    /// // The admin changes their mind before it's accepted.
    /// client.cancel_admin_proposal();
    ///
    /// // Nothing left to accept.
    /// let result = client.try_accept_admin();
    /// assert!(result.is_err());
    /// ```
    pub fn cancel_admin_proposal(env: Env) -> Result<(), Error> {
        require_admin(&env)?;

        if !env.storage().instance().has(&DataKey::PendingAdmin) {
            return Err(Error::NoPendingAdmin);
        }
        env.storage().instance().remove(&DataKey::PendingAdmin);
        extend_instance_ttl(&env);

        env.events().publish((symbol_short!("canceladm"),), ());

        Ok(())
    }

    /// Reads back a stream by id.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    ///
    /// let stream = client.get_stream(&stream_id);
    /// assert_eq!(stream.balance, 1_000);
    /// assert_eq!(stream.rate, 10);
    /// assert!(!stream.cancelled);
    ///
    /// // Once cancelled, `cancelled` stays true even though a drained
    /// // (fully withdrawn) stream would also show `rate == 0 && balance == 0`.
    /// client.cancel_stream(&stream_id);
    /// assert!(client.get_stream(&stream_id).cancelled);
    /// ```
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)
    }

    /// Reads back the number of streams ever created — the exclusive upper
    /// bound on valid stream ids.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.stream_count(), 0);
    ///
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// assert_eq!(client.stream_count(), 1);
    /// ```
    pub fn stream_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextStreamId)
            .unwrap_or(0)
    }

    /// Read-only lookup of how much a stream has accrued to the NGO so far.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// env.ledger().with_mut(|l| l.timestamp += 50);
    ///
    /// assert_eq!(client.pending_accrual(&stream_id), 500);
    /// // Balance is untouched — pending_accrual doesn't pay out.
    /// assert_eq!(client.get_stream(&stream_id).balance, 1_000);
    /// ```
    pub fn pending_accrual(env: Env, stream_id: u64) -> Result<i128, Error> {
        let stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)?;

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        Ok(math::accrued(stream.rate, elapsed, stream.balance))
    }

    /// Read-only view of the net amount the NGO would actually receive and the
    /// fee that would be taken if `withdraw` were called right now.
    ///
    /// Unlike `pending_accrual`, which returns the gross accrued amount, this
    /// accounts for any configured treasury fee, so a UI can show the correct
    /// "you will receive X" figure rather than overstating it.
    ///
    /// Never mutates storage or moves funds.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let treasury = Address::generate(&env);
    /// client.set_treasury(&treasury);
    /// client.set_fee_bps(&500); // 5%
    ///
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues
    ///
    /// let (net, fee) = client.pending_payout(&stream_id);
    /// assert_eq!(fee, 25);   // 5% of 500
    /// assert_eq!(net, 475);  // 500 - 25
    /// ```
    pub fn pending_payout(env: Env, stream_id: u64) -> Result<(i128, i128), Error> {
        let stream: Stream = env
            .storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)?;

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        let gross = math::accrued(stream.rate, elapsed, stream.balance);
        let fee = compute_fee(&env, gross);
        Ok((gross - fee, fee))
    }

    /// Bumps a stream's persistent-storage TTL without touching its state.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    ///
    /// // Anyone can keep the stream's storage alive, no auth required.
    /// client.extend_stream(&stream_id);
    /// ```
    pub fn extend_stream(env: Env, stream_id: u64) -> Result<(), Error> {
        if !env.storage().persistent().has(&DataKey::Stream(stream_id)) {
            return Err(Error::StreamNotFound);
        }
        extend_stream_ttl(&env, stream_id);
        Ok(())
    }

    /// Halts stream creation, withdrawal, top-up, and rate changes.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// client.pause();
    /// assert!(client.paused());
    /// ```
    pub fn pause(env: Env) -> Result<(), Error> {
        require_admin(&env)?;
        let already_paused = env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false);
        if already_paused {
            return Err(Error::AlreadyPaused);
        }
        env.storage().instance().set(&DataKey::Paused, &true);
        extend_instance_ttl(&env);
        env.events().publish((symbol_short!("pause"),), ());
        Ok(())
    }

    /// Lifts a pause, restoring normal operation. Admin-gated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # client.pause();
    /// client.unpause();
    /// assert!(!client.paused());
    /// ```
    pub fn unpause(env: Env) -> Result<(), Error> {
        require_admin(&env)?;
        let already_unpaused = !env
            .storage()
            .instance()
            .get::<DataKey, bool>(&DataKey::Paused)
            .unwrap_or(false);
        if already_unpaused {
            return Err(Error::AlreadyUnpaused);
        }
        env.storage().instance().set(&DataKey::Paused, &false);
        extend_instance_ttl(&env);
        env.events().publish((symbol_short!("unpause"),), ());
        Ok(())
    }

    /// Whether the vault is currently paused.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert!(!client.paused());
    /// ```
    pub fn paused(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Sets where the protocol fee (if any) gets paid. Admin-gated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let treasury = Address::generate(&env);
    /// client.set_treasury(&treasury);
    /// assert_eq!(client.treasury(), Some(treasury));
    /// ```
    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::Treasury, &treasury);
        extend_instance_ttl(&env);

        env.events().publish((symbol_short!("treasury"),), treasury);

        Ok(())
    }

    /// Reads back the configured treasury address, if any.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.treasury(), None);
    /// ```
    pub fn treasury(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Treasury)
    }

    /// Removes the configured treasury so protocol fees are no longer collected.
    /// After this call `treasury()` returns `None` and `pay_ngo` sends the full
    /// payout directly to the NGO, the same as if a treasury had never been set.
    /// Admin-gated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let treasury = Address::generate(&env);
    /// client.set_treasury(&treasury);
    /// assert_eq!(client.treasury(), Some(treasury));
    ///
    /// client.clear_treasury();
    /// assert_eq!(client.treasury(), None);
    /// ```
    pub fn clear_treasury(env: Env) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().remove(&DataKey::Treasury);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Sets the protocol fee, in basis points, taken out of accrued payouts
    /// to the NGO. Admin-gated, capped at `MAX_FEE_BPS`. Has no effect
    /// unless a treasury is also set. Emits a `feeset` event carrying the
    /// new value so an off-chain indexer can track fee changes without
    /// polling `fee_bps`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// client.set_fee_bps(&500); // 5%
    /// assert_eq!(client.fee_bps(), 500);
    ///
    /// // Anything over the 10% cap is rejected.
    /// let result = client.try_set_fee_bps(&1_001);
    /// assert!(result.is_err());
    /// ```
    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), Error> {
        require_admin(&env)?;
        if fee_bps > MAX_FEE_BPS {
            return Err(Error::FeeTooHigh);
        }
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        extend_instance_ttl(&env);

        env.events().publish((symbol_short!("feeset"),), fee_bps);

        Ok(())
    }

    /// Reads back the configured protocol fee, in basis points.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.fee_bps(), 0);
    /// ```
    pub fn fee_bps(env: Env) -> u32 {
        env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0)
    }

    /// Sets the minimum `deposit` accepted by `create_stream`, letting an
    /// operator filter out dust streams without changing application-level
    /// validation on every frontend that talks to the contract. Admin-gated.
    /// Defaults to `0` (today's behavior) until set.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// client.set_min_deposit(&100);
    /// assert_eq!(client.min_deposit(), 100);
    /// ```
    pub fn set_min_deposit(env: Env, min_deposit: i128) -> Result<(), Error> {
        require_admin(&env)?;
        if min_deposit < 0 {
            return Err(Error::InvalidAmount);
        }
        env.storage()
            .instance()
            .set(&DataKey::MinDeposit, &min_deposit);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Reads back the configured minimum deposit. `0` until an admin sets
    /// one.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.min_deposit(), 0);
    /// ```
    pub fn min_deposit(env: Env) -> i128 {
        env.storage()
            .instance()
            .get(&DataKey::MinDeposit)
            .unwrap_or(0)
    }

    /// Sets how many additional ledgers a cancelled stream remains available
    /// for indexers after the normal stream-retention period. Admin-only.
    pub fn set_cancel_grace_ledgers(env: Env, grace_ledgers: u32) -> Result<(), Error> {
        require_admin(&env)?;
        STREAM_BUMP_AMOUNT
            .checked_add(grace_ledgers)
            .ok_or(Error::ArithmeticOverflow)?;
        env.storage()
            .instance()
            .set(&DataKey::CancelGraceLedgers, &grace_ledgers);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Returns the configured cancelled-stream indexing grace period.
    pub fn cancel_grace_ledgers(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::CancelGraceLedgers)
            .unwrap_or(0)
    }

    /// Sets the per-donor stream cap. Admin-gated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// client.set_max_streams_per_donor(&5);
    /// assert_eq!(client.max_streams_per_donor(), 5);
    /// ```
    pub fn set_max_streams_per_donor(env: Env, limit: u64) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage()
            .instance()
            .set(&DataKey::MaxStreamsPerDonor, &limit);
        extend_instance_ttl(&env);
        env.events()
            .publish((symbol_short!("maxstrm"),), limit);
        Ok(())
    }

    /// Reads back the configured per-donor stream cap, or the default if
    /// `set_max_streams_per_donor` has never been called.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// assert_eq!(client.max_streams_per_donor(), 100);
    /// ```
    pub fn max_streams_per_donor(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::MaxStreamsPerDonor)
            .unwrap_or(DEFAULT_MAX_STREAMS_PER_DONOR)
    }

    /// Sets (or clears) the NGO registry contract used to verify NGOs
    /// before a stream is opened. Admin-gated.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let registry = Address::generate(&env);
    /// client.set_registry(&registry);
    /// assert_eq!(client.registry(), Some(registry));
    /// ```
    pub fn set_registry(env: Env, registry: Address) -> Result<(), Error> {
        require_admin(&env)?;
        env.storage().instance().set(&DataKey::Registry, &registry);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Reads back the configured NGO registry address, if any.
    pub fn registry(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Registry)
    }

    /// Opens a new stream: pulls `deposit` of `token` from the donor into the
    /// vault, to be released to the NGO at `rate` per second on withdrawal.
    /// `donor` and `ngo` must be distinct addresses.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient, Error};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// // Stream 1_000 units of the token to `ngo` at 10 units/second.
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// assert_eq!(client.get_stream(&stream_id).balance, 1_000);
    ///
    /// // A stream needs two distinct parties — the vault refuses to pay a
    /// // donor back their own deposit.
    /// let result = client.try_create_stream(&donor, &donor, &sac.address(), &1_000, &10);
    /// assert_eq!(result, Err(Ok(Error::SelfStream)));
    /// ```
    pub fn create_stream(
        env: Env,
        donor: Address,
        ngo: Address,
        token: Address,
        deposit: i128,
        rate: i128,
    ) -> Result<u64, Error> {
        require_not_paused(&env)?;
        donor.require_auth();

        // Checked before the deposit is pulled and before the amounts are
        // validated: a self-stream is never a legitimate call regardless of
        // how the other arguments look, and it must not reach the transfer.
        if donor == ngo {
            return Err(Error::SelfStream);
        }

        if deposit <= 0 || rate <= 0 {
            return Err(Error::InvalidAmount);
        }
        if deposit < Self::min_deposit(env.clone()) {
            return Err(Error::DepositTooLow);
        }

        // Per-donor stream cap (issue #94). Read before the token transfer
        // so a rejected donor is rejected without moving funds.
        let max_streams: u64 = env
            .storage()
            .instance()
            .get(&DataKey::MaxStreamsPerDonor)
            .unwrap_or(DEFAULT_MAX_STREAMS_PER_DONOR);
        let donor_streams: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::DonorStreamCount(donor.clone()))
            .unwrap_or(0);
        if donor_streams >= max_streams {
            return Err(Error::StreamLimitExceeded);
        }

        // If a registry has been configured, verify the NGO is approved before
        // pulling any funds from the donor.
        if let Some(registry_addr) = env
            .storage()
            .instance()
            .get::<_, Address>(&DataKey::Registry)
        {
            let registry = NgoRegistryClient::new(&env, &registry_addr);
            let ngo_entry = registry
                .try_get_ngo(&ngo)
                .map_err(|_| Error::NgoNotVerified)?
                .map_err(|_| Error::NgoNotVerified)?;
            if !ngo_entry.verified {
                return Err(Error::NgoNotVerified);
            }
        }

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&donor, env.current_contract_address(), &deposit);

        let stream_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextStreamId)
            .ok_or(Error::StreamCounterMissing)?;

        let now = env.ledger().timestamp();
        let stream = Stream {
            donor: donor.clone(),
            ngo: ngo.clone(),
            token: token.clone(),
            rate,
            balance: deposit,
            withdrawn: 0,
            created_at: now,
            last_update: now,
            status: StreamStatus::Active,
            cancelled: false,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);
        let next_stream_id = stream_id.checked_add(1).ok_or(Error::ArithmeticOverflow)?;
        env.storage()
            .instance()
            .set(&DataKey::NextStreamId, &next_stream_id);

        let donor_key = DataKey::DonorStreamCount(donor.clone());
        let new_count = donor_streams
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        env.storage().persistent().set(&donor_key, &new_count);
        env.storage().persistent().extend_ttl(
            &donor_key,
            STREAM_LIFETIME_THRESHOLD,
            STREAM_BUMP_AMOUNT,
        );

        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        env.events().publish(
            (symbol_short!("created"), stream_id),
            (donor, ngo, token, deposit, rate),
        );

        Ok(stream_id)
    }

    /// Pays out everything accrued to the NGO since the last checkpoint.
    /// NGO-auth-gated.
    ///
    /// Returns the net amount the NGO actually receives: the gross accrued
    /// amount minus any protocol fee routed to the treasury. With no
    /// treasury configured, or when the fee rounds down to zero, that equals
    /// the full accrued amount. The stream's `withdrawn` bookkeeping and the
    /// `withdraw` event still report the gross accrued value.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    ///
    /// // 50 seconds pass -> 10/s * 50 = 500 has accrued.
    /// env.ledger().with_mut(|l| l.timestamp += 50);
    ///
    /// // No treasury is configured here, so the return value is the full
    /// // accrued amount. With a fee configured it would be net of that fee.
    /// let withdrawn = client.withdraw(&stream_id);
    /// assert_eq!(withdrawn, 500);
    /// ```
    pub fn withdraw(env: Env, stream_id: u64) -> Result<i128, Error> {
        require_not_paused(&env)?;

        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreamNotFound)?;

        stream.ngo.require_auth();

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        let accrued = math::accrued(stream.rate, elapsed, stream.balance);

        if accrued <= 0 {
            return Err(Error::NothingToWithdraw);
        }

        record_payout(&mut stream, accrued)?;
        stream.last_update = now;
        if stream.balance == 0 {
            stream.status = StreamStatus::Drained;
        }
        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        let token_client = token::Client::new(&env, &stream.token);
        let net = pay_ngo(&env, &token_client, &stream.ngo, accrued);

        env.events()
            .publish((symbol_short!("withdraw"), stream_id), accrued);

        Ok(net)
    }

    /// Stops a stream for good: settles whatever has already accrued to the
    /// NGO, refunds the untouched remainder to the donor, then zeroes the
    /// stream's rate and balance. Donor-auth-gated. Returns the refunded
    /// amount.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// env.ledger().with_mut(|l| l.timestamp += 20); // 200 accrues
    ///
    /// // Settles the 200 already accrued to the NGO, refunds the
    /// // untouched 800 to the donor, and zeroes the stream out.
    /// let refund = client.cancel_stream(&stream_id);
    /// assert_eq!(refund, 800);
    /// assert_eq!(client.get_stream(&stream_id).balance, 0);
    /// assert!(client.get_stream(&stream_id).cancelled);
    /// ```
    pub fn cancel_stream(env: Env, stream_id: u64) -> Result<i128, Error> {
        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreamNotFound)?;

        stream.donor.require_auth();

        let now = env.ledger().timestamp();
        let accrued = settle(&env, &mut stream, now)?;

        let token_client = token::Client::new(&env, &stream.token);

        let refund = stream.balance;
        if refund > 0 {
            token_client.transfer(&env.current_contract_address(), &stream.donor, &refund);
        }

        stream.balance = 0;
        stream.rate = 0;
        stream.cancelled = true;
        stream.last_update = now;
        stream.status = StreamStatus::Cancelled;
        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        let grace_ledgers = env
            .storage()
            .instance()
            .get(&DataKey::CancelGraceLedgers)
            .unwrap_or(0);
        extend_cancelled_stream_ttl(&env, stream_id, grace_ledgers)?;

        // Only emit the cancel event when something actually moved. When
        // both values are zero the stream was already cancelled — emitting
        // here would produce a duplicate that an indexer can't distinguish
        // from a real cancellation (see issue #91).
        if accrued > 0 || refund > 0 {
            env.events()
                .publish((symbol_short!("cancel"), stream_id), (accrued, refund));
        }

        Ok(refund)
    }

    /// Adds more funds to an existing stream. Donor-auth-gated. Settles
    /// whatever has already accrued to the NGO first, so the top-up only
    /// ever affects accrual going forward. Fails with
    /// `Error::StreamCancelled` if `cancel_stream` has already closed the
    /// stream out.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &2_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// env.ledger().with_mut(|l| l.timestamp += 10); // 100 accrues and settles first
    ///
    /// client.top_up(&stream_id, &500);
    /// assert_eq!(client.get_stream(&stream_id).balance, 1_400); // 1000 - 100 + 500
    /// ```
    pub fn top_up(env: Env, stream_id: u64, amount: i128) -> Result<(), Error> {
        require_not_paused(&env)?;

        if amount <= 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreamNotFound)?;

        stream.donor.require_auth();

        if stream.cancelled {
            return Err(Error::StreamCancelled);
        }

        let token_client = token::Client::new(&env, &stream.token);

        let now = env.ledger().timestamp();
        let _accrued = settle(&env, &mut stream, now)?;

        token_client.transfer(&stream.donor, env.current_contract_address(), &amount);
        stream.balance = stream
            .balance
            .checked_add(amount)
            .ok_or(Error::ArithmeticOverflow)?;

        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        env.events()
            .publish((symbol_short!("topup"), stream_id), amount);

        Ok(())
    }

    /// Changes the per-second accrual rate on an existing stream. Donor-auth-gated.
    /// Settles whatever has already accrued at the old rate first, so the new
    /// rate only ever applies going forward — never retroactively. Fails
    /// with `Error::StreamCancelled` if `cancel_stream` has already closed
    /// the stream out — otherwise this would quietly revive it.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::{Address as _, Ledger}, token, Address, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let token_admin = Address::generate(&env);
    /// # let sac = env.register_stellar_asset_contract_v2(token_admin.clone());
    /// # let token_client = token::StellarAssetClient::new(&env, &sac.address());
    /// # let donor = Address::generate(&env);
    /// # let ngo = Address::generate(&env);
    /// # token_client.mint(&donor, &1_000);
    /// let stream_id = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// env.ledger().with_mut(|l| l.timestamp += 5); // 50 accrues at the old rate first
    ///
    /// client.modify_rate(&stream_id, &20);
    /// assert_eq!(client.get_stream(&stream_id).rate, 20);
    /// ```
    pub fn modify_rate(env: Env, stream_id: u64, new_rate: i128) -> Result<(), Error> {
        require_not_paused(&env)?;

        if new_rate <= 0 {
            return Err(Error::InvalidAmount);
        }

        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreamNotFound)?;

        stream.donor.require_auth();
        let old_rate = stream.rate;

        if stream.cancelled {
            return Err(Error::StreamCancelled);
        }

        let now = env.ledger().timestamp();
        let _accrued = settle(&env, &mut stream, now)?;
        stream.rate = new_rate;

        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        env.events()
            .publish((symbol_short!("ratemod"), stream_id), (old_rate, new_rate));

        Ok(())
    }

    /// Replaces the contract's Wasm bytecode in place. Admin-only.
    /// Lets a bug fix be deployed without changing the contract address,
    /// preserving every existing stream and configuration value.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, BytesN, Env};
    /// # use donation_vault::{DonationVault, DonationVaultClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(DonationVault, ());
    /// # let client = DonationVaultClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let new_wasm_hash = BytesN::from_array(&env, &[0u8; 32]);
    /// client.upgrade(&new_wasm_hash);
    /// ```
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), Error> {
        require_admin(&env)?;
        env.deployer().update_current_contract_wasm(new_wasm_hash);
        Ok(())
    }
}

mod test;
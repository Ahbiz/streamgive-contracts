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
    Map, Vec,
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
    /// The token allowlist surfaced to frontend token pickers. See
    /// [`allowed_tokens`](DonationVault::allowed_tokens); empty until an
    /// operator configures one.
    AllowedTokens,
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
    /// Per-token protocol fee override, in basis points. Absent for a given
    /// token means "use the global `FeeBps`". See `set_token_fee_bps`.
    TokenFeeBps(Address),
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
    /// The donor has already reached `max_streams_per_donor`'s cap.
    StreamLimitExceeded = 16,
    /// A registry is configured, but the target NGO is unknown to it or not
    /// marked verified.
    NgoNotVerified = 17,
    /// `DataKey::NextStreamId` was missing from instance storage. `init`
    /// always sets it, so this should never happen in practice; returned
    /// rather than defaulting to `0`, which could collide with an existing
    /// stream.
    StreamCounterMissing = 18,
    /// `rescue_stream` was called while the vault is not paused. It only
    /// exists for incident response, not as an ordinary way to close a
    /// stream out.
    NotPaused = 19,
    /// The admin has renounced control, so admin-gated calls are permanently
    /// disabled.
    AdminRenounced = 20,
    /// `withdraw_batch` was handed streams that don't all belong to the same
    /// NGO. Payouts are aggregated per token, so a single batch can only ever
    /// pay one NGO.
    MixedNgo = 21,
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

/// Approximate seconds per ledger, matching `DAY_IN_LEDGERS`'s own implied
/// close time (17_280 ledgers/day * 5s = 86_400s). Used only to convert a
/// stream's remaining lifetime from seconds to ledgers for the TTL bump
/// below; if the network's real close time drifts from this, the bump is
/// only ever approximate, and it already floors at the normal 90-day
/// default regardless (see issue #197).
const SECONDS_PER_LEDGER: u64 = 5;

/// Keeps a stream's persistent entry alive for at least 90 days past its
/// last touch (issue #197). A stream with a long way left to drain at its
/// current rate — `balance / rate` seconds — gets a longer bump instead,
/// so a slow trickle nobody happens to interact with doesn't need an
/// external `extend_stream` call before the *normal* 90-day window would
/// have expired it. Never bumps past the network's own `max_ttl()`, since
/// requesting more than that errors instead of clamping.
///
/// This mitigates the risk described in issue #197, it does not eliminate
/// it: a stream whose remaining lifetime is *longer* than the network's
/// max TTL (independent of anything this contract can request) still needs
/// an eventual `extend_stream` call, same as before. See the "Why does
/// each stream have its own TTL?" section of the README for the full
/// picture, including that residual case.
fn extend_stream_ttl(env: &Env, stream_id: u64, rate: i128, balance: i128) {
    let key = DataKey::Stream(stream_id);

    let lifetime_bump = if rate > 0 && balance > 0 {
        let remaining_seconds = (balance / rate).max(0) as u64;
        let remaining_ledgers = remaining_seconds / SECONDS_PER_LEDGER;
        remaining_ledgers.min(u64::from(u32::MAX)) as u32
    } else {
        0
    };

    let bump_amount = STREAM_BUMP_AMOUNT
        .max(lifetime_bump)
        .min(env.storage().max_ttl());
    let threshold = bump_amount.saturating_sub(DAY_IN_LEDGERS);

    env.storage()
        .persistent()
        .extend_ttl(&key, threshold, bump_amount);
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

/// Returns the protocol fee that would be taken on `amount` of `token`,
/// using the same logic as `pay_ngo`. Zero when no treasury is configured,
/// regardless of `fee_bps` — there's nowhere to send a fee without a
/// destination address. Rounds toward zero (the NGO never loses a unit to
/// rounding).
fn compute_fee(env: &Env, token: &Address, amount: i128) -> i128 {
    let treasury: Option<Address> = env.storage().instance().get(&DataKey::Treasury);
    match treasury {
        Some(_) => {
            let fee_bps = effective_fee_bps(env, token);
            (amount.saturating_mul(fee_bps as i128) / 10_000).min(amount)
        }
        None => 0,
    }
}

/// The fee, in basis points, that actually applies to `token`: its
/// admin-configured override if one exists (`set_token_fee_bps`), otherwise
/// the global default (`set_fee_bps`).
fn effective_fee_bps(env: &Env, token: &Address) -> u32 {
    env.storage()
        .persistent()
        .get(&DataKey::TokenFeeBps(token.clone()))
        .unwrap_or_else(|| env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0))
}

/// Pays `amount` of `token` out to the NGO, skimming a protocol fee to the
/// treasury first if one is configured. With no treasury set, the full
/// amount goes to the NGO regardless of `fee_bps` — there's nowhere to send
/// a fee.
///
/// Returns the net amount actually transferred to the NGO. This is the
/// single place the fee split is computed, so callers that report the
/// payout to their own callers (`withdraw`) can return exactly what the
/// NGO received rather than recomputing the fee and risking drift.
fn pay_ngo(
    env: &Env,
    token_client: &token::Client,
    token: &Address,
    ngo: &Address,
    amount: i128,
) -> i128 {
    if amount <= 0 {
        return 0;
    }

    let fee = compute_fee(env, token, amount);
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
        pay_ngo(env, &token_client, &stream.token, &stream.ngo, accrued);
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

    /// Permanently gives up admin control. Admin-authed.
    ///
    /// Clears the stored admin and any pending admin proposal. After this
    /// call every admin-gated entry point fails with
    /// `Error::AdminRenounced`, so the admin-gated surface is permanently
    /// disabled. This cannot be undone.
    ///
    /// # Examples
    ///
    /// 
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

    /// Reads back several streams by id in a single call, so a client can fetch
    /// a page of streams without one RPC round-trip per id.
    ///
    /// Unlike `get_stream`, a missing id doesn't fail the call: it comes back
    /// as `None` in the same position, letting a caller page through ids that
    /// may include ones that were never created.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, token, Address, Env, Vec};
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
    /// let a = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &10);
    /// let b = client.create_stream(&donor, &ngo, &sac.address(), &1_000, &20);
    ///
    /// let mut ids = Vec::new(&env);
    /// ids.push_back(a);
    /// ids.push_back(999); // never created
    /// ids.push_back(b);
    ///
    /// let streams = client.get_streams(&ids);
    /// assert!(streams.get(0).unwrap().is_some());
    /// assert!(streams.get(1).unwrap().is_none());
    /// assert_eq!(streams.get(2).unwrap().unwrap().rate, 20);
    /// ```
    pub fn get_streams(env: Env, ids: Vec<u64>) -> Vec<Option<Stream>> {
        let mut streams: Vec<Option<Stream>> = Vec::new(&env);
        for id in ids.iter() {
            streams.push_back(env.storage().persistent().get(&DataKey::Stream(id)));
        }
        streams
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

    /// Read-only lookup of the ledger timestamp at which a stream's balance
    /// runs out, so every client gets the same answer with the rounding done
    /// in one place. The stream's `balance` counts everything not yet paid
    /// out (including what's accrued but unwithdrawn) and `last_update` is
    /// when it was last settled, so this is `last_update` plus the seconds
    /// `balance` takes at `rate`, rounded up — a partial final second counts
    /// as a whole one.
    ///
    /// Returns `None` when the stream will never deplete: a cancelled or
    /// zero-rate stream, or a timestamp too far out to represent. An already
    /// empty stream that still has a rate returns its `last_update`.
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
    /// # use soroban_sdk::{testutils::Address as _, token,
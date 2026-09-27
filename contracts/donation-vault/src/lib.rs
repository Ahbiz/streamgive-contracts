#![no_std]
// soroban-sdk 27 deprecates Events::publish in favour of the
// #[contractevent] macro. Migrating is not a lint cleanup: #[contractevent]
// derives its own topic/data layout, and streamgive-backend's indexer
// decodes the current layout by hand (topic[0] = symbol, topic[1] = id),
// as does docs/EVENTS.md. Both repos have to move in the same change, so
// it is tracked as its own issue rather than done under -D warnings here.
#![allow(deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, token, Address, Env,
};

mod math;

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
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    PendingAdmin,
    NextStreamId,
    Stream(u64),
    Paused,
    Treasury,
    FeeBps,
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
    /// The donor and the NGO are the same address, so the stream would pay
    /// the donor back their own deposit. Rejected at creation: a stream that
    /// nets to zero still counts as a committed donation in the indexer and
    /// on impact pages, which is a way to inflate those totals for free.
    SelfStream = 10,
}

/// Fee cap of 10%, enforced by `set_fee_bps` so the admin can never take
/// an unreasonable cut of donations.
const MAX_FEE_BPS: u32 = 1_000;

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

/// Pays `amount` out to the NGO, skimming a protocol fee to the treasury
/// first if one is configured. With no treasury set, the full amount goes
/// to the NGO regardless of `fee_bps` — there's nowhere to send a fee.
fn pay_ngo(env: &Env, token_client: &token::Client, ngo: &Address, amount: i128) {
    if amount <= 0 {
        return;
    }

    let treasury: Option<Address> = env.storage().instance().get(&DataKey::Treasury);
    let fee = match &treasury {
        Some(_) => {
            let fee_bps: u32 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
            (amount.saturating_mul(fee_bps as i128) / 10_000).min(amount)
        }
        None => 0,
    };
    let net = amount - fee;

    if net > 0 {
        token_client.transfer(&env.current_contract_address(), ngo, &net);
    }
    if fee > 0 {
        if let Some(treasury) = treasury {
            token_client.transfer(&env.current_contract_address(), &treasury, &fee);
        }
    }
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
    /// yet been accepted or cancelled. Lets the proposed admin (or anyone
    /// else) check whether there's something to accept without having to
    /// watch for the `propadmin` event.
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
    /// Requires the current admin's auth. Has no effect on who can act as
    /// admin until `accept_admin` is called by the proposed address.
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
        require_admin(&env)?;

        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        extend_instance_ttl(&env);

        env.events()
            .publish((symbol_short!("propadmin"),), new_admin);

        Ok(())
    }

    /// Completes a two-step admin transfer. Requires the proposed admin's
    /// auth. Fails with `Error::NoPendingAdmin` if `propose_admin` was never
    /// called, or has already been completed.
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

    /// Withdraws a pending admin proposal, leaving nothing pending. Requires
    /// the current admin's auth. Fails with `Error::NoPendingAdmin` if
    /// `propose_admin` was never called, or the proposal was already
    /// accepted or cancelled.
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
    /// ```
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Stream(stream_id))
            .ok_or(Error::StreamNotFound)
    }

    /// Reads back the number of streams ever created — the exclusive upper
    /// bound on valid stream ids. Lets a client enumerate streams (ids `0`
    /// through `stream_count() - 1`) or just show a running total, without
    /// exposing the raw `NextStreamId` counter directly.
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
    /// Reuses the same math `withdraw` would use to pay out, but never
    /// mutates storage or moves funds — safe to call as often as needed.
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

    /// Bumps a stream's persistent-storage TTL without touching its state.
    /// Callable by anyone — donor, NGO, or a keeper bot — so a slow,
    /// long-running stream that nobody happens to write to doesn't get
    /// archived out from under its funds between activity.
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
    /// Admin-gated emergency brake; existing balances stay put and
    /// `cancel_stream` still works so donors can always get a refund.
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

    /// Sets the protocol fee, in basis points, taken out of accrued payouts
    /// to the NGO. Admin-gated, capped at `MAX_FEE_BPS`. Has no effect
    /// unless a treasury is also set.
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

        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&donor, env.current_contract_address(), &deposit);

        let stream_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextStreamId)
            .unwrap_or(0);

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
        };

        env.storage()
            .persistent()
            .set(&DataKey::Stream(stream_id), &stream);
        let next_stream_id = stream_id
            .checked_add(1)
            .ok_or(Error::ArithmeticOverflow)?;
        env.storage()
            .instance()
            .set(&DataKey::NextStreamId, &next_stream_id);

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
        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        let token_client = token::Client::new(&env, &stream.token);
        pay_ngo(&env, &token_client, &stream.ngo, accrued);

        env.events()
            .publish((symbol_short!("withdraw"), stream_id), accrued);

        Ok(accrued)
    }

    /// Stops a stream for good: settles whatever has already accrued to the
    /// NGO (so cancelling doesn't claw back funds already earned), refunds
    /// the untouched remainder to the donor, then zeroes the stream's rate
    /// and balance. Donor-auth-gated. The record is kept, not deleted, so
    /// the stream's history stays queryable.
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
    /// client.cancel_stream(&stream_id);
    /// assert_eq!(client.get_stream(&stream_id).balance, 0);
    /// ```
    pub fn cancel_stream(env: Env, stream_id: u64) -> Result<(), Error> {
        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::StreamNotFound)?;

        stream.donor.require_auth();

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        let accrued = math::accrued(stream.rate, elapsed, stream.balance);

        let token_client = token::Client::new(&env, &stream.token);

        if accrued > 0 {
            pay_ngo(&env, &token_client, &stream.ngo, accrued);
            record_payout(&mut stream, accrued)?;
        }

        let refund = stream.balance;
        if refund > 0 {
            token_client.transfer(&env.current_contract_address(), &stream.donor, &refund);
        }

        stream.balance = 0;
        stream.rate = 0;
        stream.last_update = now;
        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        env.events()
            .publish((symbol_short!("cancel"), stream_id), (accrued, refund));

        Ok(())
    }

    /// Adds more funds to an existing stream. Donor-auth-gated. Settles
    /// whatever has already accrued to the NGO first, so the top-up only
    /// ever affects accrual going forward.
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

        let token_client = token::Client::new(&env, &stream.token);

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        let accrued = math::accrued(stream.rate, elapsed, stream.balance);
        if accrued > 0 {
            pay_ngo(&env, &token_client, &stream.ngo, accrued);
            record_payout(&mut stream, accrued)?;
        }
        stream.last_update = now;

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
    /// rate only ever applies going forward — never retroactively.
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

        let now = env.ledger().timestamp();
        let elapsed = now.saturating_sub(stream.last_update);
        let accrued = math::accrued(stream.rate, elapsed, stream.balance);
        if accrued > 0 {
            let token_client = token::Client::new(&env, &stream.token);
            pay_ngo(&env, &token_client, &stream.ngo, accrued);
            record_payout(&mut stream, accrued)?;
        }
        stream.last_update = now;
        stream.rate = new_rate;

        env.storage().persistent().set(&key, &stream);
        extend_instance_ttl(&env);
        extend_stream_ttl(&env, stream_id);

        env.events()
            .publish((symbol_short!("ratemod"), stream_id), new_rate);

        Ok(())
    }
}

mod test;

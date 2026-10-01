// SPDX-License-Identifier: Apache-2.0
#![cfg(test)]

use super::*;
use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, Events as _, Ledger, MockAuth, MockAuthInvoke,
};
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::xdr::{ContractEventBody, ScVal, ScVec};
use soroban_sdk::{IntoVal, Symbol, TryFromVal, Val, Vec};

/// The most recently published event, in XDR form. `Val` has no `PartialEq`,
/// so it compares against an expected `(topics, data)` pair by converting
/// that pair to XDR too.
struct LastEvent {
    env: Env,
    topics: ScVal,
    data: ScVal,
}

impl core::fmt::Debug for LastEvent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "({:?}, {:?})", self.topics, self.data)
    }
}

impl PartialEq<(Vec<Val>, Val)> for LastEvent {
    fn eq(&self, (topics, data): &(Vec<Val>, Val)) -> bool {
        ScVal::try_from_val(&self.env, &topics.to_val()).unwrap() == self.topics
            && ScVal::try_from_val(&self.env, data).unwrap() == self.data
    }
}

/// The topics and data of the most recently published event, regardless of
/// which contract emitted it — vault entry points always publish their own
/// event last, after any token transfer, so this is the vault's event.
fn last_event(env: &Env) -> LastEvent {
    let all = env.events().all();
    let ContractEventBody::V0(body) = &all.events().last().unwrap().body;
    LastEvent {
        env: env.clone(),
        topics: ScVal::Vec(Some(ScVec(body.topics.clone()))),
        data: body.data.clone(),
    }
}

/// The number of events emitted by `contract`. Filters by contract address
/// so token transfers firing inside the same invocation don't inflate the
/// count, and only covers the most recent invocation.
fn event_count(env: &Env, contract: &Address) -> usize {
    env.events()
        .all()
        .filter_by_contract(contract)
        .events()
        .len()
}

/// Builds a `Vec<u64>` from a Rust slice, for the batch-get / batch-withdraw
/// entry points that take an id list.
fn stream_ids(env: &Env, ids: &[u64]) -> Vec<u64> {
    let mut v = Vec::new(env);
    for id in ids {
        v.push_back(*id);
    }
    v
}

fn create_token<'a>(env: &Env, admin: &Address) -> (TokenClient<'a>, StellarAssetClient<'a>) {
    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    (
        TokenClient::new(env, &sac.address()),
        StellarAssetClient::new(env, &sac.address()),
    )
}

struct Setup<'a> {
    env: Env,
    client: DonationVaultClient<'a>,
    token: TokenClient<'a>,
    token_admin: StellarAssetClient<'a>,
    donor: Address,
    ngo: Address,
}

fn setup() -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();

    // New entries (the token's included) start with at least 20 days of
    // TTL, so the TTL tests can age the ledger past the vault's bump
    // thresholds without archiving the token out from under a transfer.
    // 20 days is still below both thresholds, so the vault's own bumps on
    // init and create_stream are what set its entries' TTLs.
    env.ledger().with_mut(|l| {
        l.min_persistent_entry_ttl = 20 * DAY_IN_LEDGERS;
        l.max_entry_ttl = 365 * DAY_IN_LEDGERS;
    });

    let contract_id = env.register(DonationVault, ());
    let client = DonationVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.init(&admin);

    let token_issuer = Address::generate(&env);
    let (token, token_admin) = create_token(&env, &token_issuer);

    let donor = Address::generate(&env);
    let ngo = Address::generate(&env);

    Setup {
        env,
        client,
        token,
        token_admin,
        donor,
        ngo,
    }
}

#[test]
fn full_lifecycle_create_accrue_withdraw_cancel() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    // Capture the event before any other contract call: the SDK only exposes
    // the events of the most recent invocation.
    let created = last_event(&s.env);

    // `last_event` only sees the latest top-level call, so assert it before
    // any other call (such as a balance read) replaces it.
    let created = last_event(&s.env);
    assert_eq!(
        created,
        (
            (symbol_short!("created"), stream_id).into_val(&s.env),
            (
                s.donor.clone(),
                s.ngo.clone(),
                s.token.address.clone(),
                1_000i128,
                10i128
            )
                .into_val(&s.env),
        )
    );
    assert_eq!(s.token.balance(&s.donor), 0);
    assert_eq!(s.token.balance(&s.client.address), 1_000);

    assert_eq!(s.token.balance(&s.donor), 0);
    assert_eq!(s.token.balance(&s.client.address), 1_000);

    // 50 seconds pass -> 10/s * 50 = 500 should be withdrawable.
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let withdrawn = s.client.withdraw(&stream_id);
    let withdrew = last_event(&s.env);
    assert_eq!(withdrawn, 500);
    assert_eq!(
        withdrew,
        (
            (symbol_short!("withdraw"), stream_id).into_val(&s.env),
            500i128.into_val(&s.env),
        )
    );
    assert_eq!(s.token.balance(&s.ngo), 500);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 500);
    assert_eq!(stream.withdrawn, 500);

    // 20 more seconds pass, then the donor cancels.
    s.env.ledger().with_mut(|l| l.timestamp += 20);
    // 200 more settles to the NGO on cancel; the untouched 300 refunds to the donor.
    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 300);
    let cancelled = last_event(&s.env);
    assert_eq!(
        cancelled,
        (
            (symbol_short!("cancel"), stream_id).into_val(&s.env),
            (200i128, 300i128).into_val(&s.env),
        )
    );
    assert_eq!(s.token.balance(&s.ngo), 700);
    assert_eq!(s.token.balance(&s.donor), 300);

    // 200 more settles to the NGO on cancel; the untouched 300 refunds to the donor.
    assert_eq!(s.token.balance(&s.ngo), 700);
    assert_eq!(s.token.balance(&s.donor), 300);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.rate, 0);
    assert!(stream.cancelled);
}

#[test]
fn one_stroop_per_second_stream_pays_and_rounds_fee_to_zero() {
    let s = setup();
    s.token_admin.mint(&s.donor, &100);
    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &100, &1);
    s.env.ledger().with_mut(|l| l.timestamp += 1);

    assert_eq!(s.client.withdraw(&stream_id), 1);
    assert_eq!(s.token.balance(&s.ngo), 1);
    assert_eq!(s.token.balance(&treasury), 0);
}

#[test]
fn concurrent_streams_to_one_ngo_accrue_and_pay_out_independently() {
    let s = setup();
    let donor_b = Address::generate(&s.env);
    s.token_admin.mint(&s.donor, &1_000);
    s.token_admin.mint(&donor_b, &600);

    // Stream A starts first; stream B starts 20 seconds later at a
    // different rate, so the two accruals differ.
    let id_a = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 20);
    let id_b = s
        .client
        .create_stream(&donor_b, &s.ngo, &s.token.address, &600, &5);
    assert_ne!(id_a, id_b);
    assert_eq!(s.token.balance(&s.client.address), 1_600);

    // 30 seconds later: A has run 50s (500), B has run 30s (150).
    s.env.ledger().with_mut(|l| l.timestamp += 30);
    assert_eq!(s.client.pending_accrual(&id_a), 500);
    assert_eq!(s.client.pending_accrual(&id_b), 150);

    // Withdrawing A pays exactly A's accrual and leaves B untouched.
    assert_eq!(s.client.withdraw(&id_a), 500);
    assert_eq!(s.token.balance(&s.ngo), 500);
    assert_eq!(s.client.pending_accrual(&id_b), 150);
    let stream_b = s.client.get_stream(&id_b);
    assert_eq!(stream_b.balance, 600);
    assert_eq!(stream_b.withdrawn, 0);

    // Withdrawing B pays exactly B's accrual and leaves A untouched.
    assert_eq!(s.client.withdraw(&id_b), 150);
    assert_eq!(s.token.balance(&s.ngo), 650);

    let stream_a = s.client.get_stream(&id_a);
    assert_eq!(stream_a.balance, 500);
    assert_eq!(stream_a.withdrawn, 500);
    let stream_b = s.client.get_stream(&id_b);
    assert_eq!(stream_b.balance, 450);
    assert_eq!(stream_b.withdrawn, 150);
    assert_eq!(s.token.balance(&s.client.address), 950);

    // Both keep accruing at their own rates after the other's withdrawal.
    s.env.ledger().with_mut(|l| l.timestamp += 10);
    assert_eq!(s.client.pending_accrual(&id_a), 100);
    assert_eq!(s.client.pending_accrual(&id_b), 50);
}

#[test]
fn top_up_and_modify_rate_settle_before_changing() {
    let s = setup();
    s.token_admin.mint(&s.donor, &2_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.env.ledger().with_mut(|l| l.timestamp += 10); // 100 accrues

    s.client.top_up(&stream_id, &500);
    let topped_up = last_event(&s.env);

    assert_eq!(
        topped_up,
        (
            (symbol_short!("topup"), stream_id).into_val(&s.env),
            500i128.into_val(&s.env),
        )
    );
    assert_eq!(s.token.balance(&s.ngo), 100);
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_400); // 1000 - 100 accrued + 500 top-up
    assert_eq!(stream.rate, 10);

    s.env.ledger().with_mut(|l| l.timestamp += 5); // 50 more accrues at the old rate

    s.client.modify_rate(&stream_id, &20);
    let rate_changed = last_event(&s.env);

    assert_eq!(
        rate_changed,
        (
            (symbol_short!("ratemod"), stream_id).into_val(&s.env),
            (10i128, 20i128).into_val(&s.env),
        )
    );
    assert_eq!(s.token.balance(&s.ngo), 150);
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.rate, 20);
    assert_eq!(stream.balance, 1_350); // 1400 - 50
}

#[test]
fn created_at_is_set_once_and_never_changes() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let stream = s.client.get_stream(&stream_id);
    let created_at = stream.created_at;
    assert_eq!(created_at, stream.last_update);

    // Withdraw, top-up, and modify_rate all move last_update forward, but
    // none of them should touch created_at.
    s.env.ledger().with_mut(|l| l.timestamp += 50);
    s.client.withdraw(&stream_id);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.created_at, created_at);
    assert_ne!(stream.last_update, created_at);
}

#[test]
fn create_stream_rejects_non_positive_amounts() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let result = s
        .client
        .try_create_stream(&s.donor, &s.ngo, &s.token.address, &0, &10);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    let result = s
        .client
        .try_create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn create_stream_errors_instead_of_defaulting_when_counter_is_missing() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    assert_eq!(s.client.min_deposit(), 0);

    // `init` always sets NextStreamId, so this shouldn't happen in
    // practice — but nothing enforces that, and if the counter were ever
    // missing, silently treating it as `0` could collide with an existing
    // stream. Simulate that by removing it directly from instance storage.
    s.env.as_contract(&s.client.address, || {
        s.env.storage().instance().remove(&DataKey::NextStreamId);
    });

    let result = s
        .client
        .try_create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert_eq!(result, Err(Ok(Error::StreamCounterMissing)));

    // No stream should have been recorded under the fabricated id 0.
    assert_eq!(
        s.client.try_get_stream(&0u64),
        Err(Ok(Error::StreamNotFound))
    );
}

#[test]
fn propose_then_accept_admin_transfers_control() {
    let s = setup();
    let old_admin = s.client.admin();
    let new_admin = Address::generate(&s.env);

    s.client.propose_admin(&new_admin);
    let proposed = last_event(&s.env);
    assert_eq!(
        proposed,
        (
            (symbol_short!("propadmin"),).into_val(&s.env),
            new_admin.into_val(&s.env),
        )
    );
    // Admin hasn't changed yet — only proposed.
    assert_eq!(s.client.admin(), old_admin);

    s.client.accept_admin();
    let accepted = last_event(&s.env);
    assert_eq!(
        accepted,
        (
            (symbol_short!("acptadmin"),).into_val(&s.env),
            new_admin.into_val(&s.env),
        )
    );
    assert_eq!(s.client.admin(), new_admin);

    // The new admin can act as admin.
    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    assert_eq!(s.client.treasury(), Some(treasury));
}

#[test]
fn propose_admin_rejects_current_admin() {
    let s = setup();
    let admin = s.client.admin();

    assert_eq!(
        s.client.try_propose_admin(&admin),
        Err(Ok(Error::InvalidAdmin))
    );
    assert_eq!(s.client.pending_admin(), None);
}

#[test]
fn allowed_tokens_is_empty_when_unconfigured() {
    let s = setup();

    assert_eq!(s.client.allowed_tokens().len(), 0);
}

#[test]
fn accept_admin_without_proposal_fails() {
    let s = setup();
    let result = s.client.try_accept_admin();
    assert_eq!(result, Err(Ok(Error::NoPendingAdmin)));
}

// ── Issue #72 ─────────────────────────────────────────────────────────────────
// propose_admin overwrites rather than queues, so the first proposed address
// is silently dropped. That's the desired behaviour, but nothing pinned it:
// a future change to "keep the earliest proposal" or "reject a second one"
// would lock an admin out with no way to tell from the outside.

#[test]
fn repropose_admin_overwrites_earlier_proposal() {
    let s = setup();
    let old_admin = s.client.admin();
    let admin_a = Address::generate(&s.env);
    let admin_b = Address::generate(&s.env);

    s.client.propose_admin(&admin_a);
    assert_eq!(s.client.pending_admin(), Some(admin_a.clone()));

    // Proposing again replaces the pending address instead of queueing.
    s.client.propose_admin(&admin_b);
    assert_eq!(
        s.client.pending_admin(),
        Some(admin_b.clone()),
        "the second proposal must overwrite the first, not queue behind it"
    );

    // A is no longer the pending admin, so accept_admin now requires B's auth
    // and not A's.
    s.client.accept_admin();
    assert_auth_required_from(&s, &admin_b, "accept_admin");

    // Control actually moved to B, and only to B.
    assert_eq!(s.client.admin(), admin_b);
    assert_ne!(s.client.admin(), old_admin);
    assert_ne!(s.client.admin(), admin_a);
}

#[test]
fn cancel_admin_proposal_without_proposal_fails() {
    let s = setup();
    let result = s.client.try_cancel_admin_proposal();
    assert_eq!(result, Err(Ok(Error::NoPendingAdmin)));
}

#[test]
#[should_panic]
fn old_admin_loses_admin_gated_access_after_transfer() {
    let s = setup();
    let old_admin = s.client.admin();
    let new_admin = Address::generate(&s.env);

    s.client.propose_admin(&new_admin);
    s.client.accept_admin();

    // set_treasury requires the current admin's auth; only the old admin
    // authorizes this call, and the old admin is no longer admin.
    let treasury = Address::generate(&s.env);
    s.env.mock_auths(&[MockAuth {
        address: &old_admin,
        invoke: &MockAuthInvoke {
            contract: &s.client.address,
            fn_name: "set_treasury",
            args: (treasury.clone(),).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);
    s.client.set_treasury(&treasury);
}

#[test]
fn pending_accrual_matches_withdraw_without_mutating_state() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    // 50 seconds pass -> 10/s * 50 = 500 should be pending.
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let pending = s.client.pending_accrual(&stream_id);
    assert_eq!(pending, 500);

    // Checking pending_accrual must not move funds or touch the stream.
    assert_eq!(s.token.balance(&s.ngo), 0);
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_000);
    assert_eq!(stream.withdrawn, 0);

    // It should match exactly what withdraw actually pays out.
    let withdrawn = s.client.withdraw(&stream_id);
    assert_eq!(withdrawn, pending);
}

#[test]
fn pending_payout_with_no_treasury_reports_zero_fee() {
fn depletion_time_is_last_update_plus_balance_over_rate() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 0); // no treasury -> no fee, regardless of fee_bps
    assert_eq!(net, 500); // full accrual goes to the NGO
}

#[test]
fn pending_payout_with_zero_fee_bps_reports_zero_fee() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    // fee_bps defaults to 0 -- left unset here deliberately.

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 0); // 0 bps -> zero fee even with a treasury set
    assert_eq!(net, 500);
}

#[test]
fn pending_payout_splits_protocol_fee_with_treasury_and_fee_bps_set() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500); // 5%

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 25); // 5% of 500
    assert_eq!(net, 475); // 500 - 25
}

#[test]
fn withdraw_with_nothing_accrued_fails() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let result = s.client.try_withdraw(&stream_id);
    assert_eq!(result, Err(Ok(Error::NothingToWithdraw)));
}

#[test]
fn withdraw_immediately_after_top_up_fails() {
    let s = setup();
    s.token_admin.mint(&s.donor, &2_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.env.ledger().with_mut(|l| l.timestamp += 10); // 100 accrues

    // top_up settles the accrued 100 to the NGO internally.
    s.client.top_up(&stream_id, &500);
    assert_eq!(s.token.balance(&s.ngo), 100);

    // No time has passed since the settlement, so nothing new has accrued.
    let result = s.client.try_withdraw(&stream_id);
    assert_eq!(result, Err(Ok(Error::NothingToWithdraw)));
    assert_eq!(s.token.balance(&s.ngo), 100);
}

#[test]
fn pause_blocks_create_but_not_cancel() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.client.pause();
    let paused_evt = last_event(&s.env);
    assert_eq!(
        paused_evt,
        (
            (symbol_short!("pause"),).into_val(&s.env),
            ().into_val(&s.env),
        )
    );
    assert!(s.client.paused());

    let result = s
        .client
        .try_create_stream(&s.donor, &s.ngo, &s.token.address, &100, &10);
    assert_eq!(result, Err(Ok(Error::ContractPaused)));

    // Cancelling still works while paused, so donors are never trapped.
    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 1_000);
    assert_eq!(s.token.balance(&s.donor), 1_000);
}

#[test]
fn pause_blocks_withdraw_but_not_cancel() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 has accrued

    s.client.pause();

    // withdraw is the one entry point that pays tokens straight out of the
    // vault, so the brake has to stop it even with funds already waiting to
    // be claimed — otherwise pausing buys no protection at all.
    let result = s.client.try_withdraw(&stream_id);
    assert_eq!(result, Err(Ok(Error::ContractPaused)));

    // The rejected call is a no-op: nothing moves, and the accrual it would
    // have settled stays on the stream for after the pause is lifted.
    assert_eq!(s.token.balance(&s.ngo), 0);
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_000);
    assert_eq!(stream.withdrawn, 0);

    // Cancelling still works while paused, so donors are never trapped.
    s.client.cancel_stream(&stream_id);
    assert_eq!(s.token.balance(&s.ngo), 500);
    assert_eq!(s.token.balance(&s.donor), 500);
}

#[test]
fn rescue_stream_rejects_when_not_paused() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let result = s.client.try_rescue_stream(&stream_id);
    assert_eq!(result, Err(Ok(Error::NotPaused)));

    // Rejected: nothing moved, stream untouched.
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_000);
    assert_eq!(stream.status, StreamStatus::Active);
}

#[test]
fn rescue_stream_requires_admin_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.pause();

    // The donor, not the admin, tries to invoke it -- require_admin's own
    // auth check should reject this regardless of who signs.
    s.env.mock_auths(&[]);
    let result = s.client.try_rescue_stream(&stream_id);
    assert!(result.is_err());
}

#[test]
fn rescue_stream_refunds_full_balance_without_settling_accrual() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 20); // 200 would accrue under a normal cancel

    s.client.pause();
    let refund = s.client.rescue_stream(&stream_id);

    // The full deposit comes back to the donor -- unlike cancel_stream,
    // nothing is settled to the NGO first.
    assert_eq!(refund, 1_000);
    assert_eq!(s.token.balance(&s.donor), 1_000);
    assert_eq!(s.token.balance(&s.ngo), 0);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.rate, 0);
    assert!(stream.cancelled);
    assert_eq!(stream.status, StreamStatus::Cancelled);
}

#[test]
fn rescue_stream_emits_a_distinct_event() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.client.pause();
    let refund = s.client.rescue_stream(&stream_id);

    assert_eq!(
        last_event(&s.env),
        (
            (symbol_short!("rescue"), stream_id).into_val(&s.env),
            refund.into_val(&s.env),
        )
    );
}

#[test]
fn rescue_stream_rejects_an_already_cancelled_stream() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.cancel_stream(&stream_id);

    s.client.pause();
    let result = s.client.try_rescue_stream(&stream_id);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));
}

#[test]
fn rescue_stream_on_unknown_stream_fails() {
    let s = setup();
    s.client.pause();
    let result = s.client.try_rescue_stream(&999u64);
    assert_eq!(result, Err(Ok(Error::StreamNotFound)));
}

#[test]
fn unpause_restores_normal_operation() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    s.client.pause();
    s.client.unpause();
    let unpaused_evt = last_event(&s.env);
    assert_eq!(
        unpaused_evt,
        (
            (symbol_short!("unpause"),).into_val(&s.env),
            ().into_val(&s.env),
        )
    );
    assert!(!s.client.paused());

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_000);
}

#[test]
fn pause_rejects_duplicate_pause() {
    let s = setup();

    s.client.pause();
    assert_eq!(s.client.try_pause(), Err(Ok(Error::AlreadyPaused)));
    assert!(s.client.paused());
}

#[test]
fn unpause_rejects_duplicate_unpause() {
    let s = setup();

    assert_eq!(s.client.try_unpause(), Err(Ok(Error::AlreadyUnpaused)));
    s.client.pause();
    s.client.unpause();
    assert_eq!(s.client.try_unpause(), Err(Ok(Error::AlreadyUnpaused)));
    assert!(!
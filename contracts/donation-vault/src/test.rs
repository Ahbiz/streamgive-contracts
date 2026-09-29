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

    // `last_event` only sees the latest top-level call, so assert it before
    // any other call (such as a balance read) replaces it.
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
            .into_val(&s.env),
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
fn unpause_restores_normal_operation() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    s.client.pause();
    s.client.unpause();
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
    assert!(!s.client.paused());
}

#[test]
fn withdraw_with_no_treasury_takes_no_fee() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    s.client.set_fee_bps(&500); // configured, but no treasury yet

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let withdrawn = s.client.withdraw(&stream_id);
    assert_eq!(withdrawn, 500);
    assert_eq!(s.token.balance(&s.ngo), 500);
}

#[test]
fn clear_treasury_stops_fee_collection_on_subsequent_withdraws() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500); // 5%

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    // First withdraw while treasury is active — fee is taken.
    s.env.ledger().with_mut(|l| l.timestamp += 20); // 200 accrues
    s.client.withdraw(&stream_id);
    assert_eq!(s.token.balance(&treasury), 10); // 5% of 200
    assert_eq!(s.token.balance(&s.ngo), 190);

    // Admin clears the treasury; storage key is gone.
    s.client.clear_treasury();
    assert_eq!(s.client.treasury(), None);

    // Second withdraw — full amount goes to the NGO, nothing to the old treasury.
    s.env.ledger().with_mut(|l| l.timestamp += 20); // 200 more accrues
    s.client.withdraw(&stream_id);
    assert_eq!(s.token.balance(&s.ngo), 390); // 190 + 200, no fee
    assert_eq!(s.token.balance(&treasury), 10); // unchanged
}

#[test]
fn withdraw_splits_protocol_fee_to_treasury() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    assert_last_event(
        &s.env,
        (symbol_short!("treasury"),).into_val(&s.env),
        treasury.clone().into_val(&s.env),
    );
    s.client.set_fee_bps(&500); // 5%
    assert_last_event(
        &s.env,
        (symbol_short!("feebps"),).into_val(&s.env),
        500u32.into_val(&s.env),
    );

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues

    let withdrawn = s.client.withdraw(&stream_id);
    // The return value is the net payout — the 500 that accrued minus the
    // 5% (25) sent to the treasury — not the gross accrued amount.
    assert_eq!(withdrawn, 475);
    assert_eq!(s.token.balance(&treasury), 25);
    assert_eq!(s.token.balance(&s.ngo), 475);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.withdrawn, 500); // bookkeeping still tracks the gross amount
}

#[test]
fn withdraw_returns_net_after_protocol_fee() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&250); // 2.5%

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 40); // 400 accrues

    let ngo_before = s.token.balance(&s.ngo);
    let withdrawn = s.client.withdraw(&stream_id);

    // The return value is exactly what the NGO received: 400 gross - 10 fee.
    assert_eq!(withdrawn, 390);
    assert_eq!(s.token.balance(&s.ngo) - ngo_before, withdrawn);
    assert_eq!(s.token.balance(&treasury), 10);

    // And the stream's bookkeeping still records the gross 400 as withdrawn.
    assert_eq!(s.client.get_stream(&stream_id).withdrawn, 400);
}

#[test]
fn cancel_stream_splits_protocol_fee_on_accrued_but_not_on_refund() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500); // 5%

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 20); // 200 accrues

    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 800);

    // 5% of the 200 accrued goes to the treasury; the rest settles to the NGO.
    assert_eq!(s.token.balance(&treasury), 10);
    assert_eq!(s.token.balance(&s.ngo), 190);
    // The untouched 800 refunds to the donor in full — no fee on refunds.
    assert_eq!(s.token.balance(&s.donor), 800);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.withdrawn, 200); // bookkeeping tracks the gross accrued amount
}

#[test]
fn small_payout_rounds_protocol_fee_down_to_zero() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500); // 5%

    // 19 units at 5% is 0.95, and the fee is computed with integer
    // division, so it truncates to nothing.
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &19);
    s.env.ledger().with_mut(|l| l.timestamp += 1); // 19 accrues

    let withdrawn = s.client.withdraw(&stream_id);
    // The fee truncates to zero, so the net return equals the gross here.
    assert_eq!(withdrawn, 19);

    // The rounding favours the NGO: it keeps the whole payout rather than
    // the treasury rounding its cut up to 1.
    assert_eq!(s.token.balance(&s.ngo), 19);
    assert_eq!(s.token.balance(&treasury), 0);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.withdrawn, 19);
}

#[test]
fn protocol_fee_becomes_nonzero_at_the_rounding_boundary() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let treasury = Address::generate(&s.env);
    s.client.set_treasury(&treasury);
    s.client.set_fee_bps(&500); // 5%

    // 20 is the smallest payout at 5% that leaves a whole unit of fee, so
    // it pins the other side of the boundary that 19 truncates below.
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &20);
    s.env.ledger().with_mut(|l| l.timestamp += 1); // 20 accrues

    let withdrawn = s.client.withdraw(&stream_id);
    // 20 gross - 1 fee: the return is net, unlike the 20 recorded as withdrawn.
    assert_eq!(withdrawn, 19);
    assert_eq!(s.token.balance(&treasury), 1);
    assert_eq!(s.token.balance(&s.ngo), 19);
}

#[test]
fn set_fee_bps_rejects_over_cap() {
    let s = setup();
    let result = s.client.try_set_fee_bps(&1_001);
    assert_eq!(result, Err(Ok(Error::FeeTooHigh)));
}

#[test]
fn set_fee_bps_boundary_exact_max_succeeds() {
    let s = setup();

    // Exactly 1 000 bps (10%) is the maximum allowed fee — it must be
    // accepted and stored faithfully.
    s.client.set_fee_bps(&1_000);
    assert_eq!(s.client.fee_bps(), 1_000);

    // One basis point above the cap must still be rejected with FeeTooHigh
    // specifically, not just any error, so an off-by-one in the guard
    // can't hide behind a different error path.
    let result = s.client.try_set_fee_bps(&1_001);
    assert_eq!(result, Err(Ok(Error::FeeTooHigh)));
}

#[test]
fn min_deposit_setter_and_guard() {
    let s = setup();
    s.token_admin.mint(&s.donor, &2_000);
    s.client.set_min_deposit(&1_000);
    assert_eq!(s.client.min_deposit(), 1_000);

    assert_eq!(
        s.client
            .try_create_stream(&s.donor, &s.ngo, &s.token.address, &999, &10),
        Err(Ok(Error::DepositTooLow))
    );
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert_eq!(s.client.get_stream(&stream_id).balance, 1_000);
}

#[test]
fn min_deposit_requires_non_negative_admin_value() {
    let s = setup();
    assert_eq!(
        s.client.try_set_min_deposit(&-1),
        Err(Ok(Error::InvalidAmount))
    );
}

#[test]
#[should_panic]
fn withdraw_fails_for_non_ngo_caller() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.env.ledger().with_mut(|l| l.timestamp += 50);

    // Only the donor authorizes this call; withdraw requires the ngo's auth,
    // so it must fail even though the donor is a party to the stream.
    s.env.mock_auths(&[MockAuth {
        address: &s.donor,
        invoke: &MockAuthInvoke {
            contract: &s.client.address,
            fn_name: "withdraw",
            args: (stream_id,).into_val(&s.env),
            sub_invokes: &[],
        },
    }]);

    s.client.withdraw(&stream_id);
}

#[test]
fn withdraw_and_cancel_on_fully_drained_stream_are_no_ops() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    // 100 seconds at 10/s would accrue 1_000, exactly draining the balance.
    s.env.ledger().with_mut(|l| l.timestamp += 100);

    let withdrawn = s.client.withdraw(&stream_id);
    assert_eq!(withdrawn, 1_000);
    assert_eq!(s.token.balance(&s.ngo), 1_000);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.withdrawn, 1_000);
    // Drained by withdrawal, not cancelled — the rate is still live.
    assert!(!stream.cancelled);
    assert_eq!(stream.rate, 10);

    // More time passes, but there's nothing left to accrue.
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let result = s.client.try_withdraw(&stream_id);
    assert_eq!(result, Err(Ok(Error::NothingToWithdraw)));

    // Cancelling a drained stream settles and refunds nothing, but it does
    // flip `cancelled` — the one bit that distinguishes it from a stream
    // that merely ran dry.
    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 0);
    assert_eq!(s.token.balance(&s.ngo), 1_000);
    assert_eq!(s.token.balance(&s.donor), 0);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.rate, 0);
    assert_eq!(stream.withdrawn, 1_000);
    assert!(stream.cancelled);
}

#[test]
fn cancel_stream_twice_is_harmless() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.env.ledger().with_mut(|l| l.timestamp += 50); // 500 accrues

    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 500);
    assert_eq!(s.token.balance(&s.ngo), 500);
    assert_eq!(s.token.balance(&s.donor), 500);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.rate, 0);
    assert_eq!(stream.withdrawn, 500);
    assert!(stream.cancelled);

    // Cancelling again settles zero (rate and balance are already zero) and
    // refunds zero, leaving balances and stream state unchanged.
    s.env.ledger().with_mut(|l| l.timestamp += 50);
    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 0);

    // Sampled here, before any further SDK call: `events().all()` only covers
    // the most recent invocation, so reading a balance or the stream below
    // would reset the buffer and make the comparison below vacuous.
    let events_after_second_cancel = event_count(&s.env, &s.client.address);

    assert_eq!(s.token.balance(&s.ngo), 500);
    assert_eq!(s.token.balance(&s.donor), 500);

    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 0);
    assert_eq!(stream.rate, 0);
    assert_eq!(stream.withdrawn, 500);
    assert!(stream.cancelled);

    // accrued and refund were both zero, so the second cancel must publish
    // nothing. A duplicate cancel event is indistinguishable from a real
    // cancellation to an indexer watching the topic (issue #91).
    assert_eq!(
        events_after_second_cancel, 0,
        "a duplicate cancel event was emitted for an already-cancelled stream"
    );
}

#[test]
fn modify_rate_rejects_non_positive_rate() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let result = s.client.try_modify_rate(&stream_id, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    let result = s.client.try_modify_rate(&stream_id, &-1);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    // A rejected call settles nothing and changes nothing: pausing a
    // stream goes through cancel_stream, not a zero rate.
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.rate, 10);
    assert_eq!(stream.balance, 1_000);
    assert_eq!(stream.withdrawn, 0);
}

#[test]
fn modify_rate_on_cancelled_stream_fails() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.cancel_stream(&stream_id);

    let result = s.client.try_modify_rate(&stream_id, &20);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));

    // The rejected call leaves the cancelled stream's rate at zero — it
    // must not be revivable via modify_rate.
    assert_eq!(s.client.get_stream(&stream_id).rate, 0);
}

#[test]
fn modify_rate_on_unknown_stream_fails() {
    let s = setup();

    let result = s.client.try_modify_rate(&999, &10);
    assert_eq!(result, Err(Ok(Error::StreamNotFound)));
}

#[test]
fn modify_rate_checks_the_rate_before_the_stream_id() {
    let s = setup();

    // Both arguments are bad. The rate is validated before the stream is
    // looked up, so the caller gets InvalidAmount rather than
    // StreamNotFound — worth pinning so the order can't quietly flip.
    let result = s.client.try_modify_rate(&999, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn top_up_rejects_non_positive_amount() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_500);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let result = s.client.try_top_up(&stream_id, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    let result = s.client.try_top_up(&stream_id, &-100);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));

    // top_up settles accrued funds and pulls tokens from the donor, so a
    // rejected call has to leave both the stream and the balances alone.
    let stream = s.client.get_stream(&stream_id);
    assert_eq!(stream.balance, 1_000);
    assert_eq!(stream.withdrawn, 0);
    assert_eq!(s.token.balance(&s.donor), 500);
    assert_eq!(s.token.balance(&s.ngo), 0);
}

#[test]
fn top_up_on_cancelled_stream_fails() {
    let s = setup();
    s.token_admin.mint(&s.donor, &2_000);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.cancel_stream(&stream_id);

    let result = s.client.try_top_up(&stream_id, &500);
    assert_eq!(result, Err(Ok(Error::StreamCancelled)));

    // The rejected call moves no tokens: the donor still holds the full
    // 1_000 refunded by cancel_stream (no time passed, so nothing accrued).
    assert_eq!(s.token.balance(&s.donor), 2_000);
    assert_eq!(s.token.balance(&s.client.address), 0);
}

#[test]
fn top_up_on_unknown_stream_fails() {
    let s = setup();

    let result = s.client.try_top_up(&999, &100);
    assert_eq!(result, Err(Ok(Error::StreamNotFound)));
}

#[test]
fn top_up_checks_the_amount_before_the_stream_id() {
    let s = setup();

    // Both arguments are bad. The amount is validated before the stream is
    // looked up, so the caller gets InvalidAmount rather than
    // StreamNotFound — worth pinning so the order can't quietly flip.
    let result = s.client.try_top_up(&999, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn get_stream_on_unknown_id_fails() {
    let s = setup();

    // Debug on Stream is what lets assert_eq! take the whole
    // Result<Result<Stream, _>, _> here instead of matching on it.
    let result = s.client.try_get_stream(&999);
    assert_eq!(result, Err(Ok(Error::StreamNotFound)));
}

#[test]
fn create_stream_stores_every_field() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    s.env.ledger().with_mut(|l| l.timestamp = 12_345);

    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    // PartialEq on Stream lets one assertion cover the whole struct, so a
    // newly added field can't slip in unchecked the way it would with a
    // handful of per-field assertions.
    assert_eq!(
        s.client.get_stream(&stream_id),
        Stream {
            donor: s.donor.clone(),
            ngo: s.ngo.clone(),
            token: s.token.address.clone(),
            rate: 10,
            balance: 1_000,
            withdrawn: 0,
            created_at: 12_345,
            last_update: 12_345,
            status: StreamStatus::Active,
            cancelled: false,
        }
    );
}

fn instance_ttl(s: &Setup) -> u32 {
    s.env
        .as_contract(&s.client.address, || s.env.storage().instance().get_ttl())
}

fn stream_ttl(s: &Setup, stream_id: u64) -> u32 {
    s.env.as_contract(&s.client.address, || {
        s.env
            .storage()
            .persistent()
            .get_ttl(&DataKey::Stream(stream_id))
    })
}

/// Moves the ledger forward two days, which drops both the instance and a
/// freshly bumped stream below their lifetime thresholds. Asserts that it
/// did, so a test calling this can't pass just because nothing needed a bump.
fn age_past_thresholds(s: &Setup, stream_id: Option<u64>) {
    s.env
        .ledger()
        .with_mut(|l| l.sequence_number += 2 * DAY_IN_LEDGERS);
    assert!(instance_ttl(s) < INSTANCE_LIFETIME_THRESHOLD);
    if let Some(stream_id) = stream_id {
        assert!(stream_ttl(s, stream_id) < STREAM_LIFETIME_THRESHOLD);
    }
}

fn create_ttl_test_stream(s: &Setup) -> u64 {
    s.token_admin.mint(&s.donor, &2_000);
    s.client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10)
}

#[test]
fn create_stream_bumps_instance_and_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);

    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn withdraw_bumps_instance_and_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    age_past_thresholds(&s, Some(stream_id));
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    s.client.withdraw(&stream_id);

    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn cancel_stream_bumps_instance_and_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    age_past_thresholds(&s, Some(stream_id));

    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 1_000);

    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn cancel_stream_applies_configured_grace_period() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    let grace_ledgers = 14 * DAY_IN_LEDGERS;

    s.client.set_cancel_grace_ledgers(&grace_ledgers);
    assert_eq!(s.client.cancel_grace_ledgers(), grace_ledgers);
    age_past_thresholds(&s, Some(stream_id));

    s.client.cancel_stream(&stream_id);

    assert_eq!(
        stream_ttl(&s, stream_id),
        STREAM_BUMP_AMOUNT + grace_ledgers
    );
}

#[test]
fn top_up_bumps_instance_and_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    age_past_thresholds(&s, Some(stream_id));

    s.client.top_up(&stream_id, &500);

    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn modify_rate_bumps_instance_and_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    age_past_thresholds(&s, Some(stream_id));

    s.client.modify_rate(&stream_id, &20);

    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn extend_stream_bumps_stream_ttl() {
    let s = setup();
    let stream_id = create_ttl_test_stream(&s);
    age_past_thresholds(&s, Some(stream_id));

    s.client.extend_stream(&stream_id);

    assert_eq!(stream_ttl(&s, stream_id), STREAM_BUMP_AMOUNT);
}

#[test]
fn admin_writes_bump_instance_ttl() {
    let s = setup();
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT); // from init

    let new_admin = Address::generate(&s.env);
    let treasury = Address::generate(&s.env);

    age_past_thresholds(&s, None);
    s.client.pause();
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    age_past_thresholds(&s, None);
    s.client.unpause();
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    age_past_thresholds(&s, None);
    s.client.set_treasury(&treasury);
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    age_past_thresholds(&s, None);
    s.client.set_fee_bps(&100);
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    age_past_thresholds(&s, None);
    s.client.propose_admin(&new_admin);
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    age_past_thresholds(&s, None);
    s.client.cancel_admin_proposal();
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);

    s.client.propose_admin(&new_admin);
    age_past_thresholds(&s, None);
    s.client.accept_admin();
    assert_eq!(instance_ttl(&s), INSTANCE_BUMP_AMOUNT);
}

/// Rewrites a stored stream in place, for pushing its bookkeeping to the
/// edge of i128 — no real token supply could get it there.
fn overwrite_stream(s: &Setup, stream_id: u64, edit: impl FnOnce(&mut Stream)) {
    s.env.as_contract(&s.client.address, || {
        let key = DataKey::Stream(stream_id);
        let mut stream: Stream = s.env.storage().persistent().get(&key).unwrap();
        edit(&mut stream);
        s.env.storage().persistent().set(&key, &stream);
    });
}

#[test]
fn top_up_past_i128_max_balance_returns_overflow_error() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_001);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    overwrite_stream(&s, stream_id, |stream| stream.balance = i128::MAX);

    let result = s.client.try_top_up(&stream_id, &1);
    assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));

    // The failed call rolls back, so the donor keeps the unit it tried to add.
    assert_eq!(s.token.balance(&s.donor), 1);
}

#[test]
fn withdraw_past_i128_max_withdrawn_returns_overflow_error() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    overwrite_stream(&s, stream_id, |stream| stream.withdrawn = i128::MAX);
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let result = s.client.try_withdraw(&stream_id);
    assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));
    assert_eq!(s.token.balance(&s.ngo), 0);
}

#[test]
fn cancel_stream_past_i128_max_withdrawn_returns_overflow_error() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    overwrite_stream(&s, stream_id, |stream| stream.withdrawn = i128::MAX);
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let result = s.client.try_cancel_stream(&stream_id);
    assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));
    assert_eq!(s.token.balance(&s.ngo), 0);
    assert_eq!(s.token.balance(&s.donor), 0);
}

#[test]
fn modify_rate_past_i128_max_withdrawn_returns_overflow_error() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    overwrite_stream(&s, stream_id, |stream| stream.withdrawn = i128::MAX);
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    let result = s.client.try_modify_rate(&stream_id, &20);
    assert_eq!(result, Err(Ok(Error::ArithmeticOverflow)));
    assert_eq!(s.client.get_stream(&stream_id).rate, 10);
}

/// Asserts that the last top-level call required auth from exactly one
/// address, `expected`, and that it was for `fn_name` on the vault.
/// mock_all_auths() lets any require_auth pass, so without this a
/// require_auth removed or moved to the wrong address would go unnoticed.
fn assert_auth_required_from(s: &Setup, expected: &Address, fn_name: &str) {
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1, "expected exactly one authorizer");

    let (address, invocation) = &auths[0];
    assert_eq!(address, expected);
    match &invocation.function {
        AuthorizedFunction::Contract((contract, function, _)) => {
            assert_eq!(contract, &s.client.address);
            assert_eq!(function, &Symbol::new(&s.env, fn_name));
        }
        _ => panic!("expected a contract invocation"),
    }
}

#[test]
fn create_stream_requires_donor_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);

    s.client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    assert_auth_required_from(&s, &s.donor, "create_stream");
}

#[test]
fn withdraw_requires_ngo_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50);

    s.client.withdraw(&stream_id);

    assert_auth_required_from(&s, &s.ngo, "withdraw");
}

#[test]
fn cancel_stream_requires_donor_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    let refund = s.client.cancel_stream(&stream_id);
    assert_eq!(refund, 1_000);

    assert_auth_required_from(&s, &s.donor, "cancel_stream");
}

#[test]
fn top_up_requires_donor_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_500);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.client.top_up(&stream_id, &500);

    assert_auth_required_from(&s, &s.donor, "top_up");
}

#[test]
fn modify_rate_requires_donor_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.client.modify_rate(&stream_id, &20);

    assert_auth_required_from(&s, &s.donor, "modify_rate");
}

#[test]
fn extend_stream_requires_no_auth() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s
        .client
        .create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    s.client.extend_stream(&stream_id);

    assert!(s.env.auths().is_empty());
}

#[test]
fn admin_entry_points_require_admin_auth() {
    let s = setup();
    let admin = s.client.admin();

    s.client.pause();
    assert_auth_required_from(&s, &admin, "pause");

    s.client.unpause();
    assert_auth_required_from(&s, &admin, "unpause");

    s.client.set_treasury(&Address::generate(&s.env));
    assert_auth_required_from(&s, &admin, "set_treasury");

    s.client.clear_treasury();
    assert_auth_required_from(&s, &admin, "clear_treasury");

    s.client.set_fee_bps(&100);
    assert_auth_required_from(&s, &admin, "set_fee_bps");

    let new_admin = Address::generate(&s.env);
    s.client.propose_admin(&new_admin);
    assert_auth_required_from(&s, &admin, "propose_admin");

    s.client.cancel_admin_proposal();
    assert_auth_required_from(&s, &admin, "cancel_admin_proposal");
}

#[test]
fn accept_admin_requires_pending_admin_auth() {
    let s = setup();
    let new_admin = Address::generate(&s.env);
    s.client.propose_admin(&new_admin);

    s.client.accept_admin();

    // The proposed address, not the outgoing admin, has to accept.
    assert_auth_required_from(&s, &new_admin, "accept_admin");
}

// =============================================================================
// Per-donor stream limit (issue #94)
// =============================================================================

#[test]
fn max_streams_per_donor_defaults_to_100() {
    let s = setup();
    assert_eq!(s.client.max_streams_per_donor(), 100);
}

#[test]
fn admin_can_set_the_per_donor_cap() {
    let s = setup();
    s.client.set_max_streams_per_donor(&5);
    assert_eq!(s.client.max_streams_per_donor(), 5);
}

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 0); // no treasury → no fee, regardless of fee_bps
    assert_eq!(net, 500); // full accrual goes to the NGO
#[test]
fn non_admin_cannot_set_the_per_donor_cap() {
    let s = setup();
    s.env.mock_auths(&[]);
    let result = s.client.try_set_max_streams_per_donor(&5);
    assert!(result.is_err());
}

#[test]
fn create_stream_rejects_the_101st_stream() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000_000);

    // Fill the default cap.
    for _ in 0..100 {
        s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    }

    let result = s.client.try_create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert!(result.is_err());
}

#[test]
fn raising_the_cap_lets_the_next_stream_through() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000_000);
    s.client.set_max_streams_per_donor(&1);

    s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);

    // At the cap, next call fails.
    let result = s.client.try_create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert!(result.is_err());

    // Raise the cap. Next call succeeds.
    s.client.set_max_streams_per_donor(&2);
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert_eq!(s.client.get_stream(&stream_id).donor, s.donor);
}

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 0); // 0 bps → zero fee even with a treasury set
    assert_eq!(net, 500);
#[test]
fn lowering_the_cap_does_not_retroactively_affect_existing_streams() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000_000);
    s.client.set_max_streams_per_donor(&5);

    for _ in 0..3 {
        s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    }

    // Drop the cap below the current count.
    s.client.set_max_streams_per_donor(&1);

    // New streams are rejected.
    let result = s.client.try_create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert!(result.is_err());

    // Existing streams are untouched and still withdrawable.
    s.env.ledger().with_mut(|l| l.timestamp += 50);
    let withdrawn = s.client.withdraw(&0);
    assert_eq!(withdrawn, 500);
}

#[test]
fn separate_donors_have_separate_counters() {
    let s = setup();
    s.client.set_max_streams_per_donor(&1);

    let donor_b = Address::generate(&s.env);
    s.token_admin.mint(&s.donor, &1_000);
    s.token_admin.mint(&donor_b, &1_000);

    s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    // A different donor is unaffected by donor A's cap.
    let donor_b_stream = s.client.create_stream(&donor_b, &s.ngo, &s.token.address, &1_000, &10);
    assert_eq!(s.client.get_stream(&donor_b_stream).donor, donor_b);

    // But donor_b is now at its own cap.
    let result = s.client.try_create_stream(&donor_b, &s.ngo, &s.token.address, &1_000, &10);
    assert!(result.is_err());
}

    let (net, fee) = s.client.pending_payout(&stream_id);
    assert_eq!(fee, 25); // 5% of 500
    assert_eq!(net, 475); // 500 - 25

// =============================================================================
// Explicit stream status (issue #92)
// =============================================================================

#[test]
fn new_stream_starts_active() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    assert_eq!(s.client.get_stream(&stream_id).status, StreamStatus::Active);
}

#[test]
fn cancelled_stream_reports_cancelled() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.cancel_stream(&stream_id);
    assert_eq!(s.client.get_stream(&stream_id).status, StreamStatus::Cancelled);
}

#[test]
fn fully_withdrawn_stream_reports_drained() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    // 10 units/s for 1000 units ? fully drained after 100 seconds.
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 100);
    s.client.withdraw(&stream_id);
    assert_eq!(s.client.get_stream(&stream_id).status, StreamStatus::Drained);
}

#[test]
fn partially_withdrawn_stream_stays_active() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.env.ledger().with_mut(|l| l.timestamp += 50);
    s.client.withdraw(&stream_id);
    assert_eq!(s.client.get_stream(&stream_id).status, StreamStatus::Active);
}

#[test]
fn status_is_queryable_after_cancel_then_further_operations_fail() {
    let s = setup();
    s.token_admin.mint(&s.donor, &1_000);
    let stream_id = s.client.create_stream(&s.donor, &s.ngo, &s.token.address, &1_000, &10);
    s.client.cancel_stream(&stream_id);
    assert_eq!(s.client.get_stream(&stream_id).status, StreamStatus::Cancelled);
    // top_up on a cancelled stream is rejected (cancelled sets rate = 0).
    s.env.ledger().with_mut(|l| l.timestamp += 10);
    let result = s.client.try_withdraw(&
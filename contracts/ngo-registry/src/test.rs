// SPDX-License-Identifier: Apache-2.0
#![cfg(test)]

use super::*;
use soroban_sdk::testutils::storage::{Instance as _, Persistent as _};
use soroban_sdk::testutils::{
    Address as _, AuthorizedFunction, Events as _, Ledger, MockAuth, MockAuthInvoke,
};
use soroban_sdk::{vec, IntoVal, Symbol, Val, Vec};

fn setup() -> (Env, NgoRegistryClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();
    // Leave room for the 90-day NGO bump so the TTL tests see it unclamped.
    env.ledger()
        .with_mut(|l| l.max_entry_ttl = 365 * DAY_IN_LEDGERS);

    let contract_id = env.register(NgoRegistry, ());
    let client = NgoRegistryClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    client.init(&admin);

    (env, client, admin)
}

#[test]
fn init_sets_admin() {
    let (_env, client, admin) = setup();
    assert_eq!(client.admin(), admin);
}

#[test]
fn double_init_fails() {
    let (_env, client, admin) = setup();
    let result = client.try_init(&admin);
    assert_eq!(result, Err(Ok(Error::AlreadyInitialized)));
}

#[test]
fn register_ngo_stores_unverified_entry() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");

    client.register(&owner, &name);

    let ngo = client.get_ngo(&owner);
    assert_eq!(ngo.owner, owner);
    assert_eq!(ngo.name, name);
    assert!(!ngo.verified);
}

#[test]
fn unverified_ngo_can_unregister_and_register_again() {
    let (env, client, _) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Pending NGO");

    client.register(&owner, &name);
    client.unregister(&owner);
    assert_eq!(client.try_get_ngo(&owner), Err(Ok(Error::NotRegistered)));

    client.register(&owner, &String::from_str(&env, "Updated NGO"));
    assert_eq!(
        client.get_ngo(&owner).name,
        String::from_str(&env, "Updated NGO")
    );
}

#[test]
fn double_register_fails() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");

    client.register(&owner, &name);
    let result = client.try_register(&owner, &name);

    assert_eq!(result, Err(Ok(Error::AlreadyRegistered)));
}

#[test]
fn register_rejects_name_over_max_length() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    // MAX_NGO_NAME_LEN (200) + 1 bytes.
    let too_long = String::from_str(&env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    let result = client.try_register(&owner, &too_long);

    assert_eq!(result, Err(Ok(Error::NameTooLong)));
    assert_eq!(client.try_get_ngo(&owner), Err(Ok(Error::NotRegistered)));
}

#[test]
fn register_accepts_name_at_max_length() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    // Exactly MAX_NGO_NAME_LEN (200) bytes.
    let at_limit = String::from_str(&env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    client.register(&owner, &at_limit);

    assert_eq!(client.get_ngo(&owner).name, at_limit);
}

#[test]
fn update_name_rejects_name_over_max_length() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    let too_long = String::from_str(&env, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    let result = client.try_update_name(&owner, &too_long);

    assert_eq!(result, Err(Ok(Error::NameTooLong)));
}

#[test]
fn get_unregistered_ngo_fails() {
    let (env, client, _admin) = setup();
    let random = Address::generate(&env);

    let result = client.try_get_ngo(&random);
    assert_eq!(result, Err(Ok(Error::NotRegistered)));
}

#[test]
fn approve_ngo_marks_verified() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);

    client.approve_ngo(&owner);

    let ngo = client.get_ngo(&owner);
    assert!(ngo.verified);
}

#[test]
fn approve_ngo_twice_fails() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);

    client.approve_ngo(&owner);

    let result = client.try_approve_ngo(&owner);
    assert_eq!(result, Err(Ok(Error::AlreadyVerified)));
    assert!(client.get_ngo(&owner).verified);
}

#[test]
fn approve_unregistered_ngo_fails() {
    let (env, client, _admin) = setup();
    let random = Address::generate(&env);

    let result = client.try_approve_ngo(&random);
    assert_eq!(result, Err(Ok(Error::NotRegistered)));
}

#[test]
fn revoke_ngo_clears_verified_status() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);

    client.approve_ngo(&owner);
    assert!(client.get_ngo(&owner).verified);

    client.revoke_ngo(&owner);

    let ngo = client.get_ngo(&owner);
    assert!(!ngo.verified);
}

#[test]
fn revoke_unverified_ngo_fails() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);

    let result = client.try_revoke_ngo(&owner);
    assert_eq!(result, Err(Ok(Error::NotVerified)));
    assert!(!client.get_ngo(&owner).verified);
}

#[test]
fn revoke_unregistered_ngo_fails() {
    let (env, client, _admin) = setup();
    let random = Address::generate(&env);

    let result = client.try_revoke_ngo(&random);
    assert_eq!(result, Err(Ok(Error::NotRegistered)));
}

#[test]
fn touch_ngo_leaves_entry_unchanged() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);
    client.approve_ngo(&owner);

    let before = client.get_ngo(&owner);
    client.touch_ngo(&owner);
    let after = client.get_ngo(&owner);

    assert_eq!(before, after);
}

#[test]
fn touch_unregistered_ngo_fails() {
    let (env, client, _admin) = setup();
    let random = Address::generate(&env);

    let result = client.try_touch_ngo(&random);
    assert_eq!(result, Err(Ok(Error::NotRegistered)));
}

fn instance_ttl(env: &Env, client: &NgoRegistryClient) -> u32 {
    env.as_contract(&client.address, || env.storage().instance().get_ttl())
}

fn ngo_ttl(env: &Env, client: &NgoRegistryClient, owner: &Address) -> u32 {
    env.as_contract(&client.address, || {
        env.storage()
            .persistent()
            .get_ttl(&DataKey::Ngo(owner.clone()))
    })
}

/// Moves the ledger forward two days, which drops both the instance and a
/// freshly bumped NGO entry below their lifetime thresholds. Asserts that
/// it did, so a test calling this can't pass just because nothing needed
/// a bump.
fn age_past_thresholds(env: &Env, client: &NgoRegistryClient, owner: &Address) {
    env.ledger()
        .with_mut(|l| l.sequence_number += 2 * DAY_IN_LEDGERS);
    assert!(instance_ttl(env, client) < INSTANCE_LIFETIME_THRESHOLD);
    assert!(ngo_ttl(env, client, owner) < NGO_LIFETIME_THRESHOLD);
}

fn assert_ttls_bumped(env: &Env, client: &NgoRegistryClient, owner: &Address) {
    assert_eq!(instance_ttl(env, client), INSTANCE_BUMP_AMOUNT);
    assert_eq!(ngo_ttl(env, client, owner), NGO_BUMP_AMOUNT);
}

#[test]
fn register_bumps_instance_and_ngo_ttl() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));

    assert_ttls_bumped(&env, &client, &owner);
}

#[test]
fn approve_ngo_bumps_instance_and_ngo_ttl() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    age_past_thresholds(&env, &client, &owner);

    client.approve_ngo(&owner);

    assert_ttls_bumped(&env, &client, &owner);
}

#[test]
fn revoke_ngo_bumps_instance_and_ngo_ttl() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    client.approve_ngo(&owner);
    age_past_thresholds(&env, &client, &owner);

    client.revoke_ngo(&owner);

    assert_ttls_bumped(&env, &client, &owner);
}

#[test]
fn update_name_changes_name_before_approval() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Crsos"));

    let fixed = String::from_str(&env, "Red Cross");
    client.update_name(&owner, &fixed);

    // Events cover only the latest top-level call, so read them before
    // `get_ngo` below replaces them.
    let events = env.events().all();
    let expected: Vec<(Address, Vec<Val>, Val)> = vec![
        &env,
        (
            client.address.clone(),
            (symbol_short!("renamed"), owner.clone()).into_val(&env),
            fixed.into_val(&env),
        ),
    ];
    assert_eq!(events, expected);

    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("renamed"), owner.clone()).into_val(&env),
                fixed.into_val(&env)
            ),
        ]
    );

    assert_eq!(
        client.get_ngo(&owner),
        Ngo {
            owner: owner.clone(),
            name: fixed.clone(),
            verified: false,
        }
    );
}

#[test]
fn update_name_after_approval_fails() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");
    client.register(&owner, &name);
    client.approve_ngo(&owner);

    let result = client.try_update_name(&owner, &String::from_str(&env, "Blue Cross"));
    assert_eq!(result, Err(Ok(Error::AlreadyVerified)));
    assert_eq!(client.get_ngo(&owner).name, name);
}

#[test]
fn update_name_after_revocation_succeeds() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Crsos"));
    client.approve_ngo(&owner);

    // The name an admin approved is locked only while the NGO stays
    // verified, so the rename has to be rejected at this point.
    let blocked = client.try_update_name(&owner, &String::from_str(&env, "Red Cross"));
    assert_eq!(blocked, Err(Ok(Error::AlreadyVerified)));

    client.revoke_ngo(&owner);
    assert!(!client.get_ngo(&owner).verified);

    let fixed = String::from_str(&env, "Red Cross");
    client.update_name(&owner, &fixed);

    // Events cover only the latest top-level call, so read them before
    // `get_ngo` below replaces them.
    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("renamed"), owner.clone()).into_val(&env),
                fixed.clone().into_val(&env),
            ),
        ]
    );

    assert_eq!(
        client.get_ngo(&owner),
        Ngo {
            owner: owner.clone(),
            name: fixed,
            verified: false,
        }
    );
}

#[test]
fn update_name_for_unregistered_ngo_fails() {
    let (env, client, _admin) = setup();
    let random = Address::generate(&env);

    let result = client.try_update_name(&random, &String::from_str(&env, "Red Cross"));
    assert_eq!(result, Err(Ok(Error::NotRegistered)));
}

#[test]
fn update_name_requires_owner_auth() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Crsos"));

    client.update_name(&owner, &String::from_str(&env, "Red Cross"));

    let auths = env.auths();
    assert_eq!(auths.len(), 1);
    let (address, invocation) = &auths[0];
    assert_eq!(address, &owner);
    match &invocation.function {
        AuthorizedFunction::Contract((contract, function, _)) => {
            assert_eq!(contract, &client.address);
            assert_eq!(function, &Symbol::new(&env, "update_name"));
        }
        _ => panic!("expected a contract invocation"),
    }
}

#[test]
fn update_name_bumps_instance_and_ngo_ttl() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Crsos"));
    age_past_thresholds(&env, &client, &owner);

    client.update_name(&owner, &String::from_str(&env, "Red Cross"));

    assert_ttls_bumped(&env, &client, &owner);
}

#[test]
fn touch_ngo_bumps_instance_and_ngo_ttl() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    age_past_thresholds(&env, &client, &owner);

    client.touch_ngo(&owner);

    assert_ttls_bumped(&env, &client, &owner);
}

// --- Event assertion tests (issue #62) ---

#[test]
fn register_publishes_event() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    let name = String::from_str(&env, "Red Cross");

    client.register(&owner, &name);

    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("register"), owner).into_val(&env),
                name.into_val(&env),
            ),
        ]
    );
}

#[test]
fn approve_ngo_publishes_event() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));

    client.approve_ngo(&owner);

    // Events are cleared per top-level call; only the approved event is visible.
    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("approved"), owner).into_val(&env),
                ().into_val(&env),
            ),
        ]
    );
}

#[test]
fn revoke_ngo_publishes_event() {
    let (env, client, _admin) = setup();
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    client.approve_ngo(&owner);

    client.revoke_ngo(&owner);

    // Events are cleared per top-level call; only the revoked event is visible.
    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("revoked"), owner).into_val(&env),
                ().into_val(&env),
            ),
        ]
    );
}

// --- Two-step admin transfer (issue #37) ---
//
// Mirrors donation-vault's propose_admin / accept_admin / cancel_admin_proposal
// (see DonationVault's tests of the same name) so both contracts behave the
// same way for operators managing admin handover.

#[test]
fn pending_admin_defaults_to_none() {
    let (_env, client, _admin) = setup();
    assert_eq!(client.pending_admin(), None);
}

#[test]
fn propose_then_accept_admin_transfers_control() {
    let (env, client, old_admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&new_admin);
    assert_eq!(client.pending_admin(), Some(new_admin.clone()));
    // Admin hasn't changed yet — only proposed.
    assert_eq!(client.admin(), old_admin);

    client.accept_admin();
    assert_eq!(client.admin(), new_admin);
    assert_eq!(client.pending_admin(), None);

    // The new admin can act as admin.
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    client.approve_ngo(&owner);
    assert!(client.get_ngo(&owner).verified);
}

#[test]
fn propose_admin_rejects_current_admin() {
    let (_env, client, admin) = setup();

    let result = client.try_propose_admin(&admin);
    assert_eq!(result, Err(Ok(Error::InvalidAdmin)));
    assert_eq!(client.pending_admin(), None);
}

#[test]
fn accept_admin_without_proposal_fails() {
    let (_env, client, _admin) = setup();

    let result = client.try_accept_admin();
    assert_eq!(result, Err(Ok(Error::NoPendingAdmin)));
}

#[test]
fn cancel_admin_proposal_without_proposal_fails() {
    let (_env, client, _admin) = setup();

    let result = client.try_cancel_admin_proposal();
    assert_eq!(result, Err(Ok(Error::NoPendingAdmin)));
}

#[test]
fn cancel_admin_proposal_clears_pending() {
    let (env, client, old_admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&new_admin);
    assert_eq!(client.pending_admin(), Some(new_admin));

    client.cancel_admin_proposal();
    assert_eq!(client.pending_admin(), None);
    assert_eq!(client.admin(), old_admin);

    // Nothing left to accept.
    let result = client.try_accept_admin();
    assert_eq!(result, Err(Ok(Error::NoPendingAdmin)));
}

#[test]
fn repropose_admin_overwrites_earlier_proposal() {
    let (env, client, old_admin) = setup();
    let admin_a = Address::generate(&env);
    let admin_b = Address::generate(&env);

    client.propose_admin(&admin_a);
    assert_eq!(client.pending_admin(), Some(admin_a.clone()));

    // Proposing again replaces the pending address instead of queueing.
    client.propose_admin(&admin_b);
    assert_eq!(
        client.pending_admin(),
        Some(admin_b.clone()),
        "the second proposal must overwrite the first, not queue behind it"
    );

    client.accept_admin();

    // Control actually moved to B, and only to B.
    assert_eq!(client.admin(), admin_b);
    assert_ne!(client.admin(), old_admin);
    assert_ne!(client.admin(), admin_a);
}

#[test]
fn accept_admin_requires_pending_admin_auth() {
    let (env, client, _admin) = setup();
    let new_admin = Address::generate(&env);
    client.propose_admin(&new_admin);

    client.accept_admin();

    // The proposed address, not the outgoing admin, has to accept.
    let auths = env.auths();
    assert_eq!(auths.len(), 1);
    let (address, invocation) = &auths[0];
    assert_eq!(address, &new_admin);
    match &invocation.function {
        AuthorizedFunction::Contract((contract, function, _)) => {
            assert_eq!(contract, &client.address);
            assert_eq!(function, &Symbol::new(&env, "accept_admin"));
        }
        _ => panic!("expected a contract invocation"),
    }
}

#[test]
#[should_panic]
fn old_admin_loses_admin_gated_access_after_transfer() {
    let (env, client, old_admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&new_admin);
    client.accept_admin();

    // approve_ngo requires the current admin's auth; only the old admin
    // authorizes this call, and the old admin is no longer admin.
    let owner = Address::generate(&env);
    client.register(&owner, &String::from_str(&env, "Red Cross"));
    env.mock_auths(&[MockAuth {
        address: &old_admin,
        invoke: &MockAuthInvoke {
            contract: &client.address,
            fn_name: "approve_ngo",
            args: (owner.clone(),).into_val(&env),
            sub_invokes: &[],
        },
    }]);
    client.approve_ngo(&owner);
}

#[test]
fn propose_admin_publishes_event() {
    let (env, client, _admin) = setup();
    let new_admin = Address::generate(&env);

    client.propose_admin(&new_admin);

    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("propadmin"),).into_val(&env),
                new_admin.into_val(&env),
            ),
        ]
    );
}

#[test]
fn accept_admin_publishes_event() {
    let (env, client, _admin) = setup();
    let new_admin = Address::generate(&env);
    client.propose_admin(&new_admin);

    client.accept_admin();

    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("acptadmin"),).into_val(&env),
                new_admin.into_val(&env),
            ),
        ]
    );
}

#[test]
fn cancel_admin_proposal_publishes_event() {
    let (env, client, _admin) = setup();
    let new_admin = Address::generate(&env);
    client.propose_admin(&new_admin);

    client.cancel_admin_proposal();

    assert_eq!(
        env.events().all(),
        soroban_sdk::vec![
            &env,
            (
                client.address.clone(),
                (symbol_short!("canceladm"),).into_val(&env),
                ().into_val(&env),
            ),
        ]
    );
}

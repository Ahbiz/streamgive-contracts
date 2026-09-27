#![no_std]
// soroban-sdk 27 deprecates Events::publish in favour of the
// #[contractevent] macro. Migrating is not a lint cleanup: #[contractevent]
// derives its own topic/data layout, and streamgive-backend's indexer
// decodes the current layout by hand (topic[0] = symbol, topic[1] = id),
// as does docs/EVENTS.md. Both repos have to move in the same change, so
// it is tracked as its own issue rather than done under -D warnings here.
#![allow(deprecated)]

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, symbol_short, Address, Env, String,
};

#[contracttype]
// Debug and PartialEq let tests assert_eq! on a try_* call’s full
// Result<Result<Ngo, _>, _> rather than unwrapping it by hand first.
#[derive(Clone, Debug, PartialEq)]
pub struct Ngo {
    pub owner: Address,
    pub name: String,
    pub verified: bool,
}

#[contracttype]
#[derive(Clone)]
pub enum DataKey {
    Admin,
    Ngo(Address),
    NgoCount,
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    AlreadyRegistered = 3,
    NotRegistered = 4,
    /// The NGO has already been approved, so its name is locked.
    AlreadyVerified = 5,
}

/// Approximate ledgers per day at a 5-second close time. Used to express
/// storage TTLs (which the network counts in ledgers, not wall time) in
/// human terms.
const DAY_IN_LEDGERS: u32 = 17_280;

const INSTANCE_BUMP_AMOUNT: u32 = 30 * DAY_IN_LEDGERS;
const INSTANCE_LIFETIME_THRESHOLD: u32 = INSTANCE_BUMP_AMOUNT - DAY_IN_LEDGERS;

const NGO_BUMP_AMOUNT: u32 = 90 * DAY_IN_LEDGERS;
const NGO_LIFETIME_THRESHOLD: u32 = NGO_BUMP_AMOUNT - DAY_IN_LEDGERS;

fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
}

fn extend_ngo_ttl(env: &Env, owner: &Address) {
    env.storage().persistent().extend_ttl(
        &DataKey::Ngo(owner.clone()),
        NGO_LIFETIME_THRESHOLD,
        NGO_BUMP_AMOUNT,
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

#[contract]
pub struct NgoRegistry;

#[contractimpl]
impl NgoRegistry {
    /// Sets the registry admin. Can only be called once.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// let env = Env::default();
    /// env.mock_all_auths();
    ///
    /// let contract_id = env.register(NgoRegistry, ());
    /// let client = NgoRegistryClient::new(&env, &contract_id);
    ///
    /// let admin = Address::generate(&env);
    /// client.init(&admin);
    /// ```
    pub fn init(env: Env, admin: Address) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        extend_instance_ttl(&env);
        Ok(())
    }

    /// Reads back the registry admin set by `init`.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
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

    /// Submits an NGO application. Callable by the NGO's own address.
    /// The entry starts unverified until an admin approves it.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let owner = Address::generate(&env);
    /// let name = String::from_str(&env, "Example NGO");
    /// client.register(&owner, &name);
    ///
    /// let ngo = client.get_ngo(&owner);
    /// assert_eq!(ngo.name, name);
    /// assert!(!ngo.verified);
    /// ```
    pub fn register(env: Env, owner: Address, name: String) -> Result<(), Error> {
        owner.require_auth();

        let key = DataKey::Ngo(owner.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::AlreadyRegistered);
        }

        let ngo = Ngo {
            owner: owner.clone(),
            name: name.clone(),
            verified: false,
        };
        env.storage().persistent().set(&key, &ngo);

        let count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NgoCount)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&DataKey::NgoCount, &(count + 1));

        extend_instance_ttl(&env);
        extend_ngo_ttl(&env, &owner);

        env.events()
            .publish((symbol_short!("register"), owner), name);

        Ok(())
    }

    /// Changes the name on an NGO's own pending application, so a typo or
    /// rename can be fixed without going through an admin. Requires the
    /// owner's auth. Fails with `Error::NotRegistered` for an address with
    /// no entry, and `Error::AlreadyVerified` once an admin has approved
    /// it — the name an admin approved is the name that stays.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let owner = Address::generate(&env);
    /// client.register(&owner, &String::from_str(&env, "Exmaple NGO"));
    ///
    /// let fixed = String::from_str(&env, "Example NGO");
    /// client.update_name(&owner, &fixed);
    /// assert_eq!(client.get_ngo(&owner).name, fixed);
    /// ```
    pub fn update_name(env: Env, owner: Address, name: String) -> Result<(), Error> {
        owner.require_auth();

        let key = DataKey::Ngo(owner.clone());
        let mut ngo: Ngo = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotRegistered)?;
        if ngo.verified {
            return Err(Error::AlreadyVerified);
        }

        ngo.name = name.clone();
        env.storage().persistent().set(&key, &ngo);
        extend_instance_ttl(&env);
        extend_ngo_ttl(&env, &owner);

        env.events()
            .publish((symbol_short!("renamed"), owner), name);

        Ok(())
    }

    /// Reads back an NGO's registry entry, registered or not.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let owner = Address::generate(&env);
    /// # let name = String::from_str(&env, "Example NGO");
    /// # client.register(&owner, &name);
    /// let ngo = client.get_ngo(&owner);
    /// assert_eq!(ngo.owner, owner);
    /// assert!(!ngo.verified);
    /// ```
    pub fn get_ngo(env: Env, owner: Address) -> Result<Ngo, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Ngo(owner))
            .ok_or(Error::NotRegistered)
    }

    /// Reads back the total number of registered NGOs.
    ///
    /// Lets callers (such as the impact page) display the total count
    /// of registered NGOs without querying a backend indexer.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// assert_eq!(client.ngo_count(), 0);
    ///
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// let owner = Address::generate(&env);
    /// let name = String::from_str(&env, "Example NGO");
    /// client.register(&owner, &name);
    /// assert_eq!(client.ngo_count(), 1);
    /// ```
    pub fn ngo_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NgoCount)
            .unwrap_or(0)
    }

    /// Marks a registered NGO as verified. Admin-only.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let owner = Address::generate(&env);
    /// # let name = String::from_str(&env, "Example NGO");
    /// # client.register(&owner, &name);
    /// client.approve_ngo(&owner);
    /// assert!(client.get_ngo(&owner).verified);
    /// ```
    pub fn approve_ngo(env: Env, ngo_owner: Address) -> Result<(), Error> {
        require_admin(&env)?;

        let key = DataKey::Ngo(ngo_owner.clone());
        let mut ngo: Ngo = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotRegistered)?;
        ngo.verified = true;
        env.storage().persistent().set(&key, &ngo);
        extend_instance_ttl(&env);
        extend_ngo_ttl(&env, &ngo_owner);

        env.events()
            .publish((symbol_short!("approved"), ngo_owner), ());

        Ok(())
    }

    /// Reverses a prior approval, marking a registered NGO as unverified
    /// again. Admin-only. Returns `Error::NotRegistered` for an address
    /// with no entry, matching `approve_ngo`'s existing behavior.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let owner = Address::generate(&env);
    /// # let name = String::from_str(&env, "Example NGO");
    /// # client.register(&owner, &name);
    /// # client.approve_ngo(&owner);
    /// client.revoke_ngo(&owner);
    /// assert!(!client.get_ngo(&owner).verified);
    /// ```
    pub fn revoke_ngo(env: Env, ngo_owner: Address) -> Result<(), Error> {
        require_admin(&env)?;

        let key = DataKey::Ngo(ngo_owner.clone());
        let mut ngo: Ngo = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::NotRegistered)?;
        ngo.verified = false;
        env.storage().persistent().set(&key, &ngo);
        extend_instance_ttl(&env);
        extend_ngo_ttl(&env, &ngo_owner);

        env.events()
            .publish((symbol_short!("revoked"), ngo_owner), ());

        Ok(())
    }

    /// Bumps a registered NGO entry's storage TTL without changing
    /// anything about it. Callable by anyone — a verified NGO that
    /// `register`, `approve_ngo`, and `revoke_ngo` haven't touched in a
    /// while would otherwise have its entry archived after 90 days, with
    /// no other way to keep it alive.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// # use soroban_sdk::{testutils::Address as _, Address, Env, String};
    /// # use ngo_registry::{NgoRegistry, NgoRegistryClient};
    /// # let env = Env::default();
    /// # env.mock_all_auths();
    /// # let contract_id = env.register(NgoRegistry, ());
    /// # let client = NgoRegistryClient::new(&env, &contract_id);
    /// # let admin = Address::generate(&env);
    /// # client.init(&admin);
    /// # let owner = Address::generate(&env);
    /// # let name = String::from_str(&env, "Example NGO");
    /// # client.register(&owner, &name);
    /// client.touch_ngo(&owner);
    /// ```
    pub fn touch_ngo(env: Env, owner: Address) -> Result<(), Error> {
        if !env.storage().persistent().has(&DataKey::Ngo(owner.clone())) {
            return Err(Error::NotRegistered);
        }
        extend_instance_ttl(&env);
        extend_ngo_ttl(&env, &owner);
        Ok(())
    }
}

mod test;

# Storage keys and TTL policy

Which `DataKey` lives in which Soroban storage type, and how its TTL is
managed, directly affects fees and archival risk. This is written out here
so that isn't something you have to reconstruct from the constants in both
contracts' `lib.rs` files.

## Storage types, briefly

Soroban has two storage types relevant here (a third, temporary, isn't
used by either contract):

- **Instance** storage lives alongside the contract instance itself. It's
  cheap to read/write and its TTL is extended as a single unit — bumping
  one instance entry's TTL bumps them all.
- **Persistent** storage is per-entry: each entry has its own TTL and must
  be extended independently, or it can be archived once its TTL expires
  (still recoverable on-chain, but at extra cost to restore).

Both contracts extend TTLs eagerly on every state-changing call, so an
entry only expires from prolonged inactivity, not normal use. Ledger
counts below assume a 5-second average ledger close time
(`DAY_IN_LEDGERS = 17,280`, defined in each contract).

## `donation-vault`

| Key | Storage | Bump / threshold | Purpose |
| --- | --- | --- | --- |
| `Admin` | Instance | 30d / 29d | The address that can pause/unpause, set the fee, and manage the treasury. |
| `PendingAdmin` | Instance | 30d / 29d | Address proposed by `propose_admin`, awaiting `accept_admin`. |
| `NextStreamId` | Instance | 30d / 29d | Auto-incrementing counter handed out by `create_stream`. |
| `Paused` | Instance | 30d / 29d | Emergency-brake flag checked by `require_not_paused`. |
| `Treasury` | Instance | 30d / 29d | Address that receives the protocol fee cut on withdrawal. |
| `FeeBps` | Instance | 30d / 29d | Protocol fee, in basis points, capped at `MAX_FEE_BPS` (1,000 / 10%). |
| `CancelGraceLedgers` | Instance | 30d / 29d | Additional ledgers to retain cancelled stream records for indexing. |
| `Stream(u64)` | Persistent | 90d / 89d | One donor→NGO stream record, keyed by stream id. Extended on every `create_stream`, `withdraw`, `top_up`, `cancel_stream`, or rate change touching that stream. |

All instance keys share one TTL (bumped to 30 days, refreshed once it
would otherwise drop below 29 days remaining) via `extend_instance_ttl`,
called on every state-changing entry point. Each `Stream(u64)` entry gets
its own 90-day TTL via `extend_stream_ttl`, called whenever that specific
stream is touched — an untouched stream can still expire independently of
the instance and of other streams.

On cancellation, the stream TTL is bumped to the normal 90-day retention
period plus the configured `cancel_grace_ledgers`. This gives indexers a
configurable window to observe the cancellation before the record becomes
eligible for archival.

## `ngo-registry`

| Key | Storage | Bump / threshold | Purpose |
| --- | --- | --- | --- |
| `Admin` | Instance | 30d / 29d | The address that can `approve_ngo` / `revoke_ngo`. |
| `Ngo(Address)` | Persistent | 90d / 89d | One NGO's registry entry (name, verified flag), keyed by its owner address. |

As with `donation-vault`, the instance TTL is refreshed on every
state-changing call via `extend_instance_ttl`. Each `Ngo(Address)` entry
gets its own 90-day TTL via `extend_ngo_ttl`, refreshed by `register`,
`approve_ngo`, and `revoke_ngo` for that specific entry — an NGO that
registers once and is never approved, revoked, or re-touched can still
have its entry archived independently of the registry's admin data.

## Implication for fee estimation

Instance data is cheap to keep alive since one bump covers every instance
key at once. Persistent per-entry data (`Stream` and `Ngo` records) is
where inactivity risk concentrates — an old stream or NGO entry nobody
interacts with for 90 days becomes eligible for archival, and reading it
back afterward costs a restore in addition to the read.

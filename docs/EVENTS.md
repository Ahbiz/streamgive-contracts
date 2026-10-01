# Contract events

Every event published by the contracts in this repo, with its topics and
data shape, so an indexer can be written directly against this repo without
going to `streamgive-docs` first.

Soroban events have two parts:

- **Topics** â€” a tuple, always starting with a `Symbol` naming the event.
  Topics are indexed/filterable.
- **Data** â€” the event payload. Shown below as the Rust type(s) passed to
  `env.events().publish((topics...), data)`. A single value publishes as
  itself; a tuple of values publishes as an XDR array in that order.

`Address`, `i128`, `u64`, `u32`, `bool`, and `String` are the standard
Soroban SDK/XDR types (`Address` is the SDK's account/contract address
type, `String` is `soroban_sdk::String`).

## `ngo-registry`

### `register`

Emitted by `register` when an NGO submits an application.

| | |
|---|---|
| Topics | `("register", owner: Address)` |
| Data | `name: String` |

The entry starts unverified (`verified: false`); look for a matching
`approved` event for the same `owner` to know when it's live.

### `renamed`

Emitted by `update_name` when an NGO changes the name on its application
before it has been approved.

| | |
|---|---|
| Topics | `("renamed", owner: Address)` |
| Data | `name: String` (the new name) |

Once an `approved` event has been seen for an `owner`, no further
`renamed` events can follow for it while it stays verified.

### `approved`

Emitted by `approve_ngo` when an admin marks a registered NGO as verified.

| | |
|---|---|
| Topics | `("approved", ngo_owner: Address)` |
| Data | `()` (no payload) |

### `revoked`

Emitted by `revoke_ngo` when an admin reverses a prior approval, marking a
verified NGO as unverified again. This is the direct counterpart to
`approved` for the same `ngo_owner` â€” an indexer should treat a `revoked`
event as cancelling the most recent `approved` event for that address.

| | |
|---|---|
| Topics | `("revoked", ngo_owner: Address)` |
| Data | `()` (no payload) |

## `donation-vault`

### `propadmin`

Emitted by `propose_admin` when the current admin nominates a new admin.
The transfer is not complete until the nominated address calls
`accept_admin` and an `acptadmin` event is emitted.

| | |
|---|---|
| Topics | `("propadmin",)` |
| Data | `new_admin: Address` |

### `acptadmin`

Emitted by `accept_admin` when the nominated admin accepts the transfer.
After this event the address in `data` is the active admin; the previous
admin has no further authority.

| | |
|---|---|
| Topics | `("acptadmin",)` |
| Data | `new_admin: Address` |

### `pause`

Emitted by `pause` when an admin halts stream creation, withdrawal, top-up,
and rate changes.

| | |
|---|---|
| Topics | `("pause",)` |
| Data | `()` (no payload) |

### `unpause`

Emitted by `unpause` when an admin lifts a pause.

| | |
|---|---|
| Topics | `("unpause",)` |
| Data | `()` (no payload) |

### `feeset`

Emitted by `set_fee_bps` when an admin changes the protocol fee.

| | |
|---|---|
| Topics | `("feeset",)` |
| Data | `fee_bps: u32` (the new fee, in basis points) |

The fee is capped at `MAX_FEE_BPS` (1,000 / 10%); calls above the cap fail
with `FeeTooHigh` and emit nothing. The event carries the full new value
(not a delta), so an indexer can track the fee without polling `fee_bps`.
"Accepted" here means stored, not effective: the fee only affects payouts
once a treasury is set, so pair this with `set_treasury`/`treasury()` when
deriving an actual split.

### `created`

Emitted by `create_stream` when a donor opens a new stream.

| | |
|---|---|
| Topics | `("created", stream_id: u64)` |
| Data | `(donor: Address, ngo: Address, token: Address, deposit: i128, rate: i128)` |

`deposit` is the amount pulled from the donor into the vault; `rate` is how
much of it accrues to the NGO per second (see [`math::accrued`](../contracts/donation-vault/src/math.rs)).

### `withdraw`

Emitted by `withdraw` when an NGO claims everything accrued on a stream
since the last checkpoint, and once per contributing stream by
`withdraw_batch`.

| | |
|---|---|
| Topics | `("withdraw", stream_id: u64)` |
| Data | `accrued: i128` |

`accrued` is the gross amount released from the stream's balance â€” if a
protocol fee is configured (see `set_fee_bps`/`set_treasury`), the NGO
actually receives `accrued` minus the fee, with the fee paid to the
treasury in the same transaction. No separate fee event is emitted; derive
the split from the vault's `fee_bps()`/`treasury()` at the time of the
transaction.

A `withdraw_batch` call aggregates the gross accruals per token and makes a
single transfer per token, but still emits one `withdraw` event per stream
that had something to withdraw, so an indexer can track streams exactly as
it would with individual `withdraw` calls. Streams that had accrued nothing
are skipped and emit no event.

The `withdraw` entry point **returns** that same net amount (gross minus
fee), so a caller displaying the payout can use the return value directly;
the event data stays gross, matching the stream's `withdrawn` bookkeeping.

### `cancel`

Emitted by `cancel_stream` when a donor stops a stream for good.

| | |
|---|---|
| Topics | `("cancel", stream_id: u64)` |
| Data | `(accrued: i128, refund: i128)` |

`accrued` is whatever had accrued to the NGO and was paid out (subject to
the same protocol-fee split as `withdraw`) as part of settling the stream;
`refund` is the untouched remainder returned to the donor. The stream
record is kept with `balance` and `rate` zeroed, not deleted.

### `topup`

Emitted by `top_up` when a donor adds more funds to an existing stream.

| | |
|---|---|
| Topics | `("topup", stream_id: u64)` |
| Data | `amount: i128` |

`amount` is only the newly added deposit. Any balance already accrued at
the time of the top-up is settled to the NGO first (as its own implicit
payout, without emitting a `withdraw` event) before the deposit is added.

### `ratemod`

Emitted by `modify_rate` when a donor changes a stream's per-second accrual
rate.

| | |
|---|---|
| Topics | `("ratemod", stream_id: u64)` |
| Data | `(old_rate: i128, new_rate: i128)` |

As with `top_up`, any balance already accrued at the old rate is settled to
the NGO first, so the new rate only ever applies going forward. The event
records both values so an indexer can calculate the change without another
state query.

### `unregist`

Emitted by `ngo-registry` when an unverified NGO removes its own application.

| | |
|---|---|
| Topics | `("unregist", owner: Address)` |
| Data | `()` (no payload) |
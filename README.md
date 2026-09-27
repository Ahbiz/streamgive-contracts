# StreamGive — Contracts

Soroban smart contracts powering StreamGive, a recurring/streaming donation
platform for verified NGOs on Stellar.

## Contracts

- `ngo-registry` — on-chain NGO application, verification, and registry
- `donation-vault` — streaming donation vault (create / withdraw / cancel / modify streams)

## Release profile

The workspace `Cargo.toml`'s `[profile.release]` sets several non-default
flags. Soroban's resource-fee model charges per byte of the deployed wasm
and per CPU instruction executed, so a smaller, more predictable binary
isn't just nice-to-have — it directly lowers what every invocation of
these contracts costs:

| Setting             | Value       | Why                                                                                                   |
| -------------------- | ----------- | ------------------------------------------------------------------------------------------------------ |
| `opt-level`          | `"z"`       | Optimizes for binary size over speed — wasm size drives upload and storage fees.                       |
| `lto`                | `true`      | Whole-program link-time optimization, trimming dead code and shrinking the binary further.             |
| `codegen-units`      | `1`         | A single codegen unit gives the optimizer the whole crate to work with, trading build time for smaller output. |
| `panic`              | `"abort"`   | Drops unwinding tables and landing pads; Soroban traps on panic and can't unwind across the host boundary anyway. |
| `strip`              | `"symbols"` | Strips symbol/debug info from the deployed artifact — of no use on-chain, pure size cost otherwise.     |
| `debug`              | `0`         | No debug info emitted for release builds, same rationale as `strip`.                                    |
| `debug-assertions`   | `false`     | Standard release behavior — keeps hot paths free of debug-only checks.                                  |
| `overflow-checks`    | `true`      | Kept **on** in release, contrary to the Rust default — these contracts move token balances, and a silently wrapped `i128` is far worse than the small extra cost of a checked op. |

Change these with care: relaxing `opt-level`, `lto`, or `strip` grows the
deployed wasm and raises fees, while turning `overflow-checks` off would
let balance arithmetic wrap silently.

## Pausing

`donation-vault` has an admin-gated `pause` / `unpause` pair — an
emergency brake for when something is wrong. `pause` only flips a flag in
the instance storage: no funds are moved, so every balance stays exactly
where it was and there is nothing to unwind when the pause is lifted.

While the vault is paused, every entry point that moves tokens or changes
a stream rejects the call with `Error::ContractPaused` (code 6) before
touching storage or requiring any auth:

| Entry point     | While paused                                    |
| --------------- | ----------------------------------------------- |
| `create_stream` | Rejected                                        |
| `withdraw`      | Rejected                                        |
| `top_up`        | Rejected                                        |
| `modify_rate`   | Rejected                                        |
| `cancel_stream` | Still works — settles and refunds as usual      |

`withdraw` being on that list is the point of the brake: it is the only
path that pays tokens straight out of the vault, so a pause triggered by a
suspected vulnerability has to close it or an attacker could simply drain
funds while the rest of the contract is frozen.

`cancel_stream` is deliberately left open. It is the one path that returns
money to a donor, so keeping it available means a pause can never trap a
donor's unspent deposit. The read-only views (`admin`, `pending_admin`,
`get_stream`, `stream_count`, `pending_accrual`, `paused`, `treasury`,
`fee_bps`) and `extend_stream` also keep working, since none of them can
move funds, and `unpause` is of course still reachable.

## Related repositories

- [streamgive-backend](https://github.com/streamgive/streamgive-backend) — indexer & API
- [streamgive-frontend](https://github.com/streamgive/streamgive-frontend) — donor & NGO web app
- [streamgive-docs](https://github.com/streamgive/streamgive-docs) — documentation

## Testing

Run the full test suite for all contracts from the workspace root:

```sh
cargo test --workspace
```

To run tests for a single contract:

```sh
cargo test -p donation-vault
cargo test -p ngo-registry
```

Notable coverage:

- `donation-vault`'s `math` module unit-tests the streaming accrual
  calculation (`accrued`) directly: zero/negative rate, zero balance,
  zero elapsed time, capping at the remaining balance, and saturating
  instead of overflowing/panicking near `i128::MAX`.
- It also includes a deterministic grid-based invariant sweep
  (`invariants_hold_across_a_grid_of_inputs`) that checks, across a
  matrix of rates, balances, and elapsed durations, that accrual is
  always non-negative, never exceeds the remaining balance, and is
  monotonically non-decreasing as elapsed time (or rate) grows — a
  stand-in for property-based testing over the streaming math's edge
  cases.

CI (see [`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs
`cargo fmt --check`, `cargo clippy`, a `wasm32v1-none` release
build, a wasm binary size check (see
[`scripts/check-wasm-size.sh`](scripts/check-wasm-size.sh)), and
`cargo test --workspace` on every push and pull request.

## Error codes

Each contract exposes its failures as a `#[contracterror] enum Error`,
returned as `Result<_, Error>` from every fallible entry point. Clients see
the numeric code below (e.g. a failed `try_withdraw` surfacing `Error(5)`).

### `donation-vault`

| Code | Error                | Meaning                                                                 |
| ---- | --------------------- | ------------------------------------------------------------------------ |
| 1    | `AlreadyInitialized`  | `init` was already called; the vault already has an admin.               |
| 2    | `NotInitialized`      | `init` has not been called yet, so there is no admin to act as.          |
| 3    | `StreamNotFound`      | No stream exists for the given stream id.                                |
| 4    | `InvalidAmount`       | `deposit` or `rate` passed to `create_stream` was zero or negative.      |
| 5    | `NothingToWithdraw`   | The stream has accrued nothing since its last checkpoint.                |
| 6    | `ContractPaused`      | The admin has paused the vault; see [Pausing](#pausing) for what still works. |
| 7    | `FeeTooHigh`          | `set_fee_bps` was called with a value above the 10% (1,000 bps) cap.     |
| 8    | `NoPendingAdmin`      | `accept_admin` was called without a prior (or already-completed) `propose_admin`. |

### `ngo-registry`

| Code | Error                | Meaning                                                          |
| ---- | --------------------- | ------------------------------------------------------------------ |
| 1    | `AlreadyInitialized`  | `init` was already called; the registry already has an admin.    |
| 2    | `NotInitialized`      | `init` has not been called yet, so there is no admin to act as.  |
| 3    | `AlreadyRegistered`   | `register` was called for an address that already has an entry. |
| 4    | `NotRegistered`       | No registry entry exists for the given owner address.            |

## Status

Early development.

## License

Apache-2.0 — see [LICENSE](./LICENSE).

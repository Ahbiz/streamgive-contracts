# StreamGive — Contracts

Soroban smart contracts powering StreamGive, a recurring/streaming donation
platform for verified NGOs on Stellar.

For how these contracts fit with the backend and frontend — and how a
donation flows end to end — see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

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
Note that pausing does **not** stop time-based accrual. A stream's
`pending_accrual` keeps growing while the vault is paused, so a stream
paused for a week still owes a week of accrual once the pause is lifted.
That accrual is claimable via `withdraw` as soon as the vault is
unpaused.


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
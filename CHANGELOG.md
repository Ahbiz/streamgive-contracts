# Changelog

All notable changes to this repository's contracts will be documented in
this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project does not yet follow a formal versioning scheme — each contract's
`Cargo.toml` still reads `0.1.0` — so entries are grouped under
[Unreleased] until the first tagged release.

## [Unreleased]

### Added

- `donation-vault`: contract skeleton with stream storage schema.
- `donation-vault`: `create_stream`, `withdraw`, `cancel_stream`, `top_up`,
  and `modify_rate` for the streaming donation lifecycle.
- `donation-vault`: admin-gated `pause` / `unpause` for fund-moving actions.
- `donation-vault`: optional protocol fee (`set_fee_bps` / `fee_bps`) with
  treasury payout (`set_treasury` / `treasury`).
- `donation-vault`: read-only `pending_accrual` view.
- `donation-vault`: two-step admin transfer via `propose_admin` /
  `accept_admin`.
- `ngo-registry`: contract skeleton with storage types and `init`.
- `ngo-registry`: NGO application/registration via `register`.
- `ngo-registry`: admin-gated `approve_ngo`.
- `ngo-registry`: admin-gated `revoke_ngo`.
- `ngo-registry`: owner-gated `update_name` for fixing an application's
  name before approval; rejected with `Error::AlreadyVerified` after.
- Contract events for registry and vault state changes (see
  [`docs/EVENTS.md`](docs/EVENTS.md)).
- `scripts/deploy-testnet.sh` for deploying both contracts to testnet.
- CI workflow running `cargo fmt --check`, `cargo clippy`, a
  `wasm32v1-none` release build, and `cargo test --workspace`.

### Changed

- `donation-vault`: stream `balance`/`withdrawn` updates and the stream-id
  counter use checked arithmetic, returning `Error::ArithmeticOverflow`
  instead of panicking on overflow.
- `donation-vault`: `create_stream` rejects a stream whose `donor` and `ngo`
  are the same address with `Error::SelfStream`, so a deposit can't be
  counted as a committed donation while streaming straight back to its donor.

[Unreleased]: https://github.com/StreamGive/streamgive-contracts/compare/main...HEAD

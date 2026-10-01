# Gas and resource estimation

Soroban does not charge "gas" per opcode. Every invocation is simulated
first, and the simulation reports the resources it needs. This guide shows
how to simulate each `donation-vault` entry point with the `stellar` CLI and
how to read the result.

## Simulating an entry point

`stellar contract invoke` simulates before it sends. Pass `--send=no` to
stop after the simulation (nothing is submitted, nothing is spent) and
`--cost` to print the resource usage to stderr:

```sh
stellar contract invoke \
  --id <DONATION_VAULT_ID> \
  --source <identity> \
  --network testnet \
  --send=no \
  --cost \
  -- <function> --<arg> <value> ...
```

- Take `<DONATION_VAULT_ID>` from [`deployments.json`](../deployments.json),
  or deploy your own copy (see "Deploying" in the [README](../README.md)).
- `--source` must be an identity that can authorize the call. For
  `create_stream` that is the donor, because the vault calls
  `donor.require_auth()`.
- Argument names are the function's parameter names. Run
  `... -- <function> --help` to see the exact spelling for your CLI version.
- `--send=no` only simulates. The default (`--send=default`) sends the
  transaction when the simulation shows ledger writes, events, or auth, so
  always pass `--send=no` when you only want an estimate.
- Simulation reflects the ledger state at that moment. A `withdraw` on a
  stream that has accrued nothing costs less than one that pays out, so
  simulate against a state that matches the call you care about.

### Examples

```sh
# Open a stream: 1,000,000 units at 10 units/second.
stellar contract invoke --id <ID> --source donor --network testnet \
  --send=no --cost \
  -- create_stream --donor <DONOR_G_ADDRESS> --ngo <NGO_G_ADDRESS> \
     --token <TOKEN_CONTRACT_ID> --deposit 1000000 --rate 10

# Withdraw what has accrued so far (use the identity that authorizes it).
stellar contract invoke --id <ID> --source <identity> --network testnet \
  --send=no --cost \
  -- withdraw --stream_id 1

# Cancel and refund the remainder (use the identity that authorizes it).
stellar contract invoke --id <ID> --source <identity> --network testnet \
  --send=no --cost \
  -- cancel_stream --stream_id 1
```

### Entry points to simulate

Every contract function can be simulated the same way. State-changing
functions are the ones worth measuring:

| Group | Functions |
| --- | --- |
| Streams | `create_stream`, `withdraw`, `cancel_stream`, `top_up`, `modify_rate`, `extend_stream` |
| Admin | `init`, `propose_admin`, `accept_admin`, `cancel_admin_proposal`, `pause`, `unpause`, `upgrade` |
| Configuration | `set_treasury`, `clear_treasury`, `set_fee_bps`, `set_min_deposit`, `set_cancel_grace_ledgers`, `set_max_streams_per_donor`, `set_registry` |
| Read-only | `admin`, `pending_admin`, `get_stream`, `stream_count`, `pending_accrual`, `pending_payout`, `paused`, `treasury`, `fee_bps`, `min_deposit`, `cancel_grace_ledgers`, `max_streams_per_donor`, `registry` |

Read-only functions still consume CPU and read entries, but write nothing.

## Reading the result

| Resource | What it is |
| --- | --- |
| Instructions | CPU instructions the invocation executes. |
| Memory | Peak memory the invocation uses, in bytes. |
| Read entries / read bytes | Ledger entries the call reads, and their total size. |
| Write entries / write bytes | Ledger entries the call writes, and their total size. |
| Events size | Total size of the contract events the call emits. |
| Rent | Extra fee for extending the TTL of entries the call touches. |

Each resource has a per-transaction network limit, and the transaction fee
is derived from these numbers. Check the current limits and fee schedule in
the [Stellar resource limits and fees documentation](https://developers.stellar.org/docs/networks/resource-limits-fees)
rather than relying on numbers copied into a repo, since they change with
network upgrades.

To leave headroom for small differences between simulation and execution,
pass `--instruction-leeway <N>` to allow extra instructions when the CLI
budgets the transaction.

## Sample resource footprints

These numbers were measured with the Soroban SDK's test environment
(`env.cost_estimate().resources()`, `soroban-sdk` 27.0.6), invoking each
function once against the built-in Stellar Asset Contract. Scenario:

1. Deposit 1,000,000 units at a rate of 10 units/second.
2. `withdraw` 100 seconds later (pays out 1,000).
3. `cancel_stream` another 100 seconds later (refunds 998,000).

| Function | Instructions | Memory (bytes) | Read entries | Write entries | Read bytes | Write bytes | Events size (bytes) |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `create_stream` | 329,687 | 46,723 | 10 | 6 | 92 | 1,500 | 488 |
| `withdraw` | 283,654 | 40,923 | 9 | 4 | 92 | 1,036 | 336 |
| `cancel_stream` | 470,666 | 61,521 | 9 | 5 | 0 | 1,264 | 604 |

Read entries counts every entry the call reads. In the test environment,
most of them are already in memory, and read bytes counts only the entries
loaded from disk.

Treat these as relative sizes, not fee quotes:

- They come from the SDK's host running natively, so instruction counts can
  differ from what `stellar contract invoke --cost` reports against the
  network. Simulate on testnet for figures you plan to budget against.
- They depend on the token. A custom token contract will cost more than the
  Stellar Asset Contract, since token calls run inside each of these
  invocations.
- Of the three, `cancel_stream` used the most instructions and memory in
  this scenario, and `withdraw` the fewest.
- Rent depends on ledger state and TTL settings, so it is not tabulated.

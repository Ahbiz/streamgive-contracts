/// Computes how much of `balance` has unlocked given a constant per-second
/// `rate` sustained over `elapsed` seconds, capped so it can never exceed
/// what's actually left in the stream.
///
/// Returns 0 for a non-positive rate/balance or zero elapsed time, and
/// saturates instead of overflowing/panicking if `rate * elapsed` would
/// exceed i128's range.
pub fn accrued(rate: i128, elapsed: u64, balance: i128) -> i128 {
    if rate <= 0 || balance <= 0 || elapsed == 0 {
        return 0;
    }

    let unlocked = rate.saturating_mul(elapsed as i128);
    unlocked.min(balance)
}

#[cfg(test)]
mod test {
    use super::accrued;

    #[test]
    fn zero_rate_accrues_nothing() {
        assert_eq!(accrued(0, 100, 1_000), 0);
    }

    #[test]
    fn negative_rate_accrues_nothing() {
        assert_eq!(accrued(-5, 100, 1_000), 0);
    }

    #[test]
    fn zero_elapsed_accrues_nothing() {
        assert_eq!(accrued(10, 0, 1_000), 0);
    }

    #[test]
    fn zero_balance_accrues_nothing() {
        assert_eq!(accrued(10, 100, 0), 0);
    }

    #[test]
    fn accrues_rate_times_elapsed_under_balance() {
        assert_eq!(accrued(10, 5, 1_000), 50);
    }

    #[test]
    fn caps_at_remaining_balance() {
        assert_eq!(accrued(10, 1_000, 500), 500);
    }

    #[test]
    fn saturates_instead_of_overflowing() {
        assert_eq!(accrued(i128::MAX, u64::MAX, i128::MAX), i128::MAX);
    }

    #[test]
    fn saturates_at_various_overflow_boundaries() {
        // rate * elapsed overflows i128 well before either operand hits its
        // own max — these combinations all overflow the raw multiplication
        // and must saturate to `balance`, not panic or wrap.
        assert_eq!(accrued(i128::MAX, 2, 1_000), 1_000);
        assert_eq!(accrued(i128::MAX / 2, u64::MAX, i128::MAX), i128::MAX);
        assert_eq!(accrued(1_000_000_000_000, u64::MAX, 500), 500);
    }

    #[test]
    fn rapid_cancel_ticks_never_go_negative_or_panic() {
        // Cancelling one ledger (or zero) after creation is the minimal
        // "rapid cancel" case — accrual over 0 or 1 second should be tiny
        // (or zero) and never negative.
        for &rate in &[0i128, 1, 1_000, i128::MAX] {
            for &balance in &[0i128, 1, 1_000, i128::MAX] {
                assert_eq!(accrued(rate, 0, balance), 0);
                let one_tick = accrued(rate, 1, balance);
                assert!(one_tick >= 0);
                assert!(one_tick <= balance.max(0));
            }
        }
    }

    /// Deterministic stand-in for a property test: sweeps a grid of rates,
    /// balances, and elapsed durations (including the zero-rate,
    /// near-overflow, and zero/near-zero-elapsed edges) and checks the
    /// invariants that must hold for every input rather than a handful of
    /// hand-picked examples.
    #[test]
    fn invariants_hold_across_a_grid_of_inputs() {
        let rates = [0i128, 1, 7, 10_000, 1_000_000_000, i128::MAX / 2, i128::MAX];
        let balances = [0i128, 1, 999, 1_000_000, i128::MAX];
        let elapsed_steps = [0u64, 1, 2, 100, 10_000, u64::MAX];

        for &rate in &rates {
            for &balance in &balances {
                let mut prev = 0i128;
                for &elapsed in &elapsed_steps {
                    let a = accrued(rate, elapsed, balance);

                    // Never negative, never more than what's left in the stream.
                    assert!(
                        a >= 0,
                        "negative accrual: rate={rate} elapsed={elapsed} balance={balance}"
                    );
                    assert!(
                        a <= balance.max(0),
                        "accrual exceeds balance: rate={rate} elapsed={elapsed} balance={balance}"
                    );

                    // More elapsed time never accrues less (accrual is
                    // monotonic non-decreasing in elapsed, even once capped).
                    assert!(
                        a >= prev,
                        "accrual decreased as elapsed grew: rate={rate} elapsed={elapsed} balance={balance}"
                    );
                    prev = a;
                }
            }
        }
    }

    /// Randomised counterpart to the grid test above: the same invariants
    /// (non-negative, capped, monotonic), but over inputs drawn from the whole
    /// i128/u64 range, biased towards the zero and near-`MAX` edges the grid
    /// only samples at a few fixed points.
    mod fuzz {
        use super::accrued;
        use proptest::prelude::*;

        fn any_rate() -> impl Strategy<Value = i128> {
            prop_oneof![
                any::<i128>(),
                Just(0i128),
                Just(i128::MAX),
                0i128..=1_000,
                (i128::MAX - 1_000)..=i128::MAX,
                i128::MIN..=-1,
            ]
        }

        fn any_balance() -> impl Strategy<Value = i128> {
            any_rate()
        }

        fn any_elapsed() -> impl Strategy<Value = u64> {
            prop_oneof![
                any::<u64>(),
                Just(0u64),
                Just(u64::MAX),
                0u64..=1_000,
                (u64::MAX - 1_000)..=u64::MAX,
            ]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(10_000))]

            /// Never negative, never more than what's left in the stream, and
            /// never panics — for every input, including overflowing ones.
            #[test]
            fn non_negative_and_capped_by_balance(
                rate in any_rate(),
                elapsed in any_elapsed(),
                balance in any_balance(),
            ) {
                let a = accrued(rate, elapsed, balance);
                prop_assert!(a >= 0, "negative accrual: {a}");
                prop_assert!(a <= balance.max(0), "accrual {a} exceeds balance {balance}");
            }

            /// More elapsed time never accrues less, even once capped.
            #[test]
            fn monotonic_in_elapsed(
                rate in any_rate(),
                e1 in any_elapsed(),
                e2 in any_elapsed(),
                balance in any_balance(),
            ) {
                let (lo, hi) = if e1 <= e2 { (e1, e2) } else { (e2, e1) };
                prop_assert!(accrued(rate, lo, balance) <= accrued(rate, hi, balance));
            }

            /// A higher rate never accrues less for the same elapsed and balance.
            #[test]
            fn monotonic_in_rate(
                r1 in any_rate(),
                r2 in any_rate(),
                elapsed in any_elapsed(),
                balance in any_balance(),
            ) {
                let (lo, hi) = if r1 <= r2 { (r1, r2) } else { (r2, r1) };
                prop_assert!(accrued(lo, elapsed, balance) <= accrued(hi, elapsed, balance));
            }

            /// A larger balance never lowers the accrual: the cap only loosens.
            #[test]
            fn monotonic_in_balance(
                rate in any_rate(),
                elapsed in any_elapsed(),
                b1 in any_balance(),
                b2 in any_balance(),
            ) {
                let (lo, hi) = if b1 <= b2 { (b1, b2) } else { (b2, b1) };
                prop_assert!(accrued(rate, elapsed, lo) <= accrued(rate, elapsed, hi));
            }

            /// Non-positive rate or balance, or zero elapsed, accrues nothing.
            #[test]
            fn degenerate_inputs_accrue_nothing(
                rate in any_rate(),
                elapsed in any_elapsed(),
                balance in any_balance(),
            ) {
                if rate <= 0 || balance <= 0 || elapsed == 0 {
                    prop_assert_eq!(accrued(rate, elapsed, balance), 0);
                }
            }

            /// Whenever `rate * elapsed` fits in i128, the result is exactly
            /// `min(rate * elapsed, balance)`; when it doesn't, it saturates
            /// to `balance`.
            #[test]
            fn matches_exact_arithmetic(
                rate in 1i128..=i128::MAX,
                elapsed in 1u64..=u64::MAX,
                balance in 1i128..=i128::MAX,
            ) {
                let expected = match rate.checked_mul(elapsed as i128) {
                    Some(unlocked) => unlocked.min(balance),
                    None => balance,
                };
                prop_assert_eq!(accrued(rate, elapsed, balance), expected);
            }
        }
    }

    #[test]
    fn monotonic_in_rate_for_fixed_elapsed_and_balance() {
        let rates = [0i128, 1, 5, 50, 500, i128::MAX];
        let balance = 10_000i128;
        let elapsed = 10u64;

        let mut prev = 0i128;
        for &rate in &rates {
            let a = accrued(rate, elapsed, balance);
            assert!(a >= prev, "accrual decreased as rate grew: rate={rate}");
            prev = a;
        }
    }
}

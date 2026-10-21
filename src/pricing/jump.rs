// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! The jump guard: hold quoting when a feed print jumps away from the last
//! one Stitch trusted.
//!
//! A fresh, finite, positive print can still be wrong: ten times the real
//! price, a decimal shifted, another pair's rate. Nothing about the print
//! itself says so, and checking a quote against the print it was built
//! from (the price band) cannot catch it either. What does give it away is
//! the jump. This keeps a trailing reference — the last print it trusted —
//! and refuses a print more than `max_jump_bps` away from it, so the bot
//! goes dark instead of signing at the wrong price. No second source.
//!
//! Two cases need a rule of their own:
//!
//! - **Startup.** There is nothing to compare the first print against, and
//!   trusting it would make a glitch at startup the reference everything
//!   after is judged by. So the first print is a candidate, and Stitch
//!   starts quoting once a second read agrees with it within
//!   `max_jump_bps`. The shared cache requires a poll started at least a
//!   second later, even across producers. On a cron-sampled feed
//!   that is the same sample read twice, which rules out a one-off bad
//!   response but not a bad sample; waiting for the next sample instead
//!   would keep a cNGN bot dark for up to three minutes after every
//!   restart, and the cNGN sampler already freezes suspect jumps itself.
//! - **A real fast move.** A print past the threshold becomes the candidate
//!   for a new level. If the feed keeps printing within `max_jump_bps` of
//!   it until `settle_secs` has passed on the feed's own clock, it is a
//!   move, not a glitch: Stitch trusts it and quotes again. A print back
//!   near the trusted one drops the candidate. Re-serving the same print
//!   does not count as holding the level.
//!
//! What it cannot do: a feed that is wrong from the first print, or that
//! drifts in steps under the threshold, or that stays wrong for longer than
//! `settle_secs`, gets through. Catching those needs a second, independent
//! source.

/// How far is too far, and how long a new level must hold.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct JumpGuard {
    pub max_jump_bps: u32,
    pub settle_secs: u64,
}

/// One observation: the print and the feed's own timestamp for it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Print {
    pub price: f64,
    pub observed_at: u64,
}

/// A level the feed moved to that Stitch has not trusted yet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The first print at the new level; later ones must agree with it.
    pub first: Print,
}

/// What the guard remembers for one feed.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PrintHistory {
    pub trusted: Option<Print>,
    pub candidate: Option<Candidate>,
    /// A feed timestamp the feed gave two prices more than `max_jump_bps`
    /// apart for, as saved before a restart (see [`super::anchor`]). Until a print after it is trusted,
    /// a print at or before it is held and never becomes the candidate.
    pub contradicted_at: Option<u64>,
}

/// Why a print was held back.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hold {
    /// No trusted print yet; waiting for a second read to agree.
    WarmingUp,
    /// The print is `move_bps` away from the trusted one. The new level was
    /// first seen at `since` (feed time) and is trusted at
    /// `since + settle_secs` if it holds.
    Jumped {
        trusted: f64,
        print: f64,
        move_bps: u64,
        since: u64,
    },
    /// Older than the trusted print and out of line with it.
    Regressed,
    /// At or before `at`, a feed timestamp the feed gave two conflicting
    /// prices for.
    Contradicted { at: u64 },
}

impl Hold {
    /// One line for logs and the panel.
    pub fn describe(&self, settle_secs: u64) -> String {
        match self {
            Hold::WarmingUp => {
                "Waiting for a second read of the reference price to confirm the first before quoting"
                    .into()
            }
            Hold::Jumped {
                trusted,
                print,
                move_bps,
                since,
            } => format!(
                "Reference price jumped {:.2}% ({trusted} to {print}). Not quoting until the feed comes back, or holds the new level until {} (feed time, {settle_secs}s)",
                *move_bps as f64 / 100.0,
                since + settle_secs,
            ),
            Hold::Regressed => {
                "An older reference price out of line with the trusted one; ignoring it".into()
            }
            Hold::Contradicted { at } => format!(
                "The reference feed gave two conflicting prices for feed time {at}. Not quoting until it prints a newer one"
            ),
        }
    }
}

/// What the guard made of one print.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    /// Quote off it.
    Trusted,
    /// Quote off it: a new level held for `settle_secs`. Worth a warning.
    Reanchored { from: f64 },
    /// Do not quote off it.
    Held(Hold),
}

/// How far apart two positive prices are, in bps of the smaller one:
/// `(max / min − 1) × 10 000`, so a 10x move up and a 10x move down are the
/// same 90 000.
pub fn move_bps(a: f64, b: f64) -> f64 {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    (hi / lo - 1.0) * 10_000.0
}

/// Whether two prints are within `max_jump_bps` of each other. Compared as
/// `hi <= lo × (1 + max)` rather than through `move_bps`, so a move of
/// exactly the threshold is not lost to the subtraction's rounding.
pub fn agrees(a: f64, b: f64, guard: &JumpGuard) -> bool {
    let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
    hi <= lo * (1.0 + f64::from(guard.max_jump_bps) / 10_000.0)
}

fn trust(print: Print) -> PrintHistory {
    PrintHistory {
        trusted: Some(print),
        candidate: None,
        contradicted_at: None,
    }
}

/// Judge one print. Pure: returns the new history with the verdict. The
/// caller has already refused a stale or unusable print; this only looks
/// at how it relates to what came before.
pub fn observe(history: PrintHistory, print: Print, guard: &JumpGuard) -> (PrintHistory, Verdict) {
    if let Some(at) = history
        .contradicted_at
        .filter(|at| print.observed_at <= *at)
    {
        return (history, Verdict::Held(Hold::Contradicted { at }));
    }
    let (next, verdict) = judge(history, print, guard);
    // Once a later print is trusted, an older one is judged against it
    // like any other lagging print.
    let contradicted_at = history.contradicted_at.filter(|_| next.trusted.is_none());
    (
        PrintHistory {
            contradicted_at,
            ..next
        },
        verdict,
    )
}

fn judge(history: PrintHistory, print: Print, guard: &JumpGuard) -> (PrintHistory, Verdict) {
    match history.trusted {
        None => warm_up(history.candidate, print, guard),
        // An older print (a lagging replica or cache) says nothing about
        // where the market is now. It is as quotable as before the guard
        // existed if it agrees with the trusted print, and either way it
        // never moves the reference back.
        Some(trusted) if print.observed_at < trusted.observed_at => {
            let verdict = if agrees(trusted.price, print.price, guard) {
                Verdict::Trusted
            } else {
                Verdict::Held(Hold::Regressed)
            };
            (history, verdict)
        }
        // Including a different price at the trusted print's own timestamp.
        // On a live feed that is the same market a moment apart: the price
        // API floors timestamps to seconds, and its tasks cache quotes
        // separately, so two reads can carry one second and two prices.
        Some(trusted) if agrees(trusted.price, print.price, guard) => {
            (trust(print), Verdict::Trusted)
        }
        Some(trusted) => jumped(history, trusted, print, guard),
    }
}

fn warm_up(
    candidate: Option<Candidate>,
    print: Print,
    guard: &JumpGuard,
) -> (PrintHistory, Verdict) {
    match candidate {
        Some(c)
            if print.observed_at >= c.first.observed_at
                && agrees(c.first.price, print.price, guard) =>
        {
            (trust(print), Verdict::Trusted)
        }
        _ => (
            PrintHistory {
                trusted: None,
                candidate: Some(Candidate { first: print }),
                contradicted_at: None,
            },
            Verdict::Held(Hold::WarmingUp),
        ),
    }
}

fn jumped(
    history: PrintHistory,
    trusted: Print,
    print: Print,
    guard: &JumpGuard,
) -> (PrintHistory, Verdict) {
    let candidate = history
        .candidate
        .filter(|c| agrees(c.first.price, print.price, guard))
        .unwrap_or(Candidate { first: print });
    let held_for = print
        .observed_at
        .saturating_sub(candidate.first.observed_at);
    if print.observed_at > candidate.first.observed_at && held_for >= guard.settle_secs {
        return (
            trust(print),
            Verdict::Reanchored {
                from: trusted.price,
            },
        );
    }
    (
        PrintHistory {
            trusted: Some(trusted),
            candidate: Some(candidate),
            contradicted_at: None,
        },
        Verdict::Held(Hold::Jumped {
            trusted: trusted.price,
            print: print.price,
            move_bps: move_bps(trusted.price, print.price) as u64,
            since: candidate.first.observed_at,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const GUARD: JumpGuard = JumpGuard {
        max_jump_bps: 1_000,
        settle_secs: 300,
    };

    fn p(price: f64, observed_at: u64) -> Print {
        Print { price, observed_at }
    }

    /// Run prints through the guard from a fresh history.
    fn run(prints: &[Print]) -> (PrintHistory, Vec<Verdict>) {
        prints.iter().fold(
            (PrintHistory::default(), Vec::new()),
            |(h, mut out), print| {
                let (h, v) = observe(h, *print, &GUARD);
                out.push(v);
                (h, out)
            },
        )
    }

    fn trusted_after(prints: &[Print]) -> PrintHistory {
        let (h, v) = run(prints);
        assert!(matches!(v.last(), Some(Verdict::Trusted)), "{v:?}");
        h
    }

    #[test]
    fn moves_are_symmetric_ratios() {
        assert_eq!(move_bps(1.0, 10.0), 90_000.0);
        assert_eq!(move_bps(10.0, 1.0), 90_000.0);
        assert!((move_bps(1.0, 1.1) - 1_000.0).abs() < 1e-6);
        assert_eq!(move_bps(2.0, 2.0), 0.0);
    }

    #[test]
    fn nothing_at_a_contradicted_timestamp_is_trusted_until_a_newer_print_is() {
        let restarted = PrintHistory {
            contradicted_at: Some(200),
            ..PrintHistory::default()
        };
        let held = Verdict::Held(Hold::Contradicted { at: 200 });
        // Read again and again, the contradicted print never warms up, and
        // nor does an older one.
        let (h, v) = observe(restarted, p(5.0, 200), &GUARD);
        assert_eq!((h, v), (restarted, held));
        let (h, v) = observe(h, p(5.0, 200), &GUARD);
        assert_eq!((h, v), (restarted, held));
        assert_eq!(observe(h, p(5.0, 150), &GUARD).1, held);
        // A newer print starts the usual wait. A lagging read of the
        // contradicted one does not take its place as the candidate.
        let (h, v) = observe(h, p(5.0, 201), &GUARD);
        assert_eq!(v, Verdict::Held(Hold::WarmingUp));
        let (h, v) = observe(h, p(1.0, 200), &GUARD);
        assert_eq!(v, held);
        let (h, v) = observe(h, p(5.0, 201), &GUARD);
        assert_eq!(v, Verdict::Trusted);
        assert_eq!(h, trust(p(5.0, 201)), "trusted, and the mark is gone");
        // From here an older print is judged against the trusted one.
        assert_eq!(observe(h, p(5.0, 200), &GUARD).1, Verdict::Trusted);
    }

    #[test]
    fn a_new_price_at_the_trusted_timestamp_is_judged_like_any_other() {
        let h = trust(p(1.0, 100));
        assert_eq!(
            observe(h, p(1.05, 100), &GUARD),
            (trust(p(1.05, 100)), Verdict::Trusted)
        );
        let (_, v) = observe(h, p(5.0, 100), &GUARD);
        assert!(matches!(v, Verdict::Held(Hold::Jumped { .. })), "{v:?}");
        // And at startup: two nearby prices for one second confirm each other.
        let (_, v) = run(&[p(1.0, 100), p(1.05, 100)]);
        assert_eq!(v[1], Verdict::Trusted);
    }

    #[test]
    fn startup_waits_for_a_second_read_that_agrees() {
        let (_, v) = run(&[p(0.5, 100)]);
        assert_eq!(v, [Verdict::Held(Hold::WarmingUp)]);
        // A second read of the same sample confirms it: a cron feed is not
        // kept dark until its next sample.
        let (_, v) = run(&[p(0.5, 100), p(0.5, 100)]);
        assert_eq!(v[1], Verdict::Trusted);
        // So does a later one inside the threshold.
        let (h, v) = run(&[p(0.5, 100), p(0.51, 101)]);
        assert_eq!(v[1], Verdict::Trusted);
        assert_eq!(h.trusted, Some(p(0.51, 101)));
        // An older one starts the wait again.
        let (_, v) = run(&[p(0.5, 100), p(0.5, 99)]);
        assert_eq!(v[1], Verdict::Held(Hold::WarmingUp));
    }

    #[test]
    fn a_glitch_at_startup_is_never_trusted_and_does_not_poison_the_reference() {
        // First print 10x; the next one is right and disagrees, so it
        // becomes the candidate; the one after confirms it.
        let (h, v) = run(&[p(5.0, 100), p(0.5, 101), p(0.5, 102)]);
        assert_eq!(
            v,
            [
                Verdict::Held(Hold::WarmingUp),
                Verdict::Held(Hold::WarmingUp),
                Verdict::Trusted
            ]
        );
        assert_eq!(h.trusted.unwrap().price, 0.5);
    }

    #[test]
    fn a_10x_or_decimal_shift_print_is_held_and_the_feed_coming_back_resumes() {
        let h = trusted_after(&[p(0.5, 100), p(0.5, 101)]);
        for glitch in [5.0, 0.05, 0.5e-3, 500.0] {
            let (held, v) = observe(h, p(glitch, 102), &GUARD);
            assert!(
                matches!(v, Verdict::Held(Hold::Jumped { since: 102, .. })),
                "{glitch}: {v:?}"
            );
            assert_eq!(held.trusted, h.trusted, "the reference does not move");
            let (back, v) = observe(held, p(0.502, 103), &GUARD);
            assert_eq!(v, Verdict::Trusted);
            assert!(back.candidate.is_none(), "coming back drops the candidate");
        }
    }

    #[test]
    fn a_new_level_that_holds_for_settle_secs_is_trusted() {
        // A real 30 % step, then the feed stays there.
        let h = trusted_after(&[p(1.0, 100), p(1.0, 101)]);
        let (h, v) = observe(h, p(0.7, 200), &GUARD);
        assert!(matches!(v, Verdict::Held(Hold::Jumped { since: 200, .. })));
        let (h, v) = observe(h, p(0.71, 400), &GUARD);
        assert!(matches!(v, Verdict::Held(Hold::Jumped { since: 200, .. })));
        // One second short of 300 s on the feed's clock.
        let (h, v) = observe(h, p(0.705, 499), &GUARD);
        assert!(matches!(v, Verdict::Held(_)));
        let (h, v) = observe(h, p(0.7, 500), &GUARD);
        assert_eq!(v, Verdict::Reanchored { from: 1.0 });
        assert_eq!(h.trusted, Some(p(0.7, 500)));
        // And it is the reference from here on.
        let (_, v) = observe(h, p(0.69, 501), &GUARD);
        assert_eq!(v, Verdict::Trusted);
    }

    #[test]
    fn re_serving_the_jumped_print_does_not_count_as_holding_it() {
        let h = trusted_after(&[p(1.0, 100), p(1.0, 101)]);
        let (h, _) = observe(h, p(10.0, 200), &GUARD);
        // A cron feed re-serving one bad sample for an hour.
        let (_, v) = observe(h, p(10.0, 200), &GUARD);
        assert!(matches!(v, Verdict::Held(_)));
    }

    #[test]
    fn a_candidate_that_wanders_restarts_its_clock() {
        let h = trusted_after(&[p(1.0, 100), p(1.0, 101)]);
        let (h, _) = observe(h, p(2.0, 200), &GUARD);
        // 3.0 is a jump from the trusted 1.0 and from the candidate 2.0.
        let (h, v) = observe(h, p(3.0, 600), &GUARD);
        assert!(matches!(v, Verdict::Held(Hold::Jumped { since: 600, .. })));
        assert_eq!(h.candidate.unwrap().first, p(3.0, 600));
    }

    #[test]
    fn an_older_print_never_moves_the_reference_back() {
        let h = trusted_after(&[p(1.0, 100), p(1.0, 101)]);
        // A lagging replica a sample behind, in line: quotable as before.
        let (after, v) = observe(h, p(1.01, 90), &GUARD);
        assert_eq!(v, Verdict::Trusted);
        assert_eq!(after, h, "the reference stays on the newer print");
        // Behind and out of line: ignored.
        let (after, v) = observe(h, p(10.0, 90), &GUARD);
        assert_eq!(v, Verdict::Held(Hold::Regressed));
        assert_eq!(after, h);
    }

    #[test]
    fn the_threshold_is_inclusive() {
        let h = trusted_after(&[p(1.0, 100), p(1.0, 101)]);
        let (_, v) = observe(h, p(1.1, 102), &GUARD);
        assert_eq!(v, Verdict::Trusted);
        let (_, v) = observe(h, p(1.1001, 102), &GUARD);
        assert!(matches!(v, Verdict::Held(_)));
    }

    #[test]
    fn the_hold_says_what_happened() {
        let line = Hold::Jumped {
            trusted: 0.5,
            print: 5.0,
            move_bps: 90_000,
            since: 1_000,
        }
        .describe(300);
        assert!(line.contains("900.00%"), "{line}");
        assert!(line.contains("1300"), "{line}");
        assert!(Hold::WarmingUp.describe(300).contains("second read"));
    }
}

//! The differential oracle (C4 of the RE workbench): run an original and a candidate library over
//! the same corpus and compare their observable behavior. This is how "behaviorally matching" is
//! proven — relative to the corpus, never universally.
//!
//! For each input the oracle compares the return registers and the contents of every output
//! buffer. A call that faults in one build but returns in the other is itself a divergence (named
//! `"fault"`), recorded rather than aborting the run. The run always visits every input so `total`
//! reflects the whole corpus; `matched` counts full matches and `first_divergence` is the earliest
//! disagreement.

use crate::argspec::{Arg, CallSpecResult};
use crate::Session;

/// The outcome of a differential run.
#[derive(Debug, Clone)]
pub struct DiffResult {
    /// How many inputs were run (the corpus size).
    pub total: usize,
    /// How many inputs matched on every compared observable.
    pub matched: usize,
    /// The earliest disagreement, if any.
    pub first_divergence: Option<Divergence>,
}

/// One disagreement between original and candidate.
#[derive(Debug, Clone)]
pub struct Divergence {
    /// The corpus index where it occurred.
    pub index: usize,
    /// Which observable disagreed: `"ret"`, `"ret1"`, `"out_buffers"`, `"fault"`, or `"symbol"`.
    pub observable: &'static str,
    /// The original's value, rendered for a human.
    pub expected: String,
    /// The candidate's value, rendered for a human.
    pub got: String,
}

/// Differential-test `symbol` in `original` against the same `symbol` in `candidate`.
#[must_use]
pub fn diff_calls(
    original: &mut Session,
    candidate: &mut Session,
    symbol: &str,
    corpus: &[Vec<Arg>],
) -> DiffResult {
    diff_calls_named(original, symbol, candidate, symbol, corpus)
}

/// Differential-test `orig_sym` in `original` against `cand_sym` in `candidate` over `corpus`.
#[must_use]
pub fn diff_calls_named(
    original: &mut Session,
    orig_sym: &str,
    candidate: &mut Session,
    cand_sym: &str,
    corpus: &[Vec<Arg>],
) -> DiffResult {
    let total = corpus.len();
    let mut matched = 0usize;
    let mut first_divergence: Option<Divergence> = None;

    let oaddr = original.resolve_symbol(orig_sym).map(|s| s.address);
    let caddr = candidate.resolve_symbol(cand_sym).map(|s| s.address);
    let (oa, ca) = match (oaddr, caddr) {
        (Ok(o), Ok(c)) => (o, c),
        _ => {
            // Neither any input can run; report one symbol-resolution divergence for the whole run.
            return DiffResult {
                total,
                matched: 0,
                first_divergence: Some(Divergence {
                    index: 0,
                    observable: "symbol",
                    expected: format!("{orig_sym} resolves in original"),
                    got: format!("{cand_sym} resolves in candidate"),
                }),
            };
        }
    };

    for (i, input) in corpus.iter().enumerate() {
        match (original.call_spec(oa, input), candidate.call_spec(ca, input)) {
            (Ok(o), Ok(c)) => match compare(&o, &c) {
                None => matched += 1,
                Some(obs) => {
                    if first_divergence.is_none() {
                        first_divergence = Some(divergence_for(i, obs, &o, &c));
                    }
                }
            },
            (Err(e), Ok(_)) => {
                if first_divergence.is_none() {
                    first_divergence = Some(Divergence {
                        index: i,
                        observable: "fault",
                        expected: format!("original faulted: {e}"),
                        got: "candidate returned".to_string(),
                    });
                }
            }
            (Ok(_), Err(e)) => {
                if first_divergence.is_none() {
                    first_divergence = Some(Divergence {
                        index: i,
                        observable: "fault",
                        expected: "original returned".to_string(),
                        got: format!("candidate faulted: {e}"),
                    });
                }
            }
            // Both faulted: same observable outcome (failure). Count as a match; neither produced a
            // value to compare, and the corpus should be shaped to avoid faulting inputs anyway.
            (Err(_), Err(_)) => matched += 1,
        }
    }

    DiffResult { total, matched, first_divergence }
}

/// The first observable that disagrees, or `None` if the two results match.
///
/// Compares `X0` and every output buffer. `X1` is deliberately *not* compared by default: for a
/// function returning a single word it holds leftover register state (often a derived pointer that
/// differs because the two sessions allocated buffers at different guest addresses), so comparing
/// it produces false divergences. A 128-bit return is handled by an explicit opt-in at the tool
/// layer, not here.
fn compare(o: &CallSpecResult, c: &CallSpecResult) -> Option<&'static str> {
    if o.ret != c.ret {
        return Some("ret");
    }
    if o.out_buffers != c.out_buffers {
        return Some("out_buffers");
    }
    None
}

fn divergence_for(index: usize, obs: &'static str, o: &CallSpecResult, c: &CallSpecResult) -> Divergence {
    let (expected, got) = match obs {
        "ret" => (format!("{:#x}", o.ret), format!("{:#x}", c.ret)),
        "ret1" => (format!("{:#x}", o.ret1), format!("{:#x}", c.ret1)),
        "out_buffers" => (format!("{:02x?}", o.out_buffers), format!("{:02x?}", c.out_buffers)),
        _ => (String::new(), String::new()),
    };
    Divergence { index, observable: obs, expected, got }
}

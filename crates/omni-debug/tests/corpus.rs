//! The input corpus (C4): deterministic generation of argument vectors from a template, so the
//! differential oracle runs original and candidate over the same inputs and a run is reproducible
//! from its seed.

use omni_debug::corpus::{generate, ArgKind, ArgTemplate};
use omni_debug::Arg;

#[test]
fn same_seed_same_corpus() {
    let t = [
        ArgTemplate { kind: ArgKind::Scalar },
        ArgTemplate { kind: ArgKind::Buffer { len: 8 } },
    ];
    let a = generate(&t, 42, 10);
    let b = generate(&t, 42, 10);
    assert_eq!(a, b, "same seed must reproduce the corpus exactly");
    assert_eq!(a.len(), 10);
}

#[test]
fn different_seed_differs() {
    let t = [ArgTemplate { kind: ArgKind::Scalar }];
    assert_ne!(generate(&t, 1, 20), generate(&t, 2, 20), "different seeds should differ");
}

#[test]
fn const_kind_emits_a_fixed_scalar() {
    // A constant parameter (a length or flag held fixed across the fuzz run) is emitted verbatim.
    let t = [ArgTemplate { kind: ArgKind::Const(5) }];
    let c = generate(&t, 9, 4);
    assert_eq!(c.len(), 4);
    assert!(c.iter().all(|v| v[0] == Arg::Scalar(5)), "every vector holds the constant: {c:?}");
}

#[test]
fn count_zero_yields_empty_corpus() {
    let t = [ArgTemplate { kind: ArgKind::Scalar }];
    assert!(generate(&t, 1, 0).is_empty());
}

#[test]
fn templates_shape_each_vector() {
    let t = [
        ArgTemplate { kind: ArgKind::Scalar },
        ArgTemplate { kind: ArgKind::Buffer { len: 4 } },
        ArgTemplate { kind: ArgKind::OutBuffer { len: 16 } },
    ];
    let corpus = generate(&t, 7, 5);
    for v in &corpus {
        assert_eq!(v.len(), 3, "each vector follows the template arity");
        assert!(matches!(v[0], Arg::Scalar(_)));
        assert!(matches!(&v[1], Arg::InBuffer(b) if b.len() == 4));
        assert!(matches!(v[2], Arg::OutBuffer(16)));
    }
}

#[test]
fn boundary_scalars_appear_first() {
    // The generator leads with boundary values so the oracle exercises 0, 1, u64::MAX early.
    let t = [ArgTemplate { kind: ArgKind::Scalar }];
    let corpus = generate(&t, 123, 5);
    let first_three: Vec<u64> = corpus.iter().take(3).map(|v| match v[0] {
        Arg::Scalar(x) => x,
        _ => unreachable!(),
    }).collect();
    assert_eq!(first_three, vec![0, 1, u64::MAX]);
}

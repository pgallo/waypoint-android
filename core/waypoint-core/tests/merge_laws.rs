//! Property tests: the replicated state is a join-semilattice.
//!
//! Generators deliberately use a tiny space of entities, fields, elements and
//! nodes so that random ops collide constantly; sparse ops would make the
//! laws trivially true.

use proptest::prelude::*;
use waypoint_core::{EntityRef, Hlc, Op, OpKind, ReplicaState};

fn arb_hlc() -> impl Strategy<Value = Hlc> {
    (0i64..6, 0u32..3, prop::sample::select(vec!["a", "b", "c"]))
        .prop_map(|(w, c, n)| Hlc::new(w, c, n))
}

fn arb_entity() -> impl Strategy<Value = EntityRef> {
    prop::sample::select(vec![
        ("site", "s1"),
        ("work_order", "w1"),
        ("work_order", "w2"),
    ])
    .prop_map(|(k, i)| EntityRef::new(k, i))
}

fn arb_kind() -> impl Strategy<Value = OpKind> {
    let name = prop::sample::select(vec!["title", "status"]).prop_map(str::to_string);
    let value = prop::sample::select(vec!["x", "y", "z"]).prop_map(str::to_string);
    let set = prop::sample::select(vec!["tags", "notes"]).prop_map(str::to_string);
    let elem = prop::sample::select(vec!["e1", "e2"]).prop_map(str::to_string);
    prop_oneof![
        (name, value).prop_map(|(field, value)| OpKind::Set { field, value }),
        (set.clone(), elem.clone()).prop_map(|(set, element)| OpKind::Add { set, element }),
        (set, elem, prop::collection::vec(arb_hlc(), 0..3)).prop_map(|(set, element, observed)| {
            OpKind::Remove {
                set,
                element,
                observed,
            }
        }),
    ]
}

fn arb_op() -> impl Strategy<Value = Op> {
    (arb_hlc(), arb_entity(), arb_kind()).prop_map(|(id, entity, kind)| Op { id, entity, kind })
}

fn arb_ops() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(arb_op(), 0..24)
}

fn state_of(ops: &[Op]) -> ReplicaState {
    let mut s = ReplicaState::new();
    for op in ops {
        s.apply(op);
    }
    s
}

fn joined(a: &ReplicaState, b: &ReplicaState) -> ReplicaState {
    let mut r = a.clone();
    r.merge(b);
    r
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn merge_is_commutative(x in arb_ops(), y in arb_ops()) {
        let (a, b) = (state_of(&x), state_of(&y));
        prop_assert_eq!(joined(&a, &b), joined(&b, &a));
    }

    #[test]
    fn merge_is_associative(x in arb_ops(), y in arb_ops(), z in arb_ops()) {
        let (a, b, c) = (state_of(&x), state_of(&y), state_of(&z));
        prop_assert_eq!(joined(&joined(&a, &b), &c), joined(&a, &joined(&b, &c)));
    }

    #[test]
    fn merge_is_idempotent(x in arb_ops(), y in arb_ops()) {
        let a = state_of(&x);
        prop_assert_eq!(joined(&a, &a), a.clone());
        let ab = joined(&a, &state_of(&y));
        prop_assert_eq!(joined(&ab, &state_of(&y)), ab);
    }

    /// Op-based delivery: any permutation, with duplicates, gives one state.
    #[test]
    fn apply_order_and_duplicates_do_not_matter(
        ops in arb_ops(),
        perm in any::<u64>(),
        dup_mask in prop::collection::vec(any::<bool>(), 24),
    ) {
        let reference = state_of(&ops);
        let mut shuffled = ops.clone();
        // Deterministic Fisher-Yates driven by the generated seed.
        let mut seed = perm | 1;
        for i in (1..shuffled.len()).rev() {
            seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
            shuffled.swap(i, (seed % (i as u64 + 1)) as usize);
        }
        let mut with_dups = Vec::new();
        for (i, op) in shuffled.iter().enumerate() {
            with_dups.push(op.clone());
            if dup_mask[i % dup_mask.len()] {
                with_dups.push(op.clone());
            }
        }
        prop_assert_eq!(state_of(&with_dups), reference);
    }

    /// State-based and op-based replication agree: merging the states of two
    /// op logs equals applying the union of the logs.
    #[test]
    fn state_merge_matches_log_union(x in arb_ops(), y in arb_ops()) {
        let mut union = x.clone();
        union.extend(y.iter().cloned());
        prop_assert_eq!(joined(&state_of(&x), &state_of(&y)), state_of(&union));
    }

    /// Merge is inflationary: nothing visible in an input disappears from
    /// the join unless a remove in the other input covers it.
    #[test]
    fn merge_never_loses_register_winners(x in arb_ops(), y in arb_ops()) {
        let (a, b) = (state_of(&x), state_of(&y));
        let j = joined(&a, &b);
        for e in ["s1", "w1", "w2"] {
            for kind in ["site", "work_order"] {
                let ent = EntityRef::new(kind, e);
                for f in ["title", "status"] {
                    let wa = a.get_stamped(&ent, f).map(|(h, _)| h.clone());
                    let wj = j.get_stamped(&ent, f).map(|(h, _)| h.clone());
                    prop_assert!(wj >= wa);
                }
            }
        }
    }
}

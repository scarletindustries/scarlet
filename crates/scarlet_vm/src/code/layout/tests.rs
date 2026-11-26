use super::super::ReadsClosure;
use super::super::alloc::{allocate, room};
use super::super::ins::{Lists, VReg};
use super::super::live::settle;
use super::super::moves::expand;
use super::super::tests::programs::every_draft;
use super::super::tests::random::{Random, random_function};
use super::super::tests::{of, targets};
use super::super::translate::Draft;
use super::*;
use crate::stop::Limit;

/// The body of `ins`, with a register for no value.
fn body_of(ins: Vec<Ins<VReg>>, labels: &[usize]) -> Body {
    Body {
        ins,
        lists: Lists::default(),
        labels: labels.to_vec(),
        regs: Vec::new(),
        kinds: Vec::new(),
        params: Vec::new(),
        own: room(2),
        closure: ReadsClosure::Nothing,
    }
}

fn misses(of: &[(u32, u32)]) -> Vec<Miss> {
    let miss = |(from, to): &(u32, u32)| Miss {
        from: Label(*from),
        to: Label(*to),
    };
    of.iter().map(miss).collect()
}

/// Each instruction by its kind, what it writes first or reads when it writes nothing,
/// and where it goes.
fn shown(body: &Body) -> Vec<String> {
    let show = |ins: &Ins<VReg>| {
        let mut named = Vec::new();
        ins.operands(&body.lists, &mut |v, _| named.push(v.0.to_string()));
        let to = targets(ins, &body.lists);
        let to = to
            .iter()
            .map(|l| format!("-> {}", body.labels[l.0 as usize]));
        let all = [ins.op().name().to_string()];
        let parts = all.into_iter().chain(named.into_iter().take(1)).chain(to);
        parts.collect::<Vec<String>>().join(" ")
    };
    body.ins.iter().map(show).collect()
}

fn laid_out(ins: Vec<Ins<VReg>>, labels: &[usize], of: &[(u32, u32)]) -> Vec<String> {
    let body = lay_out(body_of(ins, labels), &misses(of));
    shown(&body.expect("it is laid out"))
}

/// A miss goes behind what comes after both, and jumps back to it. The hit runs on.
#[test]
fn a_miss_is_out_of_the_hits_way() {
    let ins = vec![
        of::get(Some(1), [0, 0], 0),
        of::len(1, 1),
        of::jump(1),
        of::constant(1),
        of::ret(1),
    ];
    assert_eq!(
        laid_out(ins, &[3, 4], &[(0, 1)]),
        [
            "ArrayGet 1 -> 3",
            "ArrayLen 1",
            "Ret 1",
            "Const 1",
            "Jump -> 2"
        ]
    );
    // One that leaves the function, or goes elsewhere, has no jump back.
    let leaves = vec![
        of::get(Some(1), [0, 0], 0),
        of::jump(1),
        of::ret(0),
        of::ret(1),
    ];
    assert_eq!(
        laid_out(leaves, &[2, 3], &[(0, 1)]),
        ["ArrayGet 1 -> 2", "Ret 1", "Ret 0"]
    );
}

/// The misses of probes in one another's hits are behind each other, and all move.
#[test]
fn the_misses_behind_a_hit_move_together() {
    let ins = vec![
        of::get(Some(1), [0, 0], 0),
        of::get(Some(1), [1, 1], 1),
        of::len(1, 1),
        of::jump(2),
        // The inner miss, and then the outer.
        of::constant(1),
        of::jump(2),
        of::push(1, 0, 0),
        of::ret(1),
    ];
    assert_eq!(
        laid_out(ins, &[6, 4, 7], &[(1, 0), (0, 2)]),
        [
            "ArrayGet 1 -> 6",
            "ArrayGet 1 -> 4",
            "ArrayLen 1",
            "Ret 1",
            "Const 1",
            "Jump -> 3",
            "ArrayPush 1",
            "Jump -> 3",
        ]
    );
}

/// A miss with nothing in it is none of those.
#[test]
fn a_miss_with_nothing_in_it_is_none() {
    let ins = || {
        vec![
            of::get(Some(1), [0, 0], 0),
            of::get(Some(1), [1, 1], 1),
            of::jump(2),
            of::constant(1),
            of::ret(1),
        ]
    };
    // The inner miss goes on at once with the outer one's, which is the constant.
    for of in [[(1, 0), (0, 2)], [(0, 2), (1, 0)]] {
        assert_eq!(
            laid_out(ins(), &[3, 3, 4], &of),
            [
                "ArrayGet 1 -> 3",
                "ArrayGet 1 -> 3",
                "Ret 1",
                "Const 1",
                "Jump -> 2"
            ]
        );
    }
    // And a jump over nothing is not there.
    let nothing = vec![
        of::get(Some(1), [0, 0], 0),
        of::jump(0),
        of::ret(1),
        of::unless(1, 0),
        of::ret(0),
    ];
    assert_eq!(
        laid_out(nothing, &[2], &[(0, 0)])[..2],
        ["ArrayGet 1 -> 1", "Ret 1"]
    );
}

/// A probe in a miss is laid out like any other, where the miss went.
#[test]
fn a_probe_in_a_miss_is_laid_out_there() {
    let ins = vec![
        of::get(Some(1), [0, 0], 0),
        of::jump(3),
        // The miss, which has a probe in it, whose miss is the second constant.
        of::get(Some(1), [0, 0], 1),
        of::jump(2),
        of::constant(1),
        of::len(1, 1),
        of::ret(1),
    ];
    assert_eq!(
        laid_out(ins, &[2, 4, 5, 6], &[(0, 3), (1, 2)]),
        [
            "ArrayGet 1 -> 2",
            "Ret 1",
            "ArrayGet 1 -> 5",
            "ArrayLen 1",
            "Jump -> 1",
            "Const 1",
            "Jump -> 3",
        ]
    );
    // Where its hit goes on with what is behind the whole miss, it has to jump there
    // whatever is moved, and nothing is.
    let last = vec![
        of::get(Some(1), [0, 0], 0),
        of::jump(1),
        of::get(Some(1), [0, 0], 2),
        of::jump(1),
        of::constant(1),
        of::ret(1),
    ];
    assert_eq!(
        laid_out(last, &[2, 5, 4], &[(0, 1), (2, 1)]),
        [
            "ArrayGet 1 -> 2",
            "Ret 1",
            "ArrayGet 1 -> 4",
            "Jump -> 1",
            "Const 1",
            "Jump -> 1",
        ]
    );
}

/// What is no miss stays where it is: the other side of an `if`, and a hit's that does
/// not go straight on behind the miss.
#[test]
fn only_a_miss_that_a_hit_jumps_over_moves() {
    let branch = vec![
        of::unless(0, 0),
        of::len(1, 0),
        of::jump(1),
        of::constant(1),
        of::ret(1),
    ];
    let as_it_was = [
        "JumpIfFalse 0 -> 3",
        "ArrayLen 1",
        "Jump -> 4",
        "Const 1",
        "Ret 1",
    ];
    assert_eq!(laid_out(branch, &[3, 4], &[]), as_it_was);
    // The other side of an `if` is between the miss and where they come together.
    let between = vec![
        of::unless(0, 0),
        of::get(Some(1), [0, 0], 1),
        of::jump(2),
        of::constant(1),
        of::jump(2),
        of::len(1, 0),
        of::ret(1),
    ];
    let was = shown(&body_of(between.clone(), &[5, 3, 6]));
    assert_eq!(laid_out(between, &[5, 3, 6], &[(1, 0)]), was);
    // A jump into a miss, which no function has, is not over one.
    let into = vec![
        of::get(Some(1), [0, 0], 0),
        of::jump(1),
        of::constant(1),
        of::len(1, 0),
        of::ret(1),
    ];
    let was = shown(&body_of(into.clone(), &[2, 3, 4]));
    assert_eq!(laid_out(into, &[2, 3, 4], &[(0, 2)]), was);
    // A hit that leaves the function has no jump to take away.
    let leaves = vec![of::get(Some(1), [0, 0], 0), of::ret(1), of::ret(0)];
    let was = shown(&body_of(leaves.clone(), &[2]));
    assert_eq!(laid_out(leaves, &[2], &[(0, 0)]), was);
}

/// A jump to a jump goes where that goes, at any distance and back to the start too,
/// and a jump that nothing then comes to is not there.
#[test]
fn a_jump_to_a_jump_goes_on() {
    let ins = vec![
        of::len(1, 0),
        of::unless(0, 1),
        of::unless(1, 2),
        of::ret(0),
        of::jump(2),
        of::unless(1, 3),
        of::ret(1),
        of::jump(0),
        of::ret(0),
    ];
    assert_eq!(
        laid_out(ins, &[0, 4, 7, 8], &[]),
        [
            "ArrayLen 1",
            "JumpIfFalse 0 -> 0",
            "JumpIfFalse 1 -> 0",
            "Ret 0",
            "JumpIfFalse 1 -> 6",
            "Ret 1",
            "Ret 0",
        ]
    );
    // A miss that is nothing but a jump elsewhere is the probe's own jump.
    let elsewhere = vec![
        of::get(Some(1), [0, 0], 0),
        of::len(1, 1),
        of::jump(1),
        of::jump(2),
        of::ret(1),
        of::ret(0),
    ];
    assert_eq!(
        laid_out(elsewhere, &[3, 4, 5], &[(0, 1)]),
        ["ArrayGet 1 -> 3", "ArrayLen 1", "Ret 1", "Ret 0"]
    );
    // A function that is a loop and nothing else.
    assert_eq!(laid_out(vec![of::jump(0)], &[0], &[]), ["Jump -> 0"]);
    // What falls through to a jump still needs it.
    let both = vec![
        of::unless(0, 0),
        of::len(1, 0),
        of::jump(1),
        of::len(2, 0),
        of::ret(0),
    ];
    let was = shown(&body_of(both.clone(), &[2, 4]));
    assert_eq!(was[0], "JumpIfFalse 0 -> 2");
    let got = laid_out(both, &[2, 4], &[]);
    assert_eq!(got[..3], ["JumpIfFalse 0 -> 4", "ArrayLen 1", "Jump -> 4"]);
}

/// A body whose labels stand nowhere, or that runs off its end, is refused.
#[test]
fn a_body_that_cannot_be_is_refused() {
    let refused = |ins, labels: &[usize], of: &[(u32, u32)]| {
        let body = lay_out(body_of(ins, labels), &misses(of));
        body.map(|body| shown(&body))
    };
    let no_place = Err(Unfit::Broken(Broken::NoPlace));
    assert_eq!(refused(vec![of::jump(7)], &[0], &[]), no_place);
    assert_eq!(refused(vec![of::ret(0)], &[0, 3], &[(0, 0)]), no_place);
    assert_eq!(refused(vec![of::ret(0)], &[0], &[(0, 4)]), no_place);
    let runs_off = Err(Unfit::Broken(Broken::RunsOff));
    assert_eq!(refused(vec![of::len(1, 0)], &[0], &[(0, 0)]), runs_off);
}

/// `d` as far as a body that is not laid out, when it fits.
fn expanded(mut d: Draft) -> Option<(Body, Vec<Miss>)> {
    let misses = std::mem::take(&mut d.misses);
    let body = settle(&mut d).and_then(|held| expand(allocate(d, &held)?));
    match body {
        Ok(body) => Some((body, misses)),
        Err(Unfit::TooMany(Limit::Values)) => None,
        Err(why) => panic!("{why:?}"),
    }
}

fn copy_of(body: &Body) -> Body {
    Body {
        ins: body.ins.clone(),
        lists: Lists {
            regs: body.lists.regs.clone(),
            args: body.lists.args.clone(),
            dsts: body.lists.dsts.clone(),
            sets: body.lists.sets.clone(),
            tables: body.lists.tables.clone(),
            places: body.lists.places.clone(),
            splits: body.lists.splits.clone(),
        },
        labels: body.labels.clone(),
        regs: body.regs.clone(),
        kinds: body.kinds.clone(),
        params: body.params.clone(),
        own: body.own,
        closure: body.closure,
    }
}

/// How many instructions a way is followed for.
const FAR: usize = 150;

/// What runs on the way that `seed` picks, without its jumps, and how many jumps. Where
/// it may not miss, every probe finds what it asks for.
fn way(body: &Body, misses: &[Miss], seed: u64, miss: bool) -> (Vec<String>, usize) {
    let mut random = Random(seed | 1);
    let (mut ran, mut jumps, mut at) = (Vec::new(), 0, 0);
    // A function may be a loop of jumps and nothing else.
    while ran.len() < FAR && jumps < 10 * FAR {
        let ins = &body.ins[at];
        let to = targets(ins, &body.lists);
        let place = |l: &Label| body.labels[l.0 as usize];
        if let Ins::Jump { to } = ins {
            jumps += 1;
            at = place(to);
            continue;
        }
        ran.push(format!("{ins:?}"));
        if to.is_empty() && !ins.falls_through() {
            break;
        }
        let stays = usize::from(ins.falls_through());
        let hits = !miss && misses.iter().any(|m| to.contains(&m.from));
        let pick = match hits {
            true => 0,
            false => random.below(to.len() + stays),
        };
        at = match pick.checked_sub(stays) {
            Some(i) => place(&to[i]),
            None => at + 1,
        };
    }
    (ran, jumps)
}

/// Hold `body` to what it is laid out as, and give how many jumps that took away from
/// the ways that find everything.
fn check(body: &Body, misses: &[Miss], what: &str, ways: u64) -> usize {
    let after = lay_out(copy_of(body), misses).expect("it is laid out");
    let jump = |ins: &&Ins<VReg>| matches!(ins, Ins::Jump { .. });
    let jumps = |body: &Body| body.ins.iter().filter(jump).count();
    let show = || format!("{what}\n{:#?}\n{:#?}", shown(body), shown(&after));
    // Every other instruction is there once, and no more jumps than there were.
    assert!(jumps(&after) <= jumps(body), "{}", show());
    let others = |body: &Body| body.ins.len() - jumps(body);
    assert_eq!(others(&after), others(body), "{}", show());
    let mut gone = 0;
    for seed in 0..ways {
        for miss in [false, true] {
            let seed = (seed + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let (was, before) = way(body, misses, seed, miss);
            let (is, now) = way(&after, misses, seed, miss);
            assert_eq!(was, is, "{}", show());
            if !miss {
                assert!(now <= before, "{now} {before} {}", show());
                gone += before - now;
            }
        }
    }
    gone
}

/// What runs on each way through a function is what ran, and no way that finds
/// everything runs a jump more. Both of functions made at random and of every function
/// in the repository.
#[test]
fn each_way_runs_what_it_ran() {
    let (mut moved, mut functions) = (0, 0);
    for seed in 0..1500 {
        let steps = 1 + usize::try_from(seed % 40).expect("few");
        let Some((body, misses)) = expanded(random_function(seed, 3, steps)) else {
            continue;
        };
        functions += 1;
        moved += check(&body, &misses, &format!("seed {seed}"), 6);
    }
    assert!(functions > 1000 && moved > 1000, "{functions} {moved}");
    let mut moved = 0;
    every_draft(&mut |d, what| {
        if let Some((body, misses)) = expanded(d) {
            moved += check(&body, &misses, what, 3);
        }
        true
    });
    assert!(moved > 100, "{moved}");
}

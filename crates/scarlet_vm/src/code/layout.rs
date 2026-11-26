//! The order of a function's instructions.
//!
//! [`translate`](super::translate) puts a probe's miss between its hit and what both
//! go on with, so the hit ends in a jump over the miss. Here the miss is moved behind
//! the rest of the function, with a jump back to what it goes on with. The hit then
//! runs on without one, and a jump is run on a miss alone. A miss is moved only where
//! that takes the hit's jump away.
//!
//! A jump to a jump goes to where the second does, and a jump that is then never run
//! is left out.
//!
//! This is after everything that counts on every jump going forward.

use std::collections::VecDeque;

use super::Unfit;
use super::ins::{Ins, Label};
use super::moves::Body;
use super::translate::Miss;
use crate::stop::Broken;

/// Put the instructions of `body` in order. `misses` are its probes'.
pub(super) fn lay_out(mut body: Body, misses: &[Miss]) -> Result<Body, Unfit> {
    let end = body.ins.len();
    let jump = |at: &usize| matches!(body.ins.get(*at), Some(Ins::Jump { .. }));
    if misses.is_empty() && !body.labels.iter().any(jump) {
        return Ok(body);
    }
    let place = |Label(l): Label| {
        let at = body.labels.get(l as usize).copied();
        at.ok_or(Unfit::Broken(Broken::NoPlace))
    };
    // The end of the miss that starts at each place.
    let mut misses_to: Vec<Option<usize>> = vec![None; end];
    for miss in misses {
        let (from, to) = (place(miss.from)?, place(miss.to)?);
        if from < to
            && let Some(slot) = misses_to.get_mut(from)
        {
            *slot = Some(to);
        }
    }
    let ends_at = threaded(&body);
    let mut out: Vec<Ins<_>> = Vec::with_capacity(end);
    // Where each instruction is now, or the next that stays for one that went.
    let mut moved_to = vec![0usize; end];
    // The instructions still to be put somewhere, and where those that are moved go on.
    let mut left = VecDeque::from([(0, end, None)]);
    while let Some((mut at, until, then)) = left.pop_front() {
        // Whether what is in front runs on to here.
        let mut goes_on = then.is_none();
        while at < until {
            let ins = body.ins.get(at).ok_or(Broken::NoPlace)?;
            if let Some(to) = moved_to.get_mut(at) {
                *to = out.len();
            }
            let next = at.saturating_add(1);
            if let Ins::Jump { to } = ins {
                // What jumps to it goes on to where it goes.
                if !goes_on {
                    at = next;
                    continue;
                }
                let over = place(*to)?;
                if (next..until).contains(&over) && only_misses(&misses_to, next, over) {
                    left.push_back((next, over, Some(*to)));
                    at = over;
                    continue;
                }
            }
            goes_on = ins.falls_through();
            out.push(*ins);
            at = next;
        }
        match then {
            Some(to) if goes_on => out.push(Ins::Jump { to }),
            None if goes_on => return Err(Broken::RunsOff.into()),
            Some(_) | None => {}
        }
    }
    for at in &mut body.labels {
        let place = ends_at.get(*at).and_then(|to| moved_to.get(*to));
        *at = place.copied().ok_or(Broken::NoPlace)?;
    }
    body.ins = out;
    Ok(body)
}

/// Whether what is from `at` to `over` is misses, one behind the other.
fn only_misses(misses_to: &[Option<usize>], mut at: usize, over: usize) -> bool {
    while at < over {
        match misses_to.get(at) {
            Some(Some(to)) => at = *to,
            Some(None) | None => return false,
        }
    }
    at == over
}

/// Where a jump to each place ends, when jumps go on from jumps.
fn threaded(body: &Body) -> Vec<usize> {
    let mut ends: Vec<usize> = (0..body.ins.len()).collect();
    for (at, ins) in body.ins.iter().enumerate().rev() {
        if let Ins::Jump { to } = ins
            && let Some(to) = body.labels.get(to.0 as usize)
            && let Some(end) = match *to > at {
                true => ends.get(*to).copied(),
                false => Some(*to),
            }
            && let Some(of) = ends.get_mut(at)
        {
            *of = end;
        }
    }
    ends
}

#[cfg(test)]
mod tests;

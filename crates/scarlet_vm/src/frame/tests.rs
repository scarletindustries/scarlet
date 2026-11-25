use super::*;
use crate::heap::Cell;

fn nil_stack(len: usize) -> Vec<Value> {
    vec![Value::NIL; len]
}

fn int(n: i64) -> Value {
    Value::int(n).expect("small")
}

fn tuple(heap: &mut Heap) -> Cell {
    heap.tuple(&[Value::NIL]).expect("room")
}

/// A link gives back each of the three things it was made of, whatever the
/// other two are, and its word is never a cell's.
#[test]
fn a_link_holds_three_things_and_is_no_cell() {
    let ends = [0, 1, 255, u32::MAX as usize - 1, u32::MAX as usize];
    for pc in ends {
        for own in [0u16, 2, 3, 255, 256] {
            for dst in [0u8, 2, 127, 128, 255] {
                let link = Link::new(pc, Room(own), Dst(Reg(dst)));
                assert_eq!(link.pc(), pc);
                assert_eq!(link.below(), u32::from(own));
                assert_eq!(link.dst().0, Reg(dst));
                let word = Value::from_bits(link.0);
                assert!(word.as_cell().is_none(), "{word:?}");
                assert!(matches!(word.view(), crate::value::View::Float(_)));
            }
        }
    }
}

/// A window is there when all 256 of its registers are, and not one before.
#[test]
fn a_window_is_whole_or_not_there() {
    let mut stack = nil_stack(WINDOW + 3);
    for base in 0..=3 {
        assert!(window(&mut stack, base).is_some(), "{base}");
    }
    for base in [4, 255, 259, 260, u32::MAX - 255, u32::MAX] {
        assert!(window(&mut stack, base).is_none(), "{base}");
    }
    assert!(window(&mut nil_stack(WINDOW - 1), 0).is_none());
}

/// A register's number finds the same place whichever window it is named in,
/// and the last register there is.
#[test]
fn a_register_is_its_number_above_the_windows_start() {
    let mut stack = nil_stack(WINDOW + 10);
    let heap = &mut Heap::default();
    let mut win = window(&mut stack, 10).expect("a window");
    for n in [0u8, 1, 2, 128, 255] {
        let set = win.set(heap, Reg(n), int(i64::from(n) + 1000));
        assert_eq!(set, Ok(()));
        assert_eq!(win.get(Reg(n)).small_int(), Some(i64::from(n) + 1000));
    }
    assert_eq!(stack[10 + 255].small_int(), Some(1255));
    assert_eq!(stack[10].small_int(), Some(1000));
    assert!(stack[..10].iter().all(|v| v.bits() == Value::NIL.bits()));
}

/// A write gives up what the register held, a take empties it, and an argument
/// that is not taken is left with one more holder.
#[test]
fn each_way_to_a_register_counts_what_it_does() {
    let mut stack = nil_stack(WINDOW);
    let heap = &mut Heap::default();
    let mut win = window(&mut stack, 0).expect("a window");
    let (a, b) = (tuple(heap), tuple(heap));
    assert_eq!(win.put(heap, Dst(Reg(2)), Value::cell(a)), Ok(()));
    let arg = |moved| Arg { reg: Reg(2), moved };
    let shared = win.pass(heap, arg(false)).map(Value::as_cell);
    assert_eq!((shared, heap.unique(a)), (Ok(Some(a)), Ok(false)));
    assert_eq!(heap.release(a), Ok(()));
    let shared = win.share(heap, Shared(Reg(2))).map(Value::as_cell);
    assert_eq!((shared, heap.unique(a)), (Ok(Some(a)), Ok(false)));
    assert_eq!(heap.release(a), Ok(()));
    let moved = win.pass(heap, arg(true)).map(Value::as_cell);
    assert_eq!((moved, heap.unique(a)), (Ok(Some(a)), Ok(true)));
    assert!(win.look(Src(Reg(2))).as_cell().is_none());
    assert_eq!(win.hit(heap, Hit(Reg(2)), Value::cell(a)), Ok(()));
    assert_eq!(win.put(heap, Dst(Reg(2)), Value::cell(b)), Ok(()));
    assert_eq!(heap.live(), 1, "the write gave the first cell up");
    assert_eq!(win.take(Taken(Reg(2))).as_cell(), Some(b));
    assert!(win.get(Reg(2)).as_cell().is_none());
    assert_eq!(win.set(heap, Reg(2), Value::cell(b)), Ok(()));
    assert_eq!(win.drop(heap, Dead(Reg(2))), Ok(()));
    assert_eq!(heap.live(), 0);
}

/// Handing a value on in two parts does what passing it does: the reference is
/// there before the value has left, and the register is emptied only when its
/// own reference goes. What was added can be taken back, and then nothing has
/// changed.
#[test]
fn a_value_is_handed_on_in_two_parts() {
    let mut stack = nil_stack(WINDOW);
    let heap = &mut Heap::default();
    let mut win = window(&mut stack, 0).expect("a window");
    let (a, plain) = (tuple(heap), int(4));
    assert_eq!(win.put(heap, Dst(Reg(2)), Value::cell(a)), Ok(()));
    assert_eq!(win.put(heap, Dst(Reg(3)), plain), Ok(()));
    let arg = |n, moved| Arg { reg: Reg(n), moved };
    let unchanged = |win: &Window<'_>, heap: &Heap| {
        assert_eq!(heap.unique(a), Ok(true));
        assert_eq!(win.get(Reg(2)).as_cell(), Some(a));
        assert_eq!(win.get(Reg(3)).bits(), plain.bits());
    };
    for n in [2, 3] {
        for moved in [false, true] {
            assert_eq!(win.add(heap, arg(n, moved)), Ok(()));
            let added = n == 2 && !moved;
            assert_eq!(heap.unique(a), Ok(!added));
            assert_eq!(win.take_back(heap, arg(n, moved)), Ok(()));
            unchanged(&win, heap);
        }
        assert_eq!(win.add(heap, arg(n, false)), Ok(()));
        let v = win.hand(arg(n, false));
        assert_eq!(v.bits(), win.get(Reg(n)).bits(), "it is still there");
        assert_eq!(give_up(heap, v), Ok(()));
        unchanged(&win, heap);
    }
    assert_eq!(win.hand(arg(3, true)).bits(), plain.bits());
    assert_eq!(win.hand(arg(2, true)).as_cell(), Some(a));
    for n in [2, 3] {
        assert_eq!(win.get(Reg(n)).bits(), Value::NIL.bits());
    }
    assert_eq!(heap.unique(a), Ok(true));
    // A freed cell has no reference to add or to take back.
    assert_eq!(win.put(heap, Dst(Reg(2)), Value::cell(a)), Ok(()));
    assert_eq!(heap.release(a), Ok(()));
    assert_eq!(win.add(heap, arg(2, false)), Err(DeadCell));
    assert_eq!(win.take_back(heap, arg(2, false)), Err(DeadCell));
    assert_eq!(win.add(heap, arg(2, true)), Ok(()));
}

/// What `leave` and `rewrite` hold here: a cell in every register from 1 up,
/// for a frame of `own` registers.
fn filled(stack: &mut [Value], heap: &mut Heap, own: u16) -> Vec<Cell> {
    let cells: Vec<Cell> = (1..own).map(|_| tuple(heap)).collect();
    let mut win = window(stack, 0).expect("a window");
    win.start(Link::new(7, Room(9), Dst(Reg(4))));
    win.run_as(Value::cell(cells[0]));
    let rest: Vec<Value> = cells[1..].iter().copied().map(Value::cell).collect();
    win.receive(&rest);
    cells
}

/// A frame that ends gives up everything from its first parameter to its last
/// register, from the top down, and leaves its link and its closure. What is
/// not a cell may stay where it was.
#[test]
fn a_frame_that_ends_leaves_no_cell_but_its_closure() {
    for own in [3u16, 4, 100, 255, 256] {
        let mut stack = nil_stack(WINDOW + 1);
        let heap = &mut Heap::default();
        let cells = filled(&mut stack, heap, own);
        let mut win = window(&mut stack, 0).expect("a window");
        assert_eq!(win.leave(heap, Room(own).last()), Ok(()));
        assert_eq!(win.link().pc(), 7);
        assert_eq!(win.closure().as_cell(), Some(cells[0]));
        assert_eq!(heap.live(), 1, "{own}");
        assert_eq!(win.drop(heap, Dead(CLOSURE)), Ok(()));
        assert_eq!(heap.live(), 0);
        // Freed from the top down, so the one freed last is made again first.
        let again = tuple(heap);
        assert_eq!(again, cells[0]);
        assert_eq!(tuple(heap), cells[1], "{own}");
    }
    // A frame of no values has nothing to give up.
    let mut stack = nil_stack(WINDOW);
    let heap = &mut Heap::default();
    let mut win = window(&mut stack, 0).expect("a window");
    assert_eq!(win.leave(heap, Room(2).last()), Ok(()));
}

/// A frame that a tail call takes over holds the values passed and no other
/// cell, whether it passes fewer than it had registers, as many, or more.
#[test]
fn a_frame_taken_over_holds_what_was_passed() {
    for own in [3u16, 6, 256] {
        for passed in [0usize, 1, 4, 8, 254] {
            let mut stack = nil_stack(WINDOW);
            let heap = &mut Heap::default();
            let cells = filled(&mut stack, heap, own);
            let values: Vec<Value> = (0..passed).map(|_| Value::cell(tuple(heap))).collect();
            let mut win = window(&mut stack, 0).expect("a window");
            assert_eq!(win.rewrite(heap, Room(own), &values), Ok(()));
            assert_eq!(win.link().pc(), 7);
            assert_eq!(win.closure().as_cell(), Some(cells[0]));
            assert_eq!(heap.live(), 1 + passed, "{own} {passed}");
            let regs = win.regs();
            for (i, v) in values.iter().enumerate() {
                assert_eq!(regs[i + 2].bits(), v.bits());
            }
            assert!(regs[2 + passed..].iter().all(|v| v.as_cell().is_none()));
        }
    }
}

/// A register swapped gives what it held with its reference, and has the new
/// value with its own.
#[test]
fn a_swapped_register_gives_what_it_held() {
    let mut stack = nil_stack(WINDOW);
    let heap = &mut Heap::default();
    let cells = filled(&mut stack, heap, 8);
    let mut win = window(&mut stack, 0).expect("a window");
    let new = Value::cell(tuple(heap));
    let old = win.swap(Dst(Reg(4)), new);
    assert_eq!(old.as_cell(), Some(cells[3]));
    assert_eq!(win.get(Reg(4)).bits(), new.bits());
    assert_eq!(heap.live(), cells.len() + 1);
}

/// The frames are counted down the links to the one nothing called. There is no
/// count past a link that leads to no frame, whether it leads past the stack's
/// start, nowhere, or on from the frame at the bottom.
#[test]
fn the_depth_is_how_many_links_there_are_to_the_bottom() {
    let mut stack = nil_stack(WINDOW * 3);
    let frames = [(0, 0u16), (4, 4), (4 + 256, 256), (260 + 3, 3)];
    for (i, (base, below)) in frames.into_iter().enumerate() {
        let mut win = window(&mut stack, base).expect("a window");
        win.start(Link::new(1, Room(below), Dst(Reg(2))));
        assert_eq!(depth(&stack, base), Some(i + 1));
    }
    for (base, below) in [(4, 9), (4, 0), (4, 256), (0, 4)] {
        let was = stack[base as usize];
        let mut win = window(&mut stack, base).expect("a window");
        win.start(Link::new(1, Room(below), Dst(Reg(2))));
        assert_eq!(depth(&stack, 263), None, "{base} {below}");
        stack[base as usize] = was;
    }
    assert_eq!(depth(&stack, 263), Some(4));
    for base in [768, u32::MAX] {
        assert_eq!(depth(&stack, base), None, "{base}");
    }
    assert_eq!(depth(&[], 0), None);
}

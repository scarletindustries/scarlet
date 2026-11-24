use super::*;

fn owner(heap: &mut Heap, bytes: &[u8], len: u64) -> Bits {
    let cell = make(heap, bytes, len).expect("room");
    bits(heap, cell).expect("a binary")
}

#[test]
fn a_binary_shows_its_bytes_and_a_partial_last_one() {
    let mut heap = Heap::default();
    let b = owner(&mut heap, &[1, 2, 3], 24);
    assert_eq!(text(&heap, b), "<<1, 2, 3>>");
    let b = owner(&mut heap, &[0xFF, 0b1011_1111], 11);
    assert_eq!(text(&heap, b), "<<255, 5:size(3)>>");
    let b = owner(&mut heap, &[], 0);
    assert_eq!(text(&heap, b), "<<>>");
}

#[test]
fn from_int_writes_the_low_bits_most_significant_first() {
    assert_eq!(from_int(&1.into(), 4), vec![0b0001_0000]);
    assert_eq!(from_int(&65.into(), 8), vec![65]);
    assert_eq!(from_int(&0x1234.into(), 16), vec![0x12, 0x34]);
    assert_eq!(from_int(&0x1FF.into(), 8), vec![0xFF]);
    assert_eq!(from_int(&(-1).into(), 12), vec![0xFF, 0xF0]);
    assert_eq!(from_int(&7.into(), 0), Vec::<u8>::new());
    let big: BigInt = BigInt::from(1) << 100;
    let wide = from_int(&big, 104);
    assert_eq!(wide.len(), 13);
    assert_eq!(wide[0], 0x10);
}

/// A slice of a slice reads the right bits of the owner, at any offset.
#[test]
fn slices_read_through_to_the_owner() {
    let mut heap = Heap::default();
    let b = owner(&mut heap, &[0b1010_1100, 0b0011_0101, 0xFF], 24);
    let s1 = slice(&mut heap, b, 3, 17).expect("room");
    let s1 = bits(&heap, s1).expect("a slice");
    let s2 = slice(&mut heap, s1, 2, 9).expect("room");
    let s2 = bits(&heap, s2).expect("a slice");
    // Bits 5..14 of the owner: 1 0 0 0 0 1 1 0 1.
    assert_eq!(bytes(&heap, s2), vec![0b1000_0110, 0b1000_0000]);
    assert_eq!(read_uint(&heap, s2, 0, 9), BigInt::from(0b1_0000_1101));
    assert_eq!(read_uint(&heap, s2, 5, 8), BigInt::from(0b1101_0000));
    let want = owner(&mut heap, &[0b1000_0110, 0b1000_0000], 9);
    assert!(equal(&heap, s2, want));
}

#[test]
fn join_packs_at_any_bit() {
    let mut heap = Heap::default();
    let a = owner(&mut heap, &[0b1010_0000], 3);
    let b = owner(&mut heap, &[0xFF, 0b1000_0000], 9);
    let j = join(&mut heap, &[a, b, a]).expect("room");
    let j = bits(&heap, j).expect("a binary");
    assert_eq!(j.len, 15);
    // 101, then nine 1s, then 101.
    assert_eq!(bytes(&heap, j), vec![0b1011_1111, 0b1111_1010]);
}

#[test]
fn utf8_reads_one_code_point_or_none() {
    let mut heap = Heap::default();
    let b = owner(&mut heap, "é!".as_bytes(), 24);
    assert_eq!(read_utf8(&heap, b, 0), Some(('é' as u32, 16)));
    assert_eq!(read_utf8(&heap, b, 16), Some(('!' as u32, 8)));
    assert_eq!(read_utf8(&heap, b, 8), None);
    assert_eq!(read_utf8(&heap, b, 24), None);
}

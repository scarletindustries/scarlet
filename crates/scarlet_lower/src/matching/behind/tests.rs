use super::*;

/// A set of places has those that were put in it, in order, whichever came first.
#[test]
fn a_set_has_the_places_that_were_put_in_it() {
    for all in [
        vec![],
        vec![0],
        vec![63, 64, 65],
        vec![200, 3, 127, 128, 0, 64, 3, 1000],
    ] {
        let mut set = PlaceSet::default();
        for p in &all {
            set.insert(Place(*p));
        }
        let want: BTreeSet<Place> = all.into_iter().map(Place).collect();
        assert_eq!(set.iter().collect::<Vec<_>>(), in_vec(&want));
        for p in 0..1100 {
            assert_eq!(set.contains(Place(p)), want.contains(&Place(p)), "{p}");
        }
    }
}

fn in_vec(all: &BTreeSet<Place>) -> Vec<Place> {
    all.iter().copied().collect()
}

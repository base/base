use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    // TODO: Vec<u8> needs drop, can't batch with codspeed.
    // for &size in &SIZE_NIBBLES {
    //     bench_arbitrary_with(
    //         criterion,
    //         &format!("from_nibbles/{size}"),
    //         arb_vec(0u8..16, size),
    //         |v| Nibbles::from_nibbles(black_box(&v)),
    //     );
    // }

    // for &size in &SIZE_BYTES {
    //     bench_arbitrary_with(criterion, &format!("unpack/{size}"), bytes_strategy(size),
    // |bytes| {         Nibbles::unpack(black_box(&bytes))
    //     });
    // }

    for &size in &SIZE_BYTES {
        bench_arbitrary_with(
            criterion,
            &format!("pack/{size}"),
            nibbles_strategy(size * 2),
            |nibbles| black_box(&nibbles).pack(),
        );
    }

    for &size in &SIZE_BYTES {
        bench_unop(criterion, "pack_to", size * 2, |nibbles| {
            let mut buf = SmallVec::<[u8; 32]>::new();
            buf.resize(nibbles.len().div_ceil(2), 0);
            nibbles.pack_to(black_box(&mut buf));
            buf
        });
    }
}

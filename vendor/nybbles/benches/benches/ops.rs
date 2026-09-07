use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    // TODO: Vec<u8> needs drop, can't batch with codspeed.
    // for &size in &SIZE_NIBBLES {
    //     bench_arbitrary_with(
    //         criterion,
    //         &format!("push/{size}"),
    //         arb_vec(0u8..16, size),
    //         |nibbles| {
    //             let mut nib = Nibbles::new();
    //             for nibble in nibbles {
    //                 nib.push(black_box(nibble));
    //             }
    //             nib
    //         },
    //     );
    // }

    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "pop", size, |nib| {
            let mut nib = nib;
            while nib.pop().is_some() {}
            nib
        });
    }

    for &size in &SIZE_NIBBLES[..SIZE_NIBBLES.len() - 1] {
        bench_binop(criterion, "join", size, |a, b| black_box(&a).join(black_box(&b)));
    }

    for &size in &SIZE_NIBBLES[..SIZE_NIBBLES.len() - 1] {
        bench_binop(criterion, "extend", size, |a, b| {
            let mut a = a;
            a.extend(black_box(&b));
            a
        });
    }

    for &size in &SIZE_NIBBLES {
        bench_arbitrary_with(criterion, &format!("set_at/{size}"), nibbles_strategy(size), |nib| {
            let mut nib = nib;
            for i in 0..nib.len() {
                nib.set_at(black_box(i), black_box((i % 16) as u8));
            }
            nib
        });
    }

    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "get_byte", size, |nib| {
            let mut sum = 0u64;
            for i in 0..nib.len().saturating_sub(1) {
                if let Some(byte) = nib.get_byte(black_box(i)) {
                    sum = sum.wrapping_add(byte as u64);
                }
            }
            sum
        });

        bench_unop(criterion, "get_byte_unchecked", size, |nib| {
            let mut sum = 0u64;
            for i in 0..nib.len().saturating_sub(1) {
                let byte = nib.get_byte_unchecked(black_box(i));
                sum = sum.wrapping_add(byte as u64);
            }
            sum
        });
    }

    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "increment", size, |nib| black_box(&nib).increment());
    }

    for &size in &SIZE_NIBBLES {
        bench_arbitrary_with(
            criterion,
            &format!("truncate/{size}"),
            nibbles_strategy(size),
            |nib| {
                let mut nib = nib;
                nib.truncate(black_box(size / 2));
                nib
            },
        );
    }

    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "clear", size, |nib| {
            let mut nib = nib;
            nib.clear();
            nib
        });
    }

    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "first", size, |nib| nib.first());
        bench_unop(criterion, "last", size, |nib| nib.last());
    }
}

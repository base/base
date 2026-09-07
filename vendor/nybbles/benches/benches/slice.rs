use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    for &size in &SIZE_NIBBLES {
        let name = |n: &str| format!("slice/{n}/{size}");

        bench_arbitrary_with(criterion, &name("from_start"), nibbles_strategy(size), |nib| {
            let end = nib.len() / 2;
            black_box(&nib).slice(black_box(0..end))
        });

        bench_arbitrary_with(criterion, &name("middle"), nibbles_strategy(size), |nib| {
            let start = nib.len() / 4;
            let end = nib.len() / 2;
            black_box(&nib).slice(black_box(start..end))
        });

        bench_arbitrary_with(criterion, &name("to_end"), nibbles_strategy(size), |nib| {
            let start = nib.len() / 2;
            black_box(&nib).slice(black_box(start..))
        });
    }

    for &size in &SIZE_NIBBLES {
        bench_arbitrary_with(
            criterion,
            &format!("starts_with/{size}"),
            nibbles_strategy(size).prop_map(|n| {
                let prefix_len = n.len() / 4;
                let prefix = n.slice(..prefix_len);
                (n, prefix)
            }),
            |(nib, prefix)| black_box(&nib).starts_with(black_box(&prefix)),
        );
    }

    for &size in &SIZE_NIBBLES {
        bench_arbitrary_with(
            criterion,
            &format!("ends_with/{size}"),
            nibbles_strategy(size).prop_map(|n| {
                let suffix_start = n.len() - n.len() / 4;
                let suffix = n.slice(suffix_start..);
                (n, suffix)
            }),
            |(nib, suffix)| black_box(&nib).ends_with(black_box(&suffix)),
        );
    }
}

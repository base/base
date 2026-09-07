use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    for &size in &SIZE_NIBBLES {
        bench_binop(criterion, "eq", size, |a, b| a == b);
        bench_binop(criterion, "cmp", size, |a, b| a.cmp(&b));
        bench_binop(criterion, "lt", size, |a, b| a < b);
        bench_binop(criterion, "gt", size, |a, b| a > b);
        bench_binop(criterion, "le", size, |a, b| a <= b);
        bench_binop(criterion, "ge", size, |a, b| a >= b);
    }

    for &size in &SIZE_NIBBLES {
        bench_arbitrary_with(
            criterion,
            &format!("common_prefix_length/{size}"),
            nibbles_strategy(size).prop_map(|n| {
                let other = n.slice(..n.len().saturating_sub(1));
                (n, other)
            }),
            |(a, b)| a.common_prefix_length(&b),
        );
    }
}

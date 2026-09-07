use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "iter", size, |nib| {
            let mut sum = 0u64;
            for n in nib.iter() {
                sum = sum.wrapping_add(n as u64);
            }
            sum
        });
    }
}

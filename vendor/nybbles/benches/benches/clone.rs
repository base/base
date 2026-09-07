use crate::prelude::*;

pub fn group(criterion: &mut Criterion) {
    for &size in &SIZE_NIBBLES {
        bench_unop(criterion, "clone", size, black_box);
    }
}

use std::cell::Cell;
use std::hint::black_box;

use criterion::Bencher;

/// Benchmarks `run` over `variants` in rotation so no per-input caching carries over between
/// iterations.
pub fn bench_independent_variants(
    bench: &mut Bencher<'_>,
    variants: &[String],
    mut run: impl FnMut(&str),
) {
    let next = Cell::new(0_usize);
    bench.iter(|| {
        let index = next.get();
        next.set((index + 1) % variants.len());
        run(black_box(variants[index].as_str()));
    });
}

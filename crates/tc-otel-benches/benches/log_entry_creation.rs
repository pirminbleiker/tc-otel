use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tc_otel_benches::LogEntryFixtures;

fn bench_create_simple(c: &mut Criterion) {
    c.bench_function("create_simple_log_entry", |b| {
        b.iter(LogEntryFixtures::simple_message)
    });
}

fn bench_create_typical(c: &mut Criterion) {
    c.bench_function("create_typical_log_entry", |b| {
        b.iter(LogEntryFixtures::typical_message)
    });
}

fn bench_create_complex(c: &mut Criterion) {
    c.bench_function("create_complex_log_entry", |b| {
        b.iter(LogEntryFixtures::complex_message)
    });
}

fn bench_create_variable_complexity(c: &mut Criterion) {
    let mut group = c.benchmark_group("create_variable_complexity");

    for (args, context) in [(1usize, 1usize), (3, 3), (5, 5), (10, 10), (20, 20)] {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("args_{}_context_{}", args, context)),
            &(args, context),
            |b, &(num_args, num_context)| {
                b.iter(|| {
                    LogEntryFixtures::with_counts(black_box(num_args), black_box(num_context))
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(500);
    targets = bench_create_simple, bench_create_typical, bench_create_complex, bench_create_variable_complexity
);
criterion_main!(benches);

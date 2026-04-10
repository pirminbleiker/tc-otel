use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use tc_otel_benches::LogEntryFixtures;
use tc_otel_core::LogRecord;

fn bench_convert_simple(c: &mut Criterion) {
    c.bench_function("convert_simple_to_otel", |b| {
        let entry = black_box(LogEntryFixtures::simple_message());
        b.iter(|| {
            LogRecord::from_log_entry(entry.clone())
        })
    });
}

fn bench_convert_typical(c: &mut Criterion) {
    c.bench_function("convert_typical_to_otel", |b| {
        let entry = black_box(LogEntryFixtures::typical_message());
        b.iter(|| {
            LogRecord::from_log_entry(entry.clone())
        })
    });
}

fn bench_convert_complex(c: &mut Criterion) {
    c.bench_function("convert_complex_to_otel", |b| {
        let entry = black_box(LogEntryFixtures::complex_message());
        b.iter(|| {
            LogRecord::from_log_entry(entry.clone())
        })
    });
}

fn bench_convert_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("convert_scaling");
    group.sample_size(100);

    for (args, context) in [(1, 1), (3, 3), (5, 5), (10, 10), (20, 20)].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("args_{}_context_{}", args, context)),
            &(args, context),
            |b, (num_args, num_context)| {
                let entry = black_box(LogEntryFixtures::with_counts(*num_args, *num_context));
                b.iter(|| {
                    LogRecord::from_log_entry(entry.clone())
                })
            }
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(200);
    targets = bench_convert_simple, bench_convert_typical, bench_convert_complex, bench_convert_scaling
);
criterion_main!(benches);

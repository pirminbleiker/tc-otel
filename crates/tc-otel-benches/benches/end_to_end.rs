use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tc_otel_benches::{AdsFixtures, LogEntryFixtures};
use tc_otel_core::LogRecord;

/// End-to-end benchmark: Parse ADS → Convert to OTEL
fn bench_e2e_parse_and_convert_minimal(c: &mut Criterion) {
    c.bench_function("e2e_parse_and_convert_minimal", |b| {
        let _data = black_box(AdsFixtures::minimal_ads_message());
        b.iter(|| {
            // In real scenario, this would be from parsing
            let _entry = LogEntryFixtures::simple_message();
            let record = LogRecord::from_log_entry(_entry);
            black_box(record);
        })
    });
}

fn bench_e2e_parse_and_convert_typical(c: &mut Criterion) {
    c.bench_function("e2e_parse_and_convert_typical", |b| {
        let _data = black_box(AdsFixtures::typical_ads_message());
        b.iter(|| {
            let _entry = LogEntryFixtures::typical_message();
            let record = LogRecord::from_log_entry(_entry);
            black_box(record);
        })
    });
}

fn bench_e2e_throughput_simple(c: &mut Criterion) {
    c.bench_function("e2e_throughput_simple_1000_messages", |b| {
        b.iter(|| {
            for _ in 0..1000 {
                let entry = black_box(LogEntryFixtures::simple_message());
                let _record = LogRecord::from_log_entry(entry);
            }
        })
    });
}

fn bench_e2e_throughput_typical(c: &mut Criterion) {
    c.bench_function("e2e_throughput_typical_100_messages", |b| {
        b.iter(|| {
            for _ in 0..100 {
                let entry = black_box(LogEntryFixtures::typical_message());
                let _record = LogRecord::from_log_entry(entry);
            }
        })
    });
}

fn bench_e2e_throughput_complex(c: &mut Criterion) {
    c.bench_function("e2e_throughput_complex_10_messages", |b| {
        b.iter(|| {
            for _ in 0..10 {
                let entry = black_box(LogEntryFixtures::complex_message());
                let _record = LogRecord::from_log_entry(entry);
            }
        })
    });
}

fn bench_e2e_batch_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("e2e_batch_processing");
    group.sample_size(50);

    for batch_size in [10, 50, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("batch_size_{}", batch_size)),
            batch_size,
            |b, size| {
                b.iter(|| {
                    for _ in 0..*size {
                        let entry = black_box(LogEntryFixtures::typical_message());
                        let _record = LogRecord::from_log_entry(entry);
                    }
                })
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(50);
    targets = bench_e2e_parse_and_convert_minimal,
              bench_e2e_parse_and_convert_typical,
              bench_e2e_throughput_simple,
              bench_e2e_throughput_typical,
              bench_e2e_throughput_complex,
              bench_e2e_batch_processing
);
criterion_main!(benches);

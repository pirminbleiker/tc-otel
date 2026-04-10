use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use tc_otel_ads::AdsParser;
use tc_otel_benches::AdsFixtures;

fn bench_parse_minimal(c: &mut Criterion) {
    c.bench_function("parse_minimal_message", |b| {
        let data = black_box(AdsFixtures::minimal_ads_message());
        b.iter(|| AdsParser::parse(&data))
    });
}

fn bench_parse_typical(c: &mut Criterion) {
    c.bench_function("parse_typical_message", |b| {
        let data = black_box(AdsFixtures::typical_ads_message());
        b.iter(|| AdsParser::parse(&data))
    });
}

fn bench_parse_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_scaling");

    for message_complexity in [1, 5, 10, 20].iter() {
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("args_and_context_{}", message_complexity)),
            message_complexity,
            |b, &_count| {
                let data = black_box(AdsFixtures::typical_ads_message()); // Reuse for now
                b.iter(|| AdsParser::parse(&data))
            },
        );
    }
    group.finish();
}

criterion_group!(
    name = benches;
    config = Criterion::default().sample_size(1000);
    targets = bench_parse_minimal, bench_parse_typical, bench_parse_scaling
);
criterion_main!(benches);

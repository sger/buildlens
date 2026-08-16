use buildlens_core::AnalyzeOptions;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use std::io::Cursor;

fn parser_benchmark(c: &mut Criterion) {
    let fixtures = [
        ("small", include_str!("../../../fixtures/sample.log")),
        (
            "diagnostic_heavy",
            include_str!("../../../fixtures/swift6-warnings.log"),
        ),
        (
            "test_heavy",
            include_str!("../../../fixtures/test-crash.log"),
        ),
        (
            "graph_heavy",
            include_str!("../../../fixtures/graph-cycle.log"),
        ),
        ("malformed", include_str!("../../../fixtures/malformed.log")),
    ];
    for (name, log) in fixtures {
        c.bench_function(&format!("parse_{name}_fixture"), |b| {
            b.iter(|| {
                buildlens_parser::analyze_reader(
                    Cursor::new(black_box(log)),
                    AnalyzeOptions::default(),
                )
                .unwrap()
            })
        });
    }

    let large_log = (0..500_000)
        .map(|index| {
            format!("/Users/ci/File{index}.swift:1:1: warning: benchmark warning {index}\n")
        })
        .collect::<String>();
    c.bench_function("parse_40mb_representative_log", |b| {
        b.iter(|| {
            buildlens_parser::analyze_reader(
                Cursor::new(black_box(&large_log)),
                AnalyzeOptions::default(),
            )
            .unwrap()
        })
    });
}
criterion_group!(benches, parser_benchmark);
criterion_main!(benches);

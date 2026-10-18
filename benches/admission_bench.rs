use criterion::{black_box, criterion_group, criterion_main, Criterion};
use snubber::{EpochGate, Priority, SnubberGuard, SnubberNode, TransactionHeader};
use std::time::Duration;

fn fast() -> Criterion {
    Criterion::default()
        .warm_up_time(Duration::from_millis(800))
        .measurement_time(Duration::from_secs(2))
        .sample_size(50)
}

fn bench_layer1(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer1_core");

    g.bench_function("guard_new_commit", |b| {
        b.iter(|| {
            let guard = SnubberGuard::new(black_box(1), black_box(1), || {});
            black_box(guard.commit());
        });
    });

    g.bench_function("epoch_gate_observe", |b| {
        let mut gate = EpochGate::new();
        let mut epoch = 1u64;
        b.iter(|| {
            let hdr = TransactionHeader {
                generation: 1,
                epoch,
                task_id: 1,
            };
            let res = gate.observe(black_box(&hdr));
            epoch = epoch.wrapping_add(1);
            black_box(res);
        });
    });

    g.finish();
}

fn bench_layer2(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer2_std");

    // Low-level entry:
    g.bench_function("node_enter_commit", |b| {
        let node = SnubberNode::builder("bench_node")
            .max_in_flight(1024)
            .build();
        let mut seq = 1u64;
        b.iter(|| {
            seq = seq.wrapping_add(1);
            let guard = node.enter(black_box(seq), black_box(1), || {}).expect("a");
            black_box(guard.commit());
        });
    });

    // Auto-sequence entry:
    g.bench_function("node_enter_auto_commit", |b| {
        let node = SnubberNode::builder("bench_auto")
            .max_in_flight(1024)
            .build();
        b.iter(|| {
            let guard = node.enter_auto(|| {}).expect("a");
            black_box(guard.commit());
        });
    });

    // Canonical entry: full scoped cycle (entry + closure + auto-commit):
    g.bench_function("node_scoped_work_commit", |b| {
        let node = SnubberNode::builder("bench_scoped")
            .max_in_flight(1024)
            .build();
        b.iter(|| {
            let res = node.scoped(
                || {},
                || {
                    black_box(42);
                    Ok::<i32, &'static str>(42)
                },
            );
            black_box(res).expect("scoped");
        });
    });

    // Priority entry:
    g.bench_function("node_enter_critical_priority", |b| {
        let node = SnubberNode::builder("bench_prio")
            .max_in_flight(1024)
            .build();
        let mut e = 1u64;
        b.iter(|| {
            e = e.wrapping_add(1);
            let guard = node
                .enter_with_priority(Priority::Critical, black_box(e), black_box(1), || {})
                .expect("a");
            black_box(guard.commit());
        });
    });

    // Transactional state transition:
    g.bench_function("node_transition", |b| {
        let node = SnubberNode::builder("bench_sm").max_in_flight(1024).build();
        let mut st = 0u8;
        b.iter(|| {
            node.transition(
                &mut st,
                black_box(1),
                || {},
                |s| {
                    *s = 1;
                    Ok::<(), &str>(())
                },
            )
            .expect("t");
            black_box(st);
        });
    });

    g.finish();
}

fn bench_layer3(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer3_alloc");

    let node = SnubberNode::builder("bench_erased")
        .max_in_flight(1024)
        .build();

    g.bench_function("erased_enter_auto_commit", |b| {
        b.iter(|| {
            let guard = node.enter_erased_auto(|| {}).expect("a");
            guard.commit();
        });
    });

    g.finish();
}

criterion_group!(
    name = benches;
    config = fast();
    targets = bench_layer1, bench_layer2, bench_layer3
);
criterion_main!(benches);

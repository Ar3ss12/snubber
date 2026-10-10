use criterion::{black_box, criterion_group, criterion_main, Criterion};
use snubber::{EpochGate, SnubberGuard, SnubberNode, TransactionHeader};
use std::sync::atomic::{AtomicBool, Ordering};

fn bench_floor1_guard_commit(c: &mut Criterion) {
    c.bench_function("floor1_guard_commit", |b| {
        let rolled_back = AtomicBool::new(false);
        b.iter(|| {
            let guard = SnubberGuard::new(black_box(1), black_box(1), || {
                rolled_back.store(true, Ordering::Relaxed);
            });
            guard.commit();
        });
    });
}

fn bench_floor1_epoch_gate(c: &mut Criterion) {
    c.bench_function("floor1_epoch_gate_observe", |b| {
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
}

fn bench_floor2_node_enter_commit(c: &mut Criterion) {
    let node = SnubberNode::builder("bench_node")
        .max_in_flight(1024)
        .build();

    use std::sync::atomic::AtomicU64;
    let epoch_gen = AtomicU64::new(1);

    c.bench_function("floor2_node_enter_commit", |b| {
        b.iter(|| {
            let ep = epoch_gen.fetch_add(1, Ordering::Relaxed);
            let guard = node
                .enter(black_box(ep), black_box(1), || {})
                .expect("admitted");
            guard.commit();
        });
    });
}

fn bench_floor2_node_enter_auto_commit(c: &mut Criterion) {
    let node = SnubberNode::builder("bench_node_auto")
        .max_in_flight(1024)
        .build();

    c.bench_function("floor2_node_enter_auto_commit", |b| {
        b.iter(|| {
            let guard = node.enter_auto(|| {}).expect("admitted");
            guard.commit();
        });
    });
}

fn bench_floor3_erased_enter_auto_commit(c: &mut Criterion) {
    let node = SnubberNode::builder("bench_erased")
        .max_in_flight(1024)
        .build();

    c.bench_function("floor3_node_enter_erased_auto_commit", |b| {
        b.iter(|| {
            let guard = node.enter_erased_auto(|| {}).expect("admitted");
            guard.commit();
        });
    });
}

criterion_group!(
    benches,
    bench_floor1_guard_commit,
    bench_floor1_epoch_gate,
    bench_floor2_node_enter_commit,
    bench_floor2_node_enter_auto_commit,
    bench_floor3_erased_enter_auto_commit
);
criterion_main!(benches);

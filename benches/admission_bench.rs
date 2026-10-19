use criterion::{black_box, criterion_group, criterion_main, Criterion};
use snubber::{
    classify_stall_ceiling, gateway, EpochGate, IsrBridge, IsrSignal, Priority, PulseWatch,
    SnubberGuard, SnubberNode, TaskConduit, TransactionHeader, WorkerPulse, OP_PULSE,
};
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

fn bench_layer15(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer1_5_hang_isr");

    // Pure const math: no clock reads, no syscalls, zero allocations.
    g.bench_function("classify_stall_ceiling", |b| {
        b.iter(|| {
            black_box(classify_stall_ceiling(
                black_box(400),
                black_box(0),
                black_box(100),
                black_box(50),
                black_box(2000),
            ))
        });
    });

    // Heartbeat + watcher: one Release store + one compare per iteration.
    g.bench_function("worker_pulse_kick_watch", |b| {
        let pulse = WorkerPulse::new();
        let mut watch = PulseWatch::new(1000);
        let mut wall = 1000u64;
        b.iter(|| {
            pulse.kick();
            let ok = watch.observe(pulse.tick(), wall);
            wall = wall.wrapping_add(1);
            black_box(ok);
        });
    });

    // SPSC ring roundtrip: push from the ISR side, pop on the main side.
    g.bench_function("isr_bridge_push_pop", |b| {
        let bridge = IsrBridge::new();
        b.iter(|| {
            bridge
                .try_push(IsrSignal::new(OP_PULSE, 0, 1))
                .expect("push");
            black_box(bridge.try_pop());
        });
    });

    // Emergency latch: single fetch_or + single swap (bypasses the ring).
    g.bench_function("isr_trip_latch_swap", |b| {
        let bridge = IsrBridge::new();
        b.iter(|| {
            bridge.trip(black_box(7));
            black_box(bridge.take_trips());
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

fn bench_telemetry(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer2_telemetry");

    // Full gateway roundtrip: try_send lands, try_recv drains, depth
    // bookkeeping runs both sides (~10 ns class).
    g.bench_function("gateway_roundtrip_send_recv", |b| {
        let (tx, rx) = gateway::<u8>(1);
        b.iter(|| {
            tx.send(black_box(1)).expect("send");
            black_box(rx.try_recv());
        });
    });

    // HWM/LWM latch triggered: send bounces with `Throttled` + advised
    // retry (two atomic loads + integer math; no lock, no sleep).
    g.bench_function("gateway_send_throttled_latch", |b| {
        let (tx, _rx) = gateway::<u8>(8);
        for i in 0..6u8 {
            tx.send_critical(i).expect("fill to HWM");
        }
        b.iter(|| {
            let r = tx.send(black_box(255));
            let _ = black_box(r);
        });
    });

    // Lock-free directive probe: one Acquire load on the worker side.
    g.bench_function("conduit_check_directive", |b| {
        let (conduit, _handle): (TaskConduit<u8>, _) = TaskConduit::pair(0);
        b.iter(|| black_box(conduit.check_directive()));
    });

    // Atomic auto-sequence minting (lock-free ticket).
    g.bench_function("node_next_sequence", |b| {
        let node = SnubberNode::builder("bench_seq").build();
        b.iter(|| black_box(node.next_sequence()));
    });

    // Stats snapshot under the node lock (cold-path telemetry read).
    g.bench_function("node_stats_snapshot", |b| {
        let node = SnubberNode::builder("bench_stats").build();
        b.iter(|| black_box(node.stats()));
    });

    // Float-free telemetry math over a live node (per-mille ratios).
    g.bench_function("stats_telemetry_math", |b| {
        let node = SnubberNode::builder("bench_tel").build();
        for _ in 0..1000 {
            if let Ok(g) = node.enter_auto(|| {}) {
                g.commit();
            }
        }
        b.iter(|| {
            let s = node.stats();
            black_box((s.shed_total(), s.success_rate_per_mille()));
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
    targets = bench_layer1, bench_layer15, bench_layer2, bench_telemetry, bench_layer3
);
criterion_main!(benches);

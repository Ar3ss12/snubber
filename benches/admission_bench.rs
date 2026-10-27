use criterion::{black_box, criterion_group, criterion_main, Criterion};
use snubber::pool::{namespace_task, task_lane, task_local, PoolConfig, WorkerPool};
use snubber::{
    classify_stall_ceiling, gateway, ConflateSender, EpochGate, ExecutionEnvelope, IsrBridge,
    IsrSignal, PipeDrain, Priority, ProcessToken, PulseWatch, SnubberGuard, SnubberNode,
    StateDigest, TaskConduit, TransactionHeader, WorkerPulse, OP_HEARTBEAT, OP_PULSE,
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

    // Pass-through transit (canary law): open doors — lock-free count +
    // dummy guard, no slots/breaker/EpochGate. Node sealed BEFORE b.iter.
    g.bench_function("node_passthrough_enter_commit", |b| {
        let node = SnubberNode::builder("bench_passthrough")
            .max_in_flight(1024)
            .passthrough(true)
            .build();
        b.iter(|| {
            let guard = node.enter_auto(|| {}).expect("transit never refuses");
            black_box(guard.commit());
        });
    });

    // Pass-through full scoped cycle: same transit through scoped().
    g.bench_function("node_passthrough_scoped_commit", |b| {
        let node = SnubberNode::builder("bench_passthrough_scoped")
            .max_in_flight(1024)
            .passthrough(true)
            .build();
        b.iter(|| {
            let res = node.scoped(
                || {},
                || {
                    black_box(42);
                    Ok::<i32, &'static str>(42)
                },
            );
            black_box(res).expect("scoped transit");
        });
    });

    g.finish();
}

fn bench_telemetry(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer2_telemetry");

    // Full gateway roundtrip on ONE thread: try_send lands, try_recv
    // drains, depth bookkeeping runs both sides. This is a same-thread
    // send/receive pair, not a cross-core producer/consumer roundtrip --
    // no cache-line ping-pong or wakeup is included.
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
        // Prove the latch actually fires: without this, a shifted HWM
        // would silently make the bench measure uncontended sends under
        // a throttled name.
        assert!(
            tx.send(255).is_err(),
            "gateway above HWM must refuse: the throttled-latch bench would be measuring the fast path"
        );
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

fn bench_impedance(c: &mut Criterion) {
    let mut g = c.benchmark_group("layer2_impedance");

    // Empty slot: publish then sample. One short slot mutex each way,
    // no payload clone, no allocation.
    g.bench_function("conflate_publish_take", |b| {
        let (tx, rx) = ConflateSender::<u64>::pair();
        let mut v = 0u64;
        b.iter(|| {
            v = v.wrapping_add(1);
            black_box(tx.publish(black_box(v)));
            black_box(rx.take_latest());
        });
    });

    // Occupied slot: latest wins and the replaced value is counted, never
    // queued. Measures conflation cost, not consumer cadence.
    g.bench_function("conflate_publish_replace", |b| {
        let (tx, _rx) = ConflateSender::<u64>::pair();
        let mut v = 0u64;
        b.iter(|| {
            v = v.wrapping_add(1);
            black_box(tx.publish(black_box(v)));
        });
    });

    // Envelope judgment on a plain (non-profiled) guard: payload half is
    // exact, wall half stays Inside without clock evidence. In-corridor
    // payload takes no telemetry lock.
    g.bench_function("node_commit_enveloped_payload", |b| {
        let node = SnubberNode::builder("bench_env")
            .max_in_flight(1024)
            .execution_envelope(ExecutionEnvelope::new(0, 0, 16, 1024))
            .build();
        b.iter(|| {
            let guard = node.enter_auto(|| {}).expect("a");
            black_box(guard.commit_enveloped(black_box(64)));
        });
    });

    g.finish();
}

fn bench_layer4_layer5(c: &mut Criterion) {
    use snubber::{StateDigest, StaticNode, STATE_LEN};

    let mut g = c.benchmark_group("layer4_layer5_diagnostics");

    // 1. ProcessToken wire framing (44-byte framed serialization)
    g.bench_function("process_token_to_bytes", |b| {
        let token = ProcessToken::new(black_box(42), black_box(7), [0xAA; 28]);
        b.iter(|| black_box(token.to_bytes()));
    });

    // 2. ProcessToken body parse (40 bytes, LE fields, no alloc)
    g.bench_function("process_token_parse", |b| {
        let wire = ProcessToken::new(42, 7, [0xAA; 28]).to_bytes();
        let mut body = [0u8; 40];
        body.copy_from_slice(&wire[4..]);
        b.iter(|| black_box(ProcessToken::from_bytes(black_box(&body))));
    });

    // 3. StateDigest creation & CRC computation (64 bytes, cache line)
    g.bench_function("state_digest_new_crc", |b| {
        let bytes = [0x55u8; STATE_LEN];
        b.iter(|| {
            let digest = StateDigest::new(black_box(bytes));
            black_box(digest.crc());
        });
    });

    // 4. StaticNode compile-time bounded slot entry + commit
    g.bench_function("static_node_enter_commit", |b| {
        let node = StaticNode::<1024, 2048>::build("bench_static");
        let mut epoch = 1u64;
        b.iter(|| {
            epoch = epoch.wrapping_add(1);
            let guard = node
                .enter(black_box(epoch), black_box(1), || {})
                .expect("static");
            black_box(guard.commit());
        });
    });

    // 5. SnapshotV1 encode and CRC32 framing (512 bytes, zero-alloc)
    g.bench_function("snapshot_v1_encode_crc", |b| {
        let node = SnubberNode::builder("bench_snap").build();
        let _ = node.enter_auto(|| {}).map(|g| g.commit());
        let snap = node.snapshot_v1();
        b.iter(|| black_box(snap.encode()));
    });

    g.finish();
}

fn bench_pipe_drain(c: &mut Criterion) {
    use std::io::Cursor;

    const FRAMES: u64 = 256;
    let mut wire = Vec::with_capacity(FRAMES as usize * 44);
    for i in 0..FRAMES {
        wire.extend_from_slice(&ProcessToken::commit(i).to_bytes());
    }

    let mut g = c.benchmark_group("layer4_pipe_drain");
    g.throughput(criterion::Throughput::Elements(FRAMES));
    // `drain_sync` owns its state (one Arc + Mutex per stream), so the
    // measurement covers stream setup plus scanning all 256 frames; the
    // per-frame cost is what the throughput column reports.
    // Verify the contract once, outside the timed closure: an `assert_eq!`
    // inside `iter` charges comparison (and potential formatting) to every
    // sample without measuring the parser.
    let probe = PipeDrain::drain_sync(Cursor::new(&wire[..]));
    assert_eq!(
        probe.counters().frames,
        FRAMES,
        "drain_sync must parse the whole chunk"
    );
    g.bench_function("drain_sync_256_frames", |b| {
        b.iter(|| {
            let drain = PipeDrain::drain_sync(Cursor::new(black_box(&wire[..])));
            black_box(drain.counters())
        });
    });
    g.finish();
}

fn bench_drain_feed(c: &mut Criterion) {
    // One frame per call: the full scanner state machine (magic window,
    // body assembly, dispatch) with no reader, no thread, no syscall.
    let frame = ProcessToken::simple(7, OP_HEARTBEAT, 0).to_bytes();
    let mut g = c.benchmark_group("layer4_drain_feed");
    let single = PipeDrain::manual();
    g.bench_function("drain_feed_frame", |b| {
        b.iter(|| {
            let parsed = single.feed(black_box(&frame));
            // Kept inside the measured call on purpose: bench hygiene treats
            // an unasserted `feed` as a silent no-op scanner.
            assert_eq!(parsed, 1, "scanner must parse exactly one frame per feed");
            black_box(parsed)
        });
    });

    // 256 frames in one chunk: amortizes the per-call lock and branch.
    const FRAMES: usize = 256;
    let mut wire = Vec::with_capacity(FRAMES * 44);
    for i in 0..FRAMES as u64 {
        wire.extend_from_slice(&ProcessToken::commit(i).to_bytes());
    }
    let bulk = PipeDrain::manual();
    g.bench_function("drain_feed_256_frames", |b| {
        b.iter(|| {
            let parsed = bulk.feed(black_box(&wire));
            assert_eq!(
                parsed, FRAMES,
                "scanner must parse the whole 256-frame chunk per feed"
            );
            black_box(parsed)
        });
    });
    g.finish();

    // Transport-agnostic token sink: 44 bytes into a real writer that
    // copies. A reusable `Vec` (capacity reserved, cleared per iteration)
    // keeps the timed window allocation-free — a discarding writer skips
    // the copy entirely and understated the cost.
    let mut g = c.benchmark_group("layer4_token_sink");
    let token = ProcessToken::simple(1, 9, 7);
    let digest = StateDigest::new([7u8; 64]);
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    g.bench_function("token_write_to", |b| {
        b.iter(|| {
            buf.clear();
            black_box(token.write_to(&mut buf))
        });
    });
    g.bench_function("state_digest_write_to", |b| {
        b.iter(|| {
            buf.clear();
            black_box(digest.write_to(&mut buf))
        });
    });
    g.finish();
}

fn bench_pool(c: &mut Criterion) {
    use std::io::Cursor;

    let mut g = c.benchmark_group("layer4_worker_pool");

    // Lane namespace packing: pure shifts, no branch, no alloc.
    g.bench_function("namespace_task_pack", |b| {
        b.iter(|| {
            black_box(namespace_task(
                black_box(3),
                black_box(0xDEAD_BEEF_0000_0001),
            ))
        });
    });

    // Lane + local extraction from a namespaced task id.
    let task = namespace_task(3, 0xDEAD_BEEF_0000_0001);
    g.bench_function("namespace_task_unpack", |b| {
        b.iter(|| {
            black_box(task_lane(black_box(task)));
            black_box(task_local(black_box(task)));
        });
    });

    // Attach + reap with an un-settled arm frame in the pipe: the rescue
    // path (EOF check, token take, generation bump, restart math,
    // per-class counters). The drain is built in the batched setup so its
    // construction stays OUT of the timed window.
    let armed_wire = ProcessToken::simple(namespace_task(0, 7), 9, 1)
        .to_bytes()
        .to_vec();
    let pool = WorkerPool::new(PoolConfig::new(1).with_eof_grace(0)).expect("lane");
    let lane = pool.lane(0).expect("lane 0");
    g.bench_function("lane_attach_reap_ambush", |b| {
        b.iter_batched(
            || PipeDrain::drain_sync(Cursor::new(&armed_wire[..])),
            |drain| {
                lane.attach(black_box(1_001), drain).expect("idle lane");
                let _ = black_box(lane.note_death(None, 0).expect("attached child"));
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Same path when the child settled cleanly (no token to hand back).
    let mut escaped_wire = armed_wire.clone();
    escaped_wire.extend_from_slice(&ProcessToken::commit(namespace_task(0, 7)).to_bytes());
    g.bench_function("lane_attach_reap_escape", |b| {
        b.iter_batched(
            || PipeDrain::drain_sync(Cursor::new(&escaped_wire[..])),
            |drain| {
                lane.attach(black_box(1_002), drain).expect("idle lane");
                let _ = black_box(lane.note_death(Some(0), 0).expect("attached child"));
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Aggregate report: four short lane locks + totals folding.
    let report_pool = WorkerPool::new(PoolConfig::new(4).with_eof_grace(0)).expect("four lanes");
    for index in 0..4usize {
        let lane = report_pool.lane(index).expect("lane");
        lane.attach(
            index as u32 + 1,
            PipeDrain::drain_sync(Cursor::new(Vec::new())),
        )
        .expect("attach");
        let _ = lane.note_death(Some(1), 0).expect("reap");
    }
    g.bench_function("report_snapshot_4_lanes", |b| {
        b.iter(|| black_box(report_pool.report()));
    });

    // One external RSS sample through the per-lane memory watch.
    let memory_pool =
        WorkerPool::new(PoolConfig::new(1).with_memory(64_000, 96_000)).expect("one lane");
    let memory_lane = memory_pool.lane(0).expect("lane 0");
    memory_lane
        .attach(2_001, PipeDrain::drain_sync(Cursor::new(Vec::new())))
        .expect("attach");
    g.bench_function("note_memory_kb", |b| {
        b.iter(|| black_box(memory_lane.note_memory_kb(black_box(48_000))));
    });

    // One app-side liveness sweep over four live-but-silent pipes: each
    // lane takes its own state lock, no child is touched.
    struct HeartbeatThenPark {
        frame: Vec<u8>,
        pos: usize,
        park: std::sync::mpsc::Receiver<()>,
    }
    impl std::io::Read for HeartbeatThenPark {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.frame.len() {
                let take = (self.frame.len() - self.pos).min(buf.len());
                buf[..take].copy_from_slice(&self.frame[self.pos..self.pos + take]);
                self.pos += take;
                return Ok(take);
            }
            let _ = self.park.recv();
            Ok(0)
        }
    }

    let lease_pool = WorkerPool::new(PoolConfig::new(4).with_eof_grace(0)).expect("four lanes");
    let mut lease_parks = Vec::new();
    for index in 0..4usize {
        let (park, rx) = std::sync::mpsc::channel();
        lease_parks.push(park);
        lease_pool
            .lane(index)
            .expect("lane")
            .attach(
                index as u32 + 3_001,
                PipeDrain::spawn(HeartbeatThenPark {
                    frame: ProcessToken::simple(namespace_task(index as u16, 1), OP_HEARTBEAT, 0)
                        .to_bytes()
                        .to_vec(),
                    pos: 0,
                    park: rx,
                }),
            )
            .expect("attach");
    }
    // Let every drain parse its heartbeat so `idle_ms` starts from a seen frame.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let parsed_all = || {
        (0..4usize).all(|index| {
            lease_pool
                .lane(index)
                .is_some_and(|lane| lane.counters().frames > 0)
        })
    };
    while !parsed_all() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(parsed_all(), "every lease lane parsed its heartbeat");
    std::thread::sleep(Duration::from_millis(50));
    g.bench_function("expired_lanes_4_lanes", |b| {
        b.iter(|| black_box(lease_pool.expired_lanes(black_box(5))));
    });

    g.finish();
    drop(lease_parks);
}

fn bench_contended(c: &mut Criterion) {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;

    let mut g = c.benchmark_group("contended_multithreaded");

    let concurrency = std::thread::available_parallelism()
        .map(|n| (n.get() / 2).max(2))
        .unwrap_or(4);
    println!("Contended benches: concurrency={concurrency}");

    // IDEAL-1: contended enter_auto+commit, ONE EpochGate lane (F3).
    {
        let node = Arc::new(
            SnubberNode::builder("bench_contended_enter")
                .max_in_flight(65536)
                .trip_after_failures(0)
                .build(),
        );
        let stale = Arc::new(AtomicU64::new(0));
        let admitted = Arc::new(AtomicU64::new(0));
        let name = format!("contended_node_enter_threads_{concurrency}");
        g.bench_function(name, |b| {
            let node = Arc::clone(&node);
            let stale = Arc::clone(&stale);
            let admitted = Arc::clone(&admitted);
            b.iter_custom(|iters| {
                stale.store(0, Ordering::Relaxed);
                admitted.store(0, Ordering::Relaxed);
                let iters_per_thread = (iters as usize / concurrency).max(1);
                let barrier = Arc::new(Barrier::new(concurrency + 1));
                let mut handles = Vec::with_capacity(concurrency);
                for _ in 0..concurrency {
                    let n = Arc::clone(&node);
                    let s = Arc::clone(&stale);
                    let a = Arc::clone(&admitted);
                    let br = Arc::clone(&barrier);
                    handles.push(thread::spawn(move || {
                        br.wait();
                        for _ in 0..iters_per_thread {
                            let mut entered = false;
                            for _ in 0..64 {
                                match n.enter_auto(|| {}) {
                                    Ok(guard) => {
                                        black_box(guard.commit());
                                        a.fetch_add(1, Ordering::Relaxed);
                                        entered = true;
                                        break;
                                    }
                                    Err(snubber::EnterError::StaleEpoch) => {
                                        s.fetch_add(1, Ordering::Relaxed);
                                        thread::yield_now();
                                    }
                                    Err(_) => thread::yield_now(),
                                }
                            }
                            assert!(entered, "stale retry exhausted after 64 attempts");
                        }
                    }));
                }
                // Start the clock at the barrier, not before the spawn
                // loop: workers are parked in their own `br.wait()`, so
                // this measures the contended section, not thread spawn.
                let start = std::time::Instant::now();
                barrier.wait();
                for h in handles {
                    h.join().expect("bench worker panicked");
                }
                let elapsed = start.elapsed();
                let expected = (iters_per_thread * concurrency) as u64;
                assert_eq!(
                    admitted.load(Ordering::Relaxed),
                    expected,
                    "iter_custom multiplies by `iters`; every iteration must admit"
                );
                let stale_seen = stale.load(Ordering::Relaxed);
                // Under 6-thread churn stale retries routinely exceed half
                // the admissions, so this is a one-shot disclosure — not a
                // per-sample assert: the published ns/op INCLUDES retry cost.
                static STALE_WARNED: std::sync::Once = std::sync::Once::new();
                if stale_seen * 2 >= expected {
                    STALE_WARNED.call_once(|| {
                        eprintln!(
                            "BENCH WARN: contended enter spends a large share of iterations on stale retries; ns/op includes retry cost"
                        );
                    });
                }
                elapsed
            });
        });
    }
    // IDEAL-2: full scoped cycle under contention, same single lane.
    {
        let node = Arc::new(
            SnubberNode::builder("bench_contended_scoped")
                .max_in_flight(65536)
                .trip_after_failures(0)
                .build(),
        );
        let name = format!("contended_node_scoped_threads_{concurrency}");
        g.bench_function(name, |b| {
            let node = Arc::clone(&node);
            b.iter_custom(|iters| {
                let iters_per_thread = (iters as usize / concurrency).max(1);
                let barrier = Arc::new(Barrier::new(concurrency + 1));
                let admitted = Arc::new(AtomicU64::new(0));
                let mut handles = Vec::with_capacity(concurrency);
                for _ in 0..concurrency {
                    let n = Arc::clone(&node);
                    let a = Arc::clone(&admitted);
                    let br = Arc::clone(&barrier);
                    handles.push(thread::spawn(move || {
                        br.wait();
                        for _ in 0..iters_per_thread {
                            let mut entered = false;
                            for _ in 0..64 {
                                let res: Result<
                                    (),
                                    snubber::SnubberError<std::convert::Infallible>,
                                > = n.scoped(|| {}, || Ok(()));
                                match res {
                                    Ok(()) => {
                                        a.fetch_add(1, Ordering::Relaxed);
                                        entered = true;
                                        break;
                                    }
                                    Err(_) => thread::yield_now(),
                                }
                            }
                            assert!(entered, "scoped retry exhausted after 64 attempts");
                        }
                    }));
                }
                // Start the clock at the barrier, not before the spawn
                // loop: workers are parked in their own `br.wait()`, so
                // this measures the contended section, not thread spawn.
                let start = std::time::Instant::now();
                barrier.wait();
                for h in handles {
                    h.join().expect("bench worker panicked");
                }
                let elapsed = start.elapsed();
                assert_eq!(
                    admitted.load(Ordering::Relaxed),
                    (iters_per_thread * concurrency) as u64,
                    "iter_custom multiplies by `iters`; every iteration must admit"
                );
                elapsed
            });
        });
    }
    // IDEAL-3: MPSC gateway flood, N producers + 1 drain core.
    {
        let name = format!("contended_gateway_mpsc_threads_{concurrency}");
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let (tx, rx) = gateway::<u64>(65536);
                let iters_per_thread = (iters as usize / concurrency).max(1);
                let barrier = Arc::new(Barrier::new(concurrency + 1));
                let draining = Arc::new(AtomicBool::new(true));
                let d_flag = Arc::clone(&draining);
                let rx_handle = thread::spawn(move || {
                    let mut count = 0usize;
                    while d_flag.load(Ordering::Relaxed) {
                        if rx.try_recv().is_some() {
                            count += 1;
                        } else {
                            thread::yield_now();
                        }
                    }
                    while rx.try_recv().is_some() {
                        count += 1;
                    }
                    count
                });
                let mut handles = Vec::with_capacity(concurrency);
                let sent_ok = Arc::new(AtomicU64::new(0));
                let send_err = Arc::new(AtomicU64::new(0));
                for _ in 0..concurrency {
                    let sender = tx.clone();
                    let br = Arc::clone(&barrier);
                    let ok = Arc::clone(&sent_ok);
                    let err = Arc::clone(&send_err);
                    handles.push(thread::spawn(move || {
                        br.wait();
                        for i in 0..iters_per_thread {
                            match black_box(sender.send_critical(black_box(i as u64))) {
                                Ok(()) => {
                                    ok.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(_) => {
                                    err.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }));
                }
                // Start the clock at the barrier, not before the spawn loop —
                // thread spawn is not part of the measured contended path.
                let start = std::time::Instant::now();
                barrier.wait();
                for h in handles {
                    h.join().expect("bench worker panicked");
                }
                // Stop the clock when the producers finish: the drain
                // thread's remaining queue sweep and its join are teardown,
                // not part of the measured contended send path.
                let elapsed = start.elapsed();
                draining.store(false, Ordering::Relaxed);
                let received = rx_handle.join().expect("bench drain panicked");
                let ok = sent_ok.load(Ordering::Relaxed);
                let err = send_err.load(Ordering::Relaxed);
                assert_eq!(
                    ok + err,
                    (iters_per_thread * concurrency) as u64,
                    "every send is either delivered or refused; none vanish"
                );
                assert!(ok > 0, "gateway bench delivered nothing");
                assert_eq!(
                    received as u64, ok,
                    "drained frame count must equal successful sends"
                );
                elapsed
            });
        });
    }
    // IDEAL-4: two-thread SPSC bridge, push thread + pop thread. Neither
    // thread is pinned to a core, so the number is a thread-handoff cost,
    // not a claim about inter-core bus latency.
    {
        let bridge = Arc::new(IsrBridge::new());
        g.bench_function("cross_thread_isr_bridge_push_pop", |b| {
            let bridge = Arc::clone(&bridge);
            b.iter_custom(|iters| {
                while bridge.try_pop().is_some() {}
                let b_pop = Arc::clone(&bridge);
                let running = Arc::new(AtomicBool::new(true));
                let r_flag = Arc::clone(&running);
                let pop_handle = thread::spawn(move || {
                    let mut popped = 0u64;
                    while r_flag.load(Ordering::Relaxed) || popped < iters {
                        if let Some(sig) = b_pop.try_pop() {
                            black_box(sig);
                            popped += 1;
                        } else {
                            thread::yield_now();
                        }
                    }
                    popped
                });
                // Give the consumer a chance to enter its pop loop so
                // the measured window starts on a live consumer.
                thread::yield_now();
                let start = std::time::Instant::now();
                for i in 0..iters {
                    while bridge
                        .try_push(IsrSignal::new(OP_PULSE, 0, (i & 0xFFFF) as u16))
                        .is_err()
                    {
                        thread::yield_now();
                    }
                }
                // The push side is what this bench measures; the consumer
                // join happens after the clock stops so its teardown is
                // not folded into the per-push latency.
                let elapsed = start.elapsed();
                running.store(false, Ordering::Relaxed);
                let popped = pop_handle.join().expect("bench pop worker panicked");
                assert_eq!(popped, iters, "every pushed signal must be popped");
                elapsed
            });
        });
    }

    g.finish();
}
fn bench_extensibility(c: &mut Criterion) {
    use snubber::{BackpressurePolicy, NodeName};
    use std::sync::Arc;

    let mut g = c.benchmark_group("layer4_extensibility");

    // Capture-capable handler: one dynamic dispatch through Arc<dyn Fn>.
    let handler: Arc<dyn Fn(u32, u32) -> u64 + Send + Sync> =
        Arc::new(|in_flight, max| 1 + u64::from(in_flight) * 3 + u64::from(max));
    g.bench_function("friction_handler_arc_call", |b| {
        b.iter(|| black_box(handler(black_box(4), black_box(16))));
    });

    // Full cold read: lock, Arc clone, dyn call outside the lock.
    let adaptive = SnubberNode::builder("bench/friction")
        .max_in_flight(16)
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 500,
            shed_deeper_than: None,
        })
        .friction_fn(|in_flight, max| 1 + u64::from(in_flight) * 3 + u64::from(max))
        .build();
    g.bench_function("friction_wait_adaptive_arc", |b| {
        b.iter(|| black_box(adaptive.friction_wait_ms()));
    });

    // Legacy fn-pointer math on the same path.
    let legacy = SnubberNode::builder("bench/friction-fn")
        .max_in_flight(16)
        .backpressure(BackpressurePolicy::AdaptiveBlock {
            max_wait_ms: 500,
            shed_deeper_than: None,
        })
        .friction_fn_ptr(|in_flight, max| 1 + u64::from(in_flight) * 3 + u64::from(max))
        .build();
    g.bench_function("friction_wait_adaptive_fn", |b| {
        b.iter(|| black_box(legacy.friction_wait_ms()));
    });

    // Node identity: static clone is a fat-pointer copy, owned is an Arc bump.
    let static_name = NodeName::Static("bench/node");
    let owned_name = NodeName::owned(String::from("bench/node"));
    g.bench_function("node_name_clone_static", |b| {
        b.iter(|| black_box(static_name.clone()));
    });
    g.bench_function("node_name_clone_owned", |b| {
        b.iter(|| black_box(owned_name.clone()));
    });
    g.bench_function("node_name_as_str", |b| {
        b.iter(|| black_box(owned_name.as_str()));
    });

    g.finish();
}

fn bench_async_waiters(c: &mut Criterion) {
    use std::future::Future as _;
    use std::task::{RawWaker, RawWakerVTable, Waker};

    fn noop_waker() -> Waker {
        unsafe fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(core::ptr::null(), &VT)
        }
        unsafe fn wake(_: *const ()) {}
        unsafe fn wake_by_ref(_: *const ()) {}
        unsafe fn drop(_: *const ()) {}
        static VT: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VT)) }
    }

    let mut g = c.benchmark_group("layer2_async_waiters");

    // Ready admission through the async future: future construction
    // (alloc-free on this path) + one attempt poll + guard commit.
    let node = SnubberNode::builder("bench/async-ready")
        .max_in_flight(1024)
        .build();
    g.bench_function("async_enter_ready_poll", |b| {
        b.iter(|| {
            let mut fut = node.enter_async_auto(|| {});
            let waker = noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            match core::pin::Pin::new(&mut fut).poll(&mut cx) {
                std::task::Poll::Ready(Ok(guard)) => guard.commit(),
                _ => panic!("free slot must admit synchronously"),
            }
        });
    });

    // Park/wake/admit roundtrip: the slot is held, so the future parks
    // (queue push + identity flag), the slot drops (wake), then the
    // re-poll admits. Measures the waiter-queue machinery end to end.
    let parked = SnubberNode::builder("bench/async-park")
        .max_in_flight(1)
        .build();
    g.bench_function("async_park_wake_admit_roundtrip", |b| {
        b.iter(|| {
            let slot = parked.enter_auto(|| {}).expect("hold slot");
            let mut fut = parked.enter_async_auto(|| {});
            let waker = noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            assert!(core::pin::Pin::new(&mut fut).poll(&mut cx).is_pending());
            drop(slot); // wake
            match core::pin::Pin::new(&mut fut).poll(&mut cx) {
                std::task::Poll::Ready(Ok(guard)) => guard.commit(),
                _ => panic!("woken future must admit"),
            }
        });
    });

    g.finish();
}

criterion_group!(
    name = benches;
    config = fast();
    targets = bench_layer1, bench_layer15, bench_layer2, bench_telemetry, bench_layer3, bench_impedance, bench_layer4_layer5, bench_pipe_drain, bench_drain_feed, bench_pool, bench_extensibility, bench_async_waiters, bench_contended
);
criterion_main!(benches);

//! IsrBridge ring drain liveness (0.6.2 regression).
//!
//! A malformed cell (opcode 0 is not a valid opcode) used to make
//! `try_pop` return `None` after consuming the slot — ending the
//! caller's `while let` drain with valid signals still pending behind
//! it. The pop must skip the cell and keep draining.

use snubber::{IsrBridge, IsrSignal, OP_PULSE};

#[test]
fn malformed_cell_does_not_stall_the_drain() {
    let bridge = IsrBridge::new();
    assert!(bridge.try_push(IsrSignal::from_raw(0)).is_ok());
    assert!(bridge.try_push(IsrSignal::new(OP_PULSE, 7, 42)).is_ok());
    assert_eq!(bridge.pending(), 2);

    let s = bridge
        .try_pop()
        .expect("valid signal behind a malformed cell");
    assert_eq!(s.opcode(), OP_PULSE);
    assert_eq!(s.reason(), 7);
    assert_eq!(s.value(), 42);

    assert!(bridge.try_pop().is_none());
    assert!(bridge.is_empty());
}

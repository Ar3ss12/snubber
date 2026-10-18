//! CNC mill / 3D printer / laser engraver: the dangerous state is a LIVE
//! tool. Boundary rejects out-of-bed coordinates; Drop kills the beam.
//!
//! Run: `cargo run --example laser_cnc`

use snubber::SnubberNode;

const BED_X: f64 = 300.0;
const BED_Y: f64 = 300.0;

struct Coords {
    x: f64,
    y: f64,
}

fn main() {
    let node = SnubberNode::embedded("cnc/laser");
    // Out-of-bed job: cut at the boundary, beam never arms.
    let bad = Coords { x: 999.0, y: 10.0 };
    let r = node.enter_checked(1, 1, &bad, physics, laser_off);
    assert!(r.is_err(), "out-of-bed job must be rejected at boundary");
    assert_eq!(node.stats().total_started, 0);
    // Valid job, trajectory panics mid-burn: Drop kills the beam.
    let good = Coords { x: 10.0, y: 20.0 };
    let guard = node
        .enter_checked(2, 2, &good, physics, laser_off)
        .expect("valid job arms");
    laser_on();
    let _ = guard; // panic / ? here → laser_off() via Drop
    println!("LASER_CNC: beam owned by guard, boundary holds the bed edge");
}

fn physics(c: &Coords) -> Result<(), &'static str> {
    if !(0.0..=BED_X).contains(&c.x) || !(0.0..=BED_Y).contains(&c.y) {
        return Err("coords outside bed");
    }
    Ok(())
}

fn laser_on() {}
fn laser_off() {}

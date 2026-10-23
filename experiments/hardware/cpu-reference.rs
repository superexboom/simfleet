// SPDX-License-Identifier: AGPL-3.0-or-later
// Calls the pinned upstream implementation, including its original gas meter.
use wfsim_engine::{fight::gas::gas_now, rules::{rng::Rng, space::{first_hit, Vec2}}};

/// # Safety
/// Inputs contain body_count * 2 and count * 4 doubles; out holds count * 3 u64s.
/// All ranges are valid for this call and out does not overlap either input.
#[no_mangle]
pub unsafe extern "C" fn hw_first_hit(
    bodies: *const f64, body_count: usize, rays: *const f64, count: usize, out: *mut u64,
) -> i32 {
    std::panic::catch_unwind(|| {
        let bodies: Vec<Vec2> = std::slice::from_raw_parts(bodies, body_count * 2)
            .chunks_exact(2).map(|v| Vec2 { x: v[0], y: v[1] }).collect();
        let rays = std::slice::from_raw_parts(rays, count * 4);
        let out = std::slice::from_raw_parts_mut(out, count * 3);
        for (ray, dst) in rays.chunks_exact(4).zip(out.chunks_exact_mut(3)) {
            let start = gas_now();
            let result = first_hit(Vec2 { x: ray[0], y: ray[1] }, Vec2 { x: ray[2], y: ray[3] }, &bodies);
            let gas = gas_now().wrapping_sub(start);
            let (index, distance) = result.map_or((u64::MAX, 0.0), |(i, d)| (i as u64, d));
            dst.copy_from_slice(&[index, distance.to_bits(), gas]);
        }
    }).map_or(-1, |_| 0)
}

/// # Safety
/// out is writable for count doubles and remains valid throughout the call.
#[no_mangle]
pub unsafe extern "C" fn hw_rng(seed: u64, offset: u64, count: usize, out: *mut f64) {
    let mut rng = Rng::new(seed.wrapping_add(offset.wrapping_mul(0x9e3779b97f4a7c15)));
    for x in std::slice::from_raw_parts_mut(out, count) {
        *x = rng.next_f64();
    }
}

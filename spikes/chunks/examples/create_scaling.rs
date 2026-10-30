//! Does box3d-rust's create_body cost grow with the number of bodies already in the world?
use pe_spike_chunks::phys::{make_body, new_world, world_def, BodyInit, Shape};
use pe_spike_chunks::time_ms;

fn main() {
    for awake in [false, true] {
        let mut w = new_world(&world_def(1.0, 10.0, false));
        let mut n = 0;
        for batch in 0..8 {
            let (_, ms) = time_ms(|| {
                for _ in 0..500 {
                    let mut b = BodyInit::at([(n % 64) as f64 * 3.0, 10.0 + (n / 4096) as f64 * 3.0, ((n / 64) % 64) as f64 * 3.0]);
                    b.awake = awake;
                    make_body(&mut w, &b, Shape::Ball(0.5));
                    n += 1;
                }
            });
            println!("awake={awake} batch={batch} bodies_before={} us_per_create={:.2}", batch * 500, ms * 1e3 / 500.0);
        }
    }
}

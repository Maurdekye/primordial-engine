use box3d_rust::body::create_body;
use box3d_rust::geometry::Sphere;
use box3d_rust::shape::create_sphere_shape;
use box3d_rust::types::{default_body_def, default_shape_def, BodyType};
use pe_spike_chunks::phys::{new_world, pos, v3, world_def};
use pe_spike_chunks::time_ms;

fn main() {
    let mut w = new_world(&world_def(1.0, 10.0, false));
    let mut n = 0usize;
    for batch in 0..8 {
        let (mut tb, mut ts) = (0.0, 0.0);
        for _ in 0..500 {
            let mut d = default_body_def();
            d.type_ = BodyType::Dynamic;
            d.is_awake = false;
            d.position = pos([(n % 64) as f64 * 3.0, 10.0, ((n / 64) % 64) as f64 * 3.0]);
            let (b, ms) = time_ms(|| create_body(&mut w, &d));
            tb += ms;
            let s = Sphere { center: v3(0.0, 0.0, 0.0), radius: 0.5 };
            let (_, ms2) = time_ms(|| create_sphere_shape(&mut w, b, &default_shape_def(), &s));
            ts += ms2;
            n += 1;
        }
        println!("batch={batch} before={} body_us={:.2} shape_us={:.2}", batch * 500, tb * 2.0, ts * 2.0);
    }
}

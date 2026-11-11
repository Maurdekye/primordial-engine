//! Small vector and quaternion helpers. Quaternions are (x, y, z, w), as OpenXR orders them.
//!
//! None of this needs to be deterministic: it runs only on the author's machine, before pe-world
//! quantizes the result into a hand entry (spec 5.2).

pub type Vec3 = [f32; 3];
pub type Quat = [f32; 4];

pub fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

pub fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

pub fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

pub fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

pub fn len(a: Vec3) -> f32 {
    dot(a, a).sqrt()
}

pub fn lerp3(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    add(a, scale(sub(b, a), t))
}

/// The angle between two vectors in radians, or `None` if either is (nearly) zero.
pub fn angle_between(a: Vec3, b: Vec3) -> Option<f32> {
    let (la, lb) = (len(a), len(b));
    if la < 1e-6 || lb < 1e-6 {
        return None;
    }
    Some((dot(a, b) / (la * lb)).clamp(-1.0, 1.0).acos())
}

pub fn qmul(a: Quat, b: Quat) -> Quat {
    let [ax, ay, az, aw] = a;
    let [bx, by, bz, bw] = b;
    [
        aw * bx + ax * bw + ay * bz - az * by,
        aw * by - ax * bz + ay * bw + az * bx,
        aw * bz + ax * by - ay * bx + az * bw,
        aw * bw - ax * bx - ay * by - az * bz,
    ]
}

pub fn qnormalize(q: Quat) -> Quat {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    if n < 1e-12 { [0.0, 0.0, 0.0, 1.0] } else { [q[0] / n, q[1] / n, q[2] / n, q[3] / n] }
}

/// Spherical interpolation along the shorter arc.
pub fn qslerp(a: Quat, b: Quat, t: f32) -> Quat {
    let mut d = a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3];
    let mut b = b;
    if d < 0.0 {
        b = [-b[0], -b[1], -b[2], -b[3]];
        d = -d;
    }
    if d > 0.9995 {
        // Nearly parallel: normalized lerp is accurate and avoids dividing by a tiny sine.
        let q =
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, a[3] + (b[3] - a[3]) * t];
        return qnormalize(q);
    }
    let theta = d.acos();
    let s = theta.sin();
    let (wa, wb) = (((1.0 - t) * theta).sin() / s, (t * theta).sin() / s);
    qnormalize([a[0] * wa + b[0] * wb, a[1] * wa + b[1] * wb, a[2] * wa + b[2] * wb, a[3] * wa + b[3] * wb])
}

/// The rotation by `omega` (rad/s, in the rig frame) for `dt` seconds, applied to `q`.
pub fn qintegrate(q: Quat, omega: Vec3, dt: f32) -> Quat {
    let angle = len(omega) * dt;
    if angle < 1e-9 {
        return q;
    }
    let axis = scale(omega, 1.0 / len(omega));
    let (s, c) = (angle * 0.5).sin_cos();
    qnormalize(qmul([axis[0] * s, axis[1] * s, axis[2] * s, c], q))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Quat, b: Quat) -> bool {
        let d = (a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3]).abs();
        d > 1.0 - 1e-5
    }

    #[test]
    fn integrate_quarter_turn_about_y() {
        let q = qintegrate([0.0, 0.0, 0.0, 1.0], [0.0, std::f32::consts::PI, 0.0], 0.5);
        let h = std::f32::consts::FRAC_1_SQRT_2;
        assert!(close(q, [0.0, h, 0.0, h]), "{q:?}");
    }

    #[test]
    fn slerp_takes_the_short_arc_and_ends_exactly() {
        let a = [0.0, 0.0, 0.0, 1.0];
        let b = [0.0, 0.0, 0.0, -1.0]; // the same rotation, opposite sign
        assert!(close(qslerp(a, b, 0.5), a));
        let c = qintegrate(a, [1.0, 0.0, 0.0], 1.0);
        assert!(close(qslerp(a, c, 1.0), c));
        assert!(close(qslerp(a, c, 0.0), a));
    }

    #[test]
    fn angle_between_handles_zero_vectors() {
        assert_eq!(angle_between([0.0; 3], [1.0, 0.0, 0.0]), None);
        let a = angle_between([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]).unwrap();
        assert!((a - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }
}

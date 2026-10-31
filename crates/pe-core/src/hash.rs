//! XXH64, pinned here so state hashes never change with a dependency update (design 9.1).
//!
//! It is not a cryptographic hash: sync-check hashes compare honest peers' states, and signatures cover
//! authenticity (10.3). It is fast enough to hash snapshot columns every check frame (several GB/s).
//! The algorithm follows the XXH64 specification and matches the reference implementation (tests).

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

#[inline]
fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2)).rotate_left(31).wrapping_mul(P1)
}

#[inline]
fn merge(acc: u64, val: u64) -> u64 {
    (acc ^ round(0, val)).wrapping_mul(P1).wrapping_add(P4)
}

#[inline]
fn read64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}

#[inline]
fn read32(b: &[u8]) -> u64 {
    u32::from_le_bytes(b[..4].try_into().unwrap()) as u64
}

fn finish(mut h: u64, tail: &[u8]) -> u64 {
    let mut t = tail;
    while t.len() >= 8 {
        h ^= round(0, read64(t));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        t = &t[8..];
    }
    if t.len() >= 4 {
        h ^= read32(t).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        t = &t[4..];
    }
    for &b in t {
        h ^= (b as u64).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// XXH64 of `data` with `seed`, in one call.
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let mut h = Hasher::new(seed);
    h.write(data);
    h.finish()
}

/// Streaming XXH64. Feeding the same bytes in any split gives the same result as [`xxh64`].
#[derive(Clone, Debug)]
pub struct Hasher {
    v: [u64; 4],
    seed: u64,
    total: u64,
    buf: [u8; 32],
    buf_len: usize,
}

impl Hasher {
    pub fn new(seed: u64) -> Self {
        Hasher {
            v: [seed.wrapping_add(P1).wrapping_add(P2), seed.wrapping_add(P2), seed, seed.wrapping_sub(P1)],
            seed,
            total: 0,
            buf: [0; 32],
            buf_len: 0,
        }
    }

    #[inline]
    fn stripe(&mut self, s: &[u8]) {
        self.v[0] = round(self.v[0], read64(&s[0..]));
        self.v[1] = round(self.v[1], read64(&s[8..]));
        self.v[2] = round(self.v[2], read64(&s[16..]));
        self.v[3] = round(self.v[3], read64(&s[24..]));
    }

    pub fn write(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let take = (32 - self.buf_len).min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len < 32 {
                return;
            }
            let buf = self.buf;
            self.stripe(&buf);
            self.buf_len = 0;
        }
        while data.len() >= 32 {
            self.stripe(&data[..32]);
            data = &data[32..];
        }
        self.buf[..data.len()].copy_from_slice(data);
        self.buf_len = data.len();
    }

    pub fn write_u8(&mut self, v: u8) {
        self.write(&[v]);
    }

    pub fn write_u32(&mut self, v: u32) {
        self.write(&v.to_le_bytes());
    }

    pub fn write_u64(&mut self, v: u64) {
        self.write(&v.to_le_bytes());
    }

    pub fn finish(&self) -> u64 {
        let [v1, v2, v3, v4] = self.v;
        let mut h = if self.total >= 32 {
            let mut h = v1
                .rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18));
            h = merge(h, v1);
            h = merge(h, v2);
            h = merge(h, v3);
            merge(h, v4)
        } else {
            self.seed.wrapping_add(P5)
        };
        h = h.wrapping_add(self.total);
        finish(h, &self.buf[..self.buf_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;

    #[test]
    fn known_vectors() {
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
    }

    #[test]
    fn matches_reference_and_streaming() {
        let mut rng = Rng::stream(1, 2);
        let data: Vec<u8> = (0..4096).map(|_| rng.next_u32() as u8).collect();
        for len in (0..200).chain([255, 256, 257, 1000, 4096]) {
            for seed in [0u64, 1, 0xDEAD_BEEF_0123_4567] {
                let d = &data[..len];
                let reference = twox_hash::XxHash64::oneshot(seed, d);
                assert_eq!(xxh64(d, seed), reference, "len {len} seed {seed}");
                // Every split point of a streamed write gives the same result.
                let mut h = Hasher::new(seed);
                let cut = len / 3;
                h.write(&d[..cut]);
                h.write(&d[cut..cut + (len - cut) / 2]);
                h.write(&d[cut + (len - cut) / 2..]);
                assert_eq!(h.finish(), reference, "streamed len {len}");
            }
        }
    }
}

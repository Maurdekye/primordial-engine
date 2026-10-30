//! The floating-point environment check (design 4.3 rule 9).
//!
//! Sim results assume IEEE defaults: round to nearest, subnormals kept (no flush-to-zero or
//! denormals-are-zero), exceptions masked. Native libraries loaded in-process (OpenXR runtimes, audio
//! drivers) can change these thread-local flags. The sim thread calls [`check_and_reset`] every tick.

/// Restores the default environment on this thread. Returns `true` if it already was the default, and
/// `false` if something had changed it (the caller should log that).
pub fn check_and_reset() -> bool {
    imp::check_and_reset()
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use core::arch::asm;

    const DAZ: u32 = 1 << 6;
    const MASKS: u32 = 0x1F80;
    const ROUNDING: u32 = 3 << 13;
    const FTZ: u32 = 1 << 15;

    pub fn read() -> u32 {
        let mut csr: u32 = 0;
        // SAFETY: stmxcsr writes 4 bytes to a valid, aligned u32.
        unsafe { asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags)) };
        csr
    }

    pub fn write(csr: u32) {
        // SAFETY: ldmxcsr reads 4 bytes from a valid u32; every value we write keeps exceptions masked.
        unsafe { asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, preserves_flags, readonly)) };
    }

    pub fn check_and_reset() -> bool {
        let csr = read();
        let want = (csr & !(DAZ | ROUNDING | FTZ)) | MASKS;
        if want == csr {
            return true;
        }
        write(want);
        false
    }

    #[cfg(test)]
    pub fn set_flush_to_zero() {
        write(read() | FTZ | DAZ);
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use core::arch::asm;

    const TRAPS: u64 = 0x9F00; // IOE, DZE, OFE, UFE, IXE, IDE trap enables
    const FZ16: u64 = 1 << 19;
    const RMODE: u64 = 3 << 22;
    const FZ: u64 = 1 << 24;

    pub fn read() -> u64 {
        let fpcr: u64;
        // SAFETY: reading FPCR has no side effects.
        unsafe { asm!("mrs {}, fpcr", out(reg) fpcr, options(nomem, nostack, preserves_flags)) };
        fpcr
    }

    pub fn write(fpcr: u64) {
        // SAFETY: we only clear flush-to-zero, rounding-mode and trap-enable bits.
        unsafe { asm!("msr fpcr, {}", in(reg) fpcr, options(nomem, nostack, preserves_flags)) };
    }

    pub fn check_and_reset() -> bool {
        let fpcr = read();
        let want = fpcr & !(TRAPS | FZ16 | RMODE | FZ);
        if want == fpcr {
            return true;
        }
        write(want);
        false
    }

    #[cfg(test)]
    pub fn set_flush_to_zero() {
        write(read() | FZ);
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod imp {
    /// wasm32 and other targets have no mutable floating-point environment.
    pub fn check_and_reset() -> bool {
        true
    }

    #[cfg(test)]
    pub fn set_flush_to_zero() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn detects_and_resets_flush_to_zero() {
        let tiny = core::hint::black_box(f32::MIN_POSITIVE);
        assert!(check_and_reset() || check_and_reset());
        imp::set_flush_to_zero();
        // With flush-to-zero on, halving the smallest normal gives 0 instead of a subnormal.
        assert_eq!(core::hint::black_box(tiny) * core::hint::black_box(0.5), 0.0);
        assert!(!check_and_reset(), "the change must be reported");
        assert!(check_and_reset());
        assert!(core::hint::black_box(tiny) * core::hint::black_box(0.5) > 0.0);
    }
}

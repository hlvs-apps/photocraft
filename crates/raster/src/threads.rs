//! How many OS threads one data-parallel kernel may use.
//!
//! A few kernels (the feather blur, the effect blurs and distance passes) split their rows over
//! scoped threads rather than Rayon, so they never wait inside a Rayon tile. They therefore sit
//! outside any Rayon pool an embedder sizes; [`set_max_kernel_threads`] bounds them too.

use std::sync::atomic::{AtomicUsize, Ordering};

static MAX: AtomicUsize = AtomicUsize::new(0);

/// Caps, process-wide, how many threads (the calling one included) each kernel uses from now on.
/// `1` runs them on the calling thread; `0` restores the default, the machine's parallelism.
pub fn set_max_kernel_threads(n: usize) {
    MAX.store(n, Ordering::Relaxed);
}

/// Threads a kernel may use: the machine's parallelism, at most `limit` and at most the cap from
/// [`set_max_kernel_threads`]; at least one, and one on wasm.
pub fn kernel_threads(limit: usize) -> usize {
    if cfg!(target_arch = "wasm32") {
        return 1;
    }
    let n = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, limit.max(1));
    match MAX.load(Ordering::Relaxed) {
        0 => n,
        cap => n.min(cap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cap_bounds_every_kernel_and_zero_restores_the_default() {
        let machine = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(kernel_threads(16), machine.clamp(1, 16));
        assert_eq!(kernel_threads(0), 1);
        set_max_kernel_threads(1);
        assert_eq!(kernel_threads(16), 1);
        set_max_kernel_threads(3);
        assert_eq!(kernel_threads(16), machine.clamp(1, 3));
        assert_eq!(kernel_threads(2), machine.clamp(1, 2));
        set_max_kernel_threads(0);
        assert_eq!(kernel_threads(16), machine.clamp(1, 16));
    }
}

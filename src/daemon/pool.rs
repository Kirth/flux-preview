use std::sync::OnceLock;

/// Worker pool for sniffing and decoding.
///
/// Capped at 4-8 threads with 2 MiB stacks instead of rayon's global default (one thread
/// per core, 8 MiB stacks), avoiding the one-time ~50 MiB RSS jump when the global pool
/// commits its stacks. Cribbed from flux `src/services/loader/pool.rs`.
pub fn pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get().clamp(4, 8))
            .unwrap_or(4);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .stack_size(2 * 1024 * 1024)
            .thread_name(|i| format!("preview-{i}"))
            .build()
            .expect("failed to build preview thread pool")
    })
}

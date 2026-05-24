//! Worker trait and shared error type for kernel execution.

use thiserror::Error;

/// Errors that kernel execution can surface.
#[derive(Debug, Error)]
pub enum KernelError {
    /// A user-supplied worker closure returned an error.
    #[error("worker failed at block {block_id}: {source}")]
    Worker {
        /// Opaque block identifier for diagnostics.
        block_id: String,
        /// Underlying error type-erased into a `Box<dyn>` so the trait stays object-safe.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Misalignment between requested block grid and the raster shape.
    #[error("block alignment: {0}")]
    Alignment(String),

    /// I/O failure surfaced upward from `eo-io`.
    #[error("io: {0}")]
    Io(String),
}

/// Result alias for kernel operations.
pub type Result<T> = std::result::Result<T, KernelError>;

/// A worker is run on every block of a raster, in parallel.
///
/// `Input` and `Output` are intentionally generic — `BlockWorker` does not
/// dictate dtypes, dimensionality, or NoData semantics. Implementations of
/// the apply orchestration (rayon / async / GPU) consume this trait.
///
/// # Threading
///
/// Workers must be `Send + Sync` because rayon will call them from multiple
/// threads concurrently with disjoint blocks.
pub trait BlockWorker<Input, Output>: Send + Sync {
    /// Process one input block, producing one output block.
    ///
    /// Returning `Err` short-circuits the kernel and propagates a
    /// [`KernelError::Worker`] back to the caller.
    fn process(
        &self,
        input: &Input,
    ) -> std::result::Result<Output, Box<dyn std::error::Error + Send + Sync>>;
}

/// Blanket impl: any `Fn` that matches the signature is a `BlockWorker`.
impl<F, Input, Output> BlockWorker<Input, Output> for F
where
    F: Fn(&Input) -> std::result::Result<Output, Box<dyn std::error::Error + Send + Sync>>
        + Send
        + Sync,
{
    fn process(
        &self,
        input: &Input,
    ) -> std::result::Result<Output, Box<dyn std::error::Error + Send + Sync>> {
        (self)(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanket_fn_impl_is_callable() {
        let w = |x: &i32| -> std::result::Result<i32, Box<dyn std::error::Error + Send + Sync>> {
            Ok(*x + 1)
        };
        let out = w.process(&41).unwrap();
        assert_eq!(out, 42);
    }

    #[test]
    fn kernel_error_worker_carries_block_id() {
        let e = KernelError::Worker {
            block_id: "(3, 4)".into(),
            source: "boom".into(),
        };
        assert!(e.to_string().contains("(3, 4)"));
    }

    #[test]
    fn kernel_error_display_categories() {
        let a = KernelError::Alignment("rows not divisible".into());
        assert!(a.to_string().contains("alignment"));
        let i = KernelError::Io("disk full".into());
        assert!(i.to_string().contains("io"));
    }
}

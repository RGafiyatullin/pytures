//! Bidirectional bridge between Rust Futures and Python Coroutines, built on PyO3.
//!
//! - [`RustCoroutine`] wraps a Rust [`Future`] as a Python coroutine.
//! - [`PyAwaitable`] wraps a Python awaitable as a Rust [`Future`].

mod py_as_rs;
mod rs_as_py;

pub use py_as_rs::PyAwaitable;
pub use rs_as_py::RustCoroutine;

/// Python module initialization is not thread-safe across threads.
/// All tests that use the Python interpreter must hold this lock.
#[cfg(test)]
pub(crate) static PYTHON_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

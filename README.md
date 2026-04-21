# pytures

Bidirectional bridge between Rust Futures and Python Coroutines, built on [PyO3](https://pyo3.rs/).

Both are poll-based state machines under the hood — `pytures` adapts one to the other while preserving proper wakeup semantics (no busy-polling when an event loop is available).

## Two types

- **`RustCoroutine`** — wraps a Rust `Future` as a Python coroutine, `await`-able from Python.
- **`PyAwaitable`** — wraps a Python awaitable as a Rust `Future`, `.await`-able from Rust.

They compose: a `RustCoroutine` can internally `.await` a `PyAwaitable`, and vice versa.

## Usage

```toml
[dependencies]
pytures = "0.1"
pyo3 = "0.28"
```

### Rust Future as a Python Coroutine

```rust
use pyo3::{Py, PyResult, Python};
use pytures::RustCoroutine;

// Wrap an async block into a Python-awaitable coroutine.
let coro = Python::attach(|py| {
    Py::new(py, RustCoroutine::new(async {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Python::attach(|py| Ok(42i32.into_pyobject(py)?.into_any().unbind()))
    }))
});
// `coro` can now be passed to Python and awaited there.
```

### Python Coroutine as a Rust Future

```rust
use pyo3::{Py, PyAny, PyResult, Python};
use pytures::PyAwaitable;

// Given a Py<PyAny> that is a Python awaitable:
fn await_python(py_coro: Py<PyAny>) -> PyResult<PyAwaitable> {
    PyAwaitable::try_from(py_coro)
}
// The returned PyAwaitable implements Future<Output = PyResult<Py<PyAny>>>.
```

### Composing both directions

See [`examples/select_race.rs`](examples/select_race.rs) for a complete example that races five Python workers (using `aiohttp`) against each other via `tokio::select!`, with Rust controlling the sleep timers.

## How it works

See [INTERNALS.md](INTERNALS.md) for details on the coroutine protocol, waker bridging, and state machines.

## License

[BSD-3-Clause](LICENSE)

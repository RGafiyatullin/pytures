use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, Wake, Waker},
};

use pyo3::{
    Bound, Py, PyAny, PyErr, PyResult, Python,
    exceptions::PyStopIteration,
    pyclass, pymethods,
    types::{PyAnyMethods, PyNone},
};

type BoxFuture = Pin<Box<dyn Future<Output = PyResult<Py<PyAny>>>>>;

static GET_RUNNING_LOOP: OnceLock<Py<PyAny>> = OnceLock::new();

fn get_running_loop_fn(py: Python<'_>) -> PyResult<&'static Py<PyAny>> {
    if let Some(cached) = GET_RUNNING_LOOP.get() {
        return Ok(cached);
    }
    let func = py.import("asyncio")?.getattr("get_running_loop")?.unbind();
    let _ = GET_RUNNING_LOOP.set(func);
    Ok(GET_RUNNING_LOOP.get().unwrap())
}

/// A Python coroutine that drives a Rust [`Future`].
///
/// Created with [`RustCoroutine::new`] and then passed into Python
/// (e.g. via [`Py::new`](pyo3::Py::new)) where it can be `await`-ed.
/// When an asyncio event loop is running, wakeups go through a Python Future
/// so the event loop only re-polls the coroutine when the Rust waker fires.
#[pyclass(unsendable)]
pub struct RustCoroutine {
    future: Option<BoxFuture>,
    waker_future: Option<Py<PyAny>>,
}

impl RustCoroutine {
    pub fn new(future: impl Future<Output = PyResult<Py<PyAny>>> + 'static) -> Self {
        Self {
            future: Some(Box::pin(future)),
            waker_future: None,
        }
    }
}

#[pyclass]
struct SetResultSilently(Py<PyAny>);

#[pymethods]
impl SetResultSilently {
    fn __call__(&self, py: Python<'_>, value: Bound<'_, PyAny>) {
        self.0
            .bind(py)
            .call_method1(pyo3::intern!(py, "set_result"), (value,))
            .ok();
    }
}

#[pymethods]
impl RustCoroutine {
    fn __await__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __iter__(slf: Py<Self>) -> Py<Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let none = PyNone::get(py).as_any().to_owned();
        self.send(py, none)
    }

    fn send(&mut self, py: Python<'_>, _value: Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let mut future = self
            .future
            .take()
            .ok_or_else(|| PyStopIteration::new_err(()))?;

        if let Some(prev) = self.waker_future.take() {
            prev.bind(py).call_method0(pyo3::intern!(py, "cancel")).ok();
        }

        match get_running_loop_fn(py)?.bind(py).call0() {
            Ok(event_loop) => {
                let py_future = event_loop.call_method0(pyo3::intern!(py, "create_future"))?;
                py_future.setattr(pyo3::intern!(py, "_asyncio_future_blocking"), true)?;

                let waker = Waker::from(Arc::new(PyFutureWaker {
                    event_loop: event_loop.unbind(),
                    future: py_future.clone().unbind(),
                }));
                let mut cx = Context::from_waker(&waker);

                match future.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => {
                        py_future.call_method0(pyo3::intern!(py, "cancel"))?;
                        match result {
                            Ok(value) => Err(PyStopIteration::new_err(value)),
                            Err(err) => Err(err),
                        }
                    }
                    Poll::Pending => {
                        self.future = Some(future);
                        let py_future = py_future.unbind();
                        self.waker_future = Some(py_future.clone_ref(py));
                        Ok(py_future)
                    }
                }
            }
            Err(_) => {
                let waker = Waker::noop();
                let mut cx = Context::from_waker(waker);

                match future.as_mut().poll(&mut cx) {
                    Poll::Ready(result) => match result {
                        Ok(value) => Err(PyStopIteration::new_err(value)),
                        Err(err) => Err(err),
                    },
                    Poll::Pending => {
                        self.future = Some(future);
                        Ok(py.None())
                    }
                }
            }
        }
    }

    fn throw(&mut self, exc: Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
        let py = exc.py();
        if let Some(wf) = self.waker_future.take() {
            wf.bind(py).call_method0(pyo3::intern!(py, "cancel")).ok();
        }
        self.future = None;
        Err(PyErr::from_value(exc))
    }

    fn close(&mut self, py: Python<'_>) {
        if let Some(wf) = self.waker_future.take() {
            let _ = wf.bind(py).call_method0(pyo3::intern!(py, "cancel"));
        }
        self.future = None;
    }
}

/// Waker backed by a Python Future — resolves it via `call_soon_threadsafe`
/// so the event loop knows to re-poll the coroutine.
struct PyFutureWaker {
    event_loop: Py<PyAny>,
    future: Py<PyAny>,
}

impl Wake for PyFutureWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        Python::attach(|py| {
            let set_result_silently = Py::new(py, SetResultSilently(self.future.clone_ref(py)))
                .expect("python allocation failure");
            let _ = self.event_loop.bind(py).call_method1(
                pyo3::intern!(py, "call_soon_threadsafe"),
                (set_result_silently, PyNone::get(py)),
            );
        });
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // intentional: serialize Python tests
mod tests {
    use std::time::Duration;

    use pyo3::{IntoPyObject, types::PyModule};

    use super::*;

    /// Drives a RustCoroutine by calling `send()` in a tight loop (without
    /// yielding to the event loop between calls). Each `send()` that returns
    /// `Pending` yields a new Python future. With the fix, the *previous*
    /// future is cancelled at the top of every `send()`. Without it the
    /// futures stay pending and accumulate.
    ///
    /// To reproduce the bug, comment out the `waker_future.take() + cancel`
    /// block in `send()` (lines 83-85 of rs_as_py.rs).
    #[tokio::test]
    async fn waker_future_cancelled_on_repoll() {
        let _guard = crate::PYTHON_TEST_MUTEX.lock().unwrap();
        tokio::task::spawn_blocking(|| {
            Python::attach(|py| -> PyResult<()> {
                let module = PyModule::from_code(
                    py,
                    c"
async def drive_and_check(coro, expected_pending_polls):
    futures = []
    try:
        while True:
            f = coro.send(None)
            futures.append(f)
    except StopIteration as e:
        result = e.value

    cancelled = sum(1 for f in futures if f.cancelled())
    assert cancelled == len(futures), (
        f'expected all {len(futures)} waker futures to be cancelled, '
        f'but only {cancelled} were'
    )
    assert len(futures) == expected_pending_polls, (
        f'expected {expected_pending_polls} pending polls, got {len(futures)}'
    )
    return result
",
                    c"test_waker_leak.py",
                    c"test_waker_leak",
                )?;
                let drive_and_check = module.getattr("drive_and_check")?;

                const PENDING_POLLS: usize = 5;

                let rust_coro = Py::new(
                    py,
                    RustCoroutine::new(async {
                        for _ in 0..PENDING_POLLS {
                            let mut yielded = false;
                            std::future::poll_fn(|cx| {
                                if yielded {
                                    Poll::Ready(())
                                } else {
                                    yielded = true;
                                    cx.waker().wake_by_ref();
                                    Poll::Pending
                                }
                            })
                            .await;
                        }
                        Python::attach(|py| Ok(42i32.into_pyobject(py)?.into_any().unbind()))
                    }),
                )?;

                let py_coro = drive_and_check.call1((rust_coro, PENDING_POLLS))?;

                let asyncio = py.import("asyncio")?;
                let event_loop = asyncio.call_method0("new_event_loop")?;
                let result = event_loop.call_method1("run_until_complete", (&py_coro,))?;
                event_loop.call_method0("close")?;

                assert_eq!(result.extract::<i32>()?, 42);
                Ok(())
            })
        })
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn nested_tokio_sleep_through_python_event_loop() {
        let _guard = crate::PYTHON_TEST_MUTEX.lock().unwrap();
        let result = tokio::task::spawn_blocking(|| {
            Python::attach(|py| -> PyResult<i32> {
                let module = PyModule::from_code(
                    py,
                    c"async def py_wrapper(coro):\n    return await coro",
                    c"test.py",
                    c"test",
                )?;
                let py_wrapper = module.getattr("py_wrapper")?;

                let rust_coro = Py::new(
                    py,
                    RustCoroutine::new(async {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Python::attach(|py| Ok(42i32.into_pyobject(py)?.into_any().unbind()))
                    }),
                )?;

                let py_coro = py_wrapper.call1((rust_coro,))?;

                let asyncio = py.import("asyncio")?;
                let event_loop = asyncio.call_method0("new_event_loop")?;
                let result = event_loop.call_method1("run_until_complete", (&py_coro,))?;
                event_loop.call_method0("close")?;

                result.extract::<i32>()
            })
        })
        .await
        .unwrap()
        .unwrap();

        assert_eq!(result, 42_i32);
    }
}

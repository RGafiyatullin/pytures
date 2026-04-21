use std::{
    mem,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll, Waker, ready},
};

use pyo3::{
    Bound, Py, PyAny, PyErr, PyResult, Python,
    exceptions::{PyRuntimeError, PyStopIteration, PyValueError},
    pyclass, pymethods,
    types::PyAnyMethods,
};

static ISFUTURE: OnceLock<Py<PyAny>> = OnceLock::new();

fn isfuture_fn(py: Python<'_>) -> PyResult<&'static Py<PyAny>> {
    if let Some(cached) = ISFUTURE.get() {
        return Ok(cached);
    }
    let func = py.import("asyncio.futures")?.getattr("isfuture")?.unbind();
    let _ = ISFUTURE.set(func);
    Ok(ISFUTURE.get().unwrap())
}

/// A Rust [`Future`](std::future::Future) that drives a Python awaitable.
///
/// Constructed via [`TryFrom<Bound<'_, PyAny>>`] or [`TryFrom<Py<PyAny>>`].
/// The input must have an `__await__` method; if it is also an `asyncio` Future
/// (`asyncio.futures.isfuture` returns `True`), completion and cancellation are
/// propagated back to it.
#[pin_project::pin_project(PinnedDrop)]
pub struct PyAwaitable {
    awaitable: Py<PyAny>,
    is_future: bool,
    state: State,
}

impl TryFrom<Bound<'_, PyAny>> for PyAwaitable {
    type Error = PyErr;

    fn try_from(awaitable: Bound<'_, PyAny>) -> Result<Self, Self::Error> {
        let py = awaitable.py();
        if !awaitable.hasattr("__await__")? {
            return Err(PyValueError::new_err(
                "the object does not have `__await__` property",
            ));
        }
        let is_future = isfuture_fn(py)?
            .bind(py)
            .call1((&awaitable,))?
            .is_truthy()?;

        Ok(Self {
            awaitable: awaitable.unbind(),
            is_future,
            state: State::Init,
        })
    }
}

impl TryFrom<Py<PyAny>> for PyAwaitable {
    type Error = PyErr;

    fn try_from(awaitable: Py<PyAny>) -> Result<Self, Self::Error> {
        Python::attach(|py| Self::try_from(awaitable.into_bound(py)))
    }
}

enum State {
    Init,
    ToSend {
        coroutine: Py<PyAny>,
        to_send: PyResult<Py<PyAny>>,
    },
    Blocked {
        coroutine: Py<PyAny>,
        blocked_on: Py<PyAny>,
    },
    Complete,
}

impl Future for PyAwaitable {
    type Output = PyResult<Py<PyAny>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let state = mem::replace(this.state, State::Complete);
        let result = Python::attach(|py| -> PyResult<(Poll<Py<PyAny>>, State)> {
            match state {
                State::Complete => panic!("poll of an already completed future"),

                State::Init => {
                    let awaitable = this.awaitable.bind(py);
                    let coroutine = awaitable
                        .call_method0(pyo3::intern!(py, "__await__"))?
                        .unbind();

                    cx.waker().wake_by_ref();
                    Ok((
                        Poll::Pending,
                        State::ToSend {
                            coroutine,
                            to_send: Ok(py.None()),
                        },
                    ))
                }

                State::ToSend { coroutine, to_send } => {
                    let dispatched = match &to_send {
                        Ok(val) => coroutine
                            .bind(py)
                            .call_method1(pyo3::intern!(py, "send"), (val.bind(py),)),
                        Err(exc) => coroutine
                            .bind(py)
                            .call_method1(pyo3::intern!(py, "throw"), (exc.clone_ref(py),)),
                    };
                    match dispatched {
                        Ok(py_none) if py_none.is_none() => {
                            cx.waker().wake_by_ref();
                            Ok((
                                Poll::Pending,
                                State::ToSend {
                                    coroutine,
                                    to_send: Ok(py_none.unbind()),
                                },
                            ))
                        }
                        Ok(py_future) => {
                            let is_future = isfuture_fn(py)?
                                .bind(py)
                                .call1((&py_future,))?
                                .is_truthy()?;
                            if !is_future {
                                return Err(PyRuntimeError::new_err(".send returned non-future"));
                            }
                            let is_asyncio_future_blocking = py_future
                                .getattr_opt(pyo3::intern!(py, "_asyncio_future_blocking"))?
                                .map(|afb| afb.is_truthy())
                                .transpose()?
                                .is_some_and(|afb| afb);
                            if !is_asyncio_future_blocking {
                                return Err(PyRuntimeError::new_err(
                                    ".send returned a future with _asyncio_future_blocking != True",
                                ));
                            }

                            py_future
                                .setattr(pyo3::intern!(py, "_asyncio_future_blocking"), false)?;
                            let waker = cx.waker().clone();
                            let done_callback = DoneCallback { waker };
                            py_future.call_method1(
                                pyo3::intern!(py, "add_done_callback"),
                                (done_callback,),
                            )?;

                            Ok((
                                Poll::Pending,
                                State::Blocked {
                                    coroutine,
                                    blocked_on: py_future.unbind(),
                                },
                            ))
                        }
                        Err(stop_iteration)
                            if stop_iteration.is_instance_of::<PyStopIteration>(py) =>
                        {
                            let value = stop_iteration
                                .value(py)
                                .getattr(pyo3::intern!(py, "value"))?;
                            if *this.is_future {
                                this.awaitable
                                    .bind(py)
                                    .call_method1(pyo3::intern!(py, "set_result"), (&value,))?;
                            }
                            Ok((Poll::Ready(value.unbind()), State::Complete))
                        }
                        Err(exception) => {
                            let exc = exception.value(py);
                            if *this.is_future {
                                this.awaitable
                                    .bind(py)
                                    .call_method1(pyo3::intern!(py, "set_exception"), (&exc,))?;
                            }
                            Err(exception)
                        }
                    }
                }

                State::Blocked {
                    coroutine,
                    blocked_on,
                } => {
                    let is_done = blocked_on
                        .bind(py)
                        .call_method0(pyo3::intern!(py, "done"))?
                        .is_truthy()?;

                    if !is_done {
                        return Ok((
                            Poll::Pending,
                            State::Blocked {
                                coroutine,
                                blocked_on,
                            },
                        ));
                    }

                    let to_send = blocked_on
                        .bind(py)
                        .call_method0(pyo3::intern!(py, "result"))
                        .map(|v| v.unbind());
                    cx.waker().wake_by_ref();
                    Ok((Poll::Pending, State::ToSend { coroutine, to_send }))
                }
            }
        });
        match result {
            Err(reason) => {
                *this.state = State::Complete;
                Poll::Ready(Err(reason))
            }
            Ok((poll, next_state)) => {
                *this.state = next_state;
                let value = ready!(poll);
                Poll::Ready(Ok(value))
            }
        }
    }
}

#[pin_project::pinned_drop]
impl PinnedDrop for PyAwaitable {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if matches!(this.state, State::Complete) {
            return;
        }
        Python::attach(|py| {
            if let State::ToSend { coroutine, .. } | State::Blocked { coroutine, .. } = this.state {
                let _ = coroutine.bind(py).call_method0(pyo3::intern!(py, "close"));
            }
            if *this.is_future {
                let _ = this
                    .awaitable
                    .bind(py)
                    .call_method0(pyo3::intern!(py, "cancel"));
            }
        });
    }
}

/// Python-callable callback that wakes the Rust task when a Future completes.
#[pyclass]
struct DoneCallback {
    waker: Waker,
}

#[pymethods]
impl DoneCallback {
    fn __call__(&self, _future: Bound<'_, PyAny>) {
        self.waker.wake_by_ref();
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)] // intentional: serialize Python tests
mod tests {
    use std::time::Duration;

    use pyo3::{IntoPyObject, types::PyModule};

    use super::*;
    use crate::RustCoroutine;

    /// Busy-polls PyAwaitable (no event loop, RustCoroutine returns None).
    #[tokio::test]
    async fn nested_tokio_sleep_through_pyawaitable() {
        let _guard = crate::PYTHON_TEST_MUTEX.lock().unwrap();
        let py_awaitable = Python::attach(|py| -> PyResult<PyAwaitable> {
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
            PyAwaitable::try_from(py_coro.unbind())
        })
        .unwrap();

        let result = Box::pin(py_awaitable).await.unwrap();

        Python::attach(|py| {
            let value: i32 = result.bind(py).extract().unwrap();
            assert_eq!(value, 42_i32);
        });
    }

    /// Full waker path: event loop drives RustCoroutine whose inner async
    /// block awaits a PyAwaitable wrapping a Python coroutine that does
    /// `asyncio.sleep`. PyAwaitable hits State::Blocked, registers
    /// DoneCallback on the sleep future, and the waker chain fires:
    /// timer → DoneCallback → PyFutureWaker → event loop re-polls.
    #[tokio::test]
    async fn combined_waker_chain() {
        let _guard = crate::PYTHON_TEST_MUTEX.lock().unwrap();
        let result = tokio::task::spawn_blocking(|| {
            Python::attach(|py| -> PyResult<i32> {
                let module = PyModule::from_code(
                    py,
                    c"import asyncio\nasync def inner():\n    await asyncio.sleep(0.05)\n    return 42",
                    c"test_inner.py",
                    c"test_inner",
                )?;
                let inner_fn = module.getattr("inner")?;
                let py_coro = inner_fn.call0()?.unbind();

                let outer = Py::new(
                    py,
                    RustCoroutine::new(async move {
                        let py_awaitable = PyAwaitable::try_from(py_coro)?;
                        Box::pin(py_awaitable).await
                    }),
                )?;

                let asyncio = py.import("asyncio")?;
                let event_loop = asyncio.call_method0("new_event_loop")?;
                let result = event_loop.call_method1("run_until_complete", (outer,))?;
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

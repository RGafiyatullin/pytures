//! Race 5 Python workers via `tokio::select!`.
//!
//! Each worker loops 10 times: sleeps via a Rust-backed `tokio::time::sleep`
//! (random 100–200 ms) and does HTTP HEAD to google.com via aiohttp.
//! The first worker to finish wins; the rest are cancelled.
//!
//! Requires `aiohttp`:
//! ```sh
//! pip install aiohttp
//! cargo run -p pytures --example select_race
//! ```

use std::time::Duration;

use pyo3::{
    Py, PyResult, Python, pyfunction,
    types::{PyAnyMethods, PyModule, PyModuleMethods},
    wrap_pyfunction,
};
use pytures::{PyAwaitable, RustCoroutine};

#[pyfunction]
fn rust_sleep(py: Python<'_>) -> PyResult<Py<RustCoroutine>> {
    let ms = rand::random_range(100_u64..=200);
    Py::new(
        py,
        RustCoroutine::new(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Python::attach(|py| Ok(py.None()))
        }),
    )
}

const PYTHON_WORKER: &std::ffi::CStr = c"import aiohttp

async def worker(worker_id, rust_sleep):
    async with aiohttp.ClientSession() as session:
        for i in range(10):
            await rust_sleep()
            resp = await session.head('https://google.com')
            print(f'  worker {worker_id} [{i}] HEAD google.com -> {resp.status}', flush=True)
    return worker_id
";

#[tokio::main]
async fn main() {
    let result = tokio::task::spawn_blocking(|| {
        Python::attach(|py| -> PyResult<i64> {
            let module = PyModule::new(py, "rust_helpers")?;
            module.add_function(wrap_pyfunction!(rust_sleep, &module)?)?;
            py.import("sys")?
                .getattr("modules")?
                .set_item("rust_helpers", &module)?;

            let worker_module = PyModule::from_code(py, PYTHON_WORKER, c"worker.py", c"worker")?;
            let worker_fn = worker_module.getattr("worker")?;
            let sleep_fn = module.getattr("rust_sleep")?;

            let c0 = worker_fn.call1((0_i64, &sleep_fn))?.unbind();
            let c1 = worker_fn.call1((1_i64, &sleep_fn))?.unbind();
            let c2 = worker_fn.call1((2_i64, &sleep_fn))?.unbind();
            let c3 = worker_fn.call1((3_i64, &sleep_fn))?.unbind();
            let c4 = worker_fn.call1((4_i64, &sleep_fn))?.unbind();

            let outer = Py::new(
                py,
                RustCoroutine::new(async move {
                    let a0 = PyAwaitable::try_from(c0)?;
                    let a1 = PyAwaitable::try_from(c1)?;
                    let a2 = PyAwaitable::try_from(c2)?;
                    let a3 = PyAwaitable::try_from(c3)?;
                    let a4 = PyAwaitable::try_from(c4)?;

                    tokio::pin!(a0, a1, a2, a3, a4);

                    tokio::select! {
                        r = a0 => r,
                        r = a1 => r,
                        r = a2 => r,
                        r = a3 => r,
                        r = a4 => r,
                    }
                }),
            )?;

            let asyncio = py.import("asyncio")?;
            let event_loop = asyncio.call_method0("new_event_loop")?;
            let result = event_loop.call_method1("run_until_complete", (outer,))?;
            event_loop.call_method0("close")?;

            result.extract::<i64>()
        })
    })
    .await
    .unwrap()
    .unwrap();

    println!("\nworker {result} finished first!");
}

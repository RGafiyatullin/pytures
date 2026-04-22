//! Waker overhead benchmark.
//!
//! Measures the scheduling overhead of the full Rust→Python→Rust async
//! bridging stack by timing how much a `tokio::sleep` deviates from its
//! expected duration when driven through an asyncio event loop.
//!
//! Each task's sleep duration is randomized within ±`jitter_pct`% of the
//! base delay so that concurrent tasks don't all fire at the same instant.
//!
//! ```sh
//! cargo run -p pytures --example bench_waker_overhead
//! cargo run -p pytures --example bench_waker_overhead -- --concurrency 100 --delay-ms 50
//! cargo run -p pytures --example bench_waker_overhead -- --jitter-pct 20
//! ```

use std::time::{Duration, Instant};

use pyo3::{
    IntoPyObject, Py, PyResult, Python, pyclass, pyfunction,
    types::{PyAnyMethods, PyModule, PyModuleMethods, PyTuple},
    wrap_pyfunction,
};
use pytures::RustCoroutine;

#[pyclass]
struct RustInstant {
    instant: Instant,
    delay_nanos: u128,
}

#[pyfunction]
fn rust_sleep(py: Python<'_>, delay_ms: u64, jitter_pct: u64) -> PyResult<Py<RustCoroutine>> {
    Py::new(
        py,
        RustCoroutine::new(async move {
            let actual_ms = if jitter_pct == 0 {
                delay_ms
            } else {
                let lo = delay_ms.saturating_sub(delay_ms * jitter_pct / 100);
                let hi = delay_ms + delay_ms * jitter_pct / 100;
                rand::random_range(lo..=hi)
            };
            let instant = Instant::now();
            tokio::time::sleep(Duration::from_millis(actual_ms)).await;
            let delay_nanos = actual_ms as u128 * 1_000_000;
            Python::attach(|py| {
                Ok(Py::new(
                    py,
                    RustInstant {
                        instant,
                        delay_nanos,
                    },
                )?
                .into_any())
            })
        }),
    )
}

#[pyfunction]
fn rust_elapsed_nanos<'py>(
    py: Python<'py>,
    instant: &RustInstant,
) -> PyResult<pyo3::Bound<'py, PyTuple>> {
    let elapsed = instant.instant.elapsed().as_nanos();
    (instant.delay_nanos, elapsed).into_pyobject(py)
}

const PYTHON_CODE: &std::ffi::CStr = c"import asyncio

async def run_one(rust_sleep_fn, rust_elapsed_nanos_fn, delay_ms, jitter_pct):
    instant = await rust_sleep_fn(delay_ms, jitter_pct)
    return rust_elapsed_nanos_fn(instant)

async def main(rust_sleep_fn, rust_elapsed_nanos_fn, delay_ms, jitter_pct, concurrency):
    tasks = [
        asyncio.create_task(
            run_one(rust_sleep_fn, rust_elapsed_nanos_fn, delay_ms, jitter_pct)
        )
        for _ in range(concurrency)
    ]
    return await asyncio.gather(*tasks)
";

struct Config {
    delay_ms: u64,
    jitter_pct: u64,
    concurrency: usize,
    iterations: usize,
}

fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().collect();
    let mut config = Config {
        delay_ms: 100,
        jitter_pct: 10,
        concurrency: 10,
        iterations: 10,
    };

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--delay-ms" => {
                config.delay_ms = args[i + 1].parse().expect("invalid --delay-ms value");
                i += 2;
            }
            "--jitter-pct" => {
                config.jitter_pct = args[i + 1].parse().expect("invalid --jitter-pct value");
                i += 2;
            }
            "--concurrency" => {
                config.concurrency = args[i + 1].parse().expect("invalid --concurrency value");
                i += 2;
            }
            "--iterations" => {
                config.iterations = args[i + 1].parse().expect("invalid --iterations value");
                i += 2;
            }
            "--help" | "-h" => {
                eprintln!("Usage: bench_waker_overhead [OPTIONS]");
                eprintln!("  --delay-ms N      base sleep duration in ms (default: 100)");
                eprintln!("  --jitter-pct N    randomize delay by +/-N%% (default: 10)");
                eprintln!("  --concurrency N   concurrent asyncio tasks (default: 10)");
                eprintln!("  --iterations N    number of rounds (default: 10)");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(1);
            }
        }
    }

    config
}

fn report_stats(samples: &[(u128, u128)], config: &Config) {
    let deviations: Vec<f64> = samples
        .iter()
        .map(|&(delay_nanos, elapsed)| elapsed.saturating_sub(delay_nanos) as f64 / 1_000.0)
        .collect();

    let n = deviations.len() as f64;
    let mean = deviations.iter().sum::<f64>() / n;
    let min = deviations.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = deviations.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let variance = deviations.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / n;
    let stddev = variance.sqrt();

    println!("=== Waker Overhead Benchmark ===");
    println!(
        "  delay:       {} ms (+/-{}%)",
        config.delay_ms, config.jitter_pct
    );
    println!("  concurrency: {}", config.concurrency);
    println!("  iterations:  {}", config.iterations);
    println!("  samples:     {}", deviations.len());
    println!();
    println!("  Deviation from expected delay:");
    println!("    min:    {:>10.1} us", min);
    println!("    max:    {:>10.1} us", max);
    println!("    mean:   {:>10.1} us", mean);
    println!("    stddev: {:>10.1} us", stddev);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let config = parse_args();

    eprintln!(
        "running {} iterations, concurrency={}, delay={}ms (+/-{}%)",
        config.iterations, config.concurrency, config.delay_ms, config.jitter_pct,
    );

    let delay_ms = config.delay_ms;
    let jitter_pct = config.jitter_pct;
    let concurrency = config.concurrency;
    let iterations = config.iterations;

    let samples = tokio::task::spawn_blocking(move || {
        Python::attach(|py| -> PyResult<Vec<(u128, u128)>> {
            let module = PyModule::new(py, "bench_helpers")?;
            module.add_function(wrap_pyfunction!(rust_sleep, &module)?)?;
            module.add_function(wrap_pyfunction!(rust_elapsed_nanos, &module)?)?;

            let bench_module = PyModule::from_code(py, PYTHON_CODE, c"bench.py", c"bench")?;
            let main_fn = bench_module.getattr("main")?;
            let sleep_fn = module.getattr("rust_sleep")?;
            let elapsed_fn = module.getattr("rust_elapsed_nanos")?;

            let mut samples = Vec::with_capacity(concurrency * iterations);

            for i in 0..iterations {
                eprint!("  iteration {}/{}...", i + 1, iterations);

                let py_coro =
                    main_fn.call1((&sleep_fn, &elapsed_fn, delay_ms, jitter_pct, concurrency))?;

                let asyncio = py.import("asyncio")?;
                let event_loop = asyncio.call_method0("new_event_loop")?;
                let result = event_loop.call_method1("run_until_complete", (&py_coro,))?;
                event_loop.call_method0("close")?;

                let batch: Vec<(u128, u128)> = result.extract()?;
                eprintln!(" done");
                samples.extend(batch);
            }

            Ok(samples)
        })
    })
    .await
    .unwrap()
    .unwrap();

    println!();
    report_stats(&samples, &config);
}

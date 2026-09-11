use std::{hint::black_box, io, time::Instant};

#[cfg(windows)]
use std::{ffi::c_void, mem::MaybeUninit};

use hyper::{HeaderMap, StatusCode, header::HOST};

use crate::config::RuntimeConfig;
use crate::{handler::simple_response, router::HostRouter};

const ITERATIONS: u64 = 2_000_000;
const TRIALS: usize = 3;

#[derive(Debug)]
pub struct RoutingStageResult {
    pub name: &'static str,
    pub iterations: u64,
    pub samples_ns_per_operation: Vec<f64>,
    pub median_ns_per_operation: f64,
    pub samples_cpu_ns_per_operation: Vec<f64>,
    pub median_cpu_ns_per_operation: f64,
    pub range_percent: f64,
}

pub fn profile_routing(router: &HostRouter) -> io::Result<Vec<RoutingStageResult>> {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, "localhost:8080".parse().unwrap());
    let path = "/api/items";
    let mut samples = [
        ("A_constant_response", Vec::with_capacity(TRIALS)),
        ("B_host_router", Vec::with_capacity(TRIALS)),
        ("C_host_location_router", Vec::with_capacity(TRIALS)),
        ("D_router_simple_handler", Vec::with_capacity(TRIALS)),
    ];
    let mut cpu_samples: [Vec<f64>; 4] = std::array::from_fn(|_| Vec::with_capacity(TRIALS));

    for _ in 0..TRIALS {
        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(simple_response(StatusCode::NOT_FOUND, "Not Found"));
        }
        samples[0].1.push(ns_per_operation(started));
        cpu_samples[0].push(cpu_ns_per_operation(cpu_started)?);

        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(router.find_host(black_box(&headers)).unwrap());
        }
        samples[1].1.push(ns_per_operation(started));
        cpu_samples[1].push(cpu_ns_per_operation(cpu_started)?);

        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(router.route(black_box(&headers), black_box(path)).unwrap());
        }
        samples[2].1.push(ns_per_operation(started));
        cpu_samples[2].push(cpu_ns_per_operation(cpu_started)?);

        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                router
                    .profiling_response(black_box(&headers), black_box(path))
                    .unwrap(),
            );
        }
        samples[3].1.push(ns_per_operation(started));
        cpu_samples[3].push(cpu_ns_per_operation(cpu_started)?);
    }

    Ok(samples
        .into_iter()
        .zip(cpu_samples)
        .map(
            |((name, mut samples_ns_per_operation), mut samples_cpu_ns_per_operation)| {
                samples_ns_per_operation.sort_by(f64::total_cmp);
                samples_cpu_ns_per_operation.sort_by(f64::total_cmp);
                let median_ns_per_operation = samples_ns_per_operation[TRIALS / 2];
                let median_cpu_ns_per_operation = samples_cpu_ns_per_operation[TRIALS / 2];
                let range_percent =
                    (samples_ns_per_operation[TRIALS - 1] - samples_ns_per_operation[0]) * 100.0
                        / median_ns_per_operation;
                RoutingStageResult {
                    name,
                    iterations: ITERATIONS,
                    samples_ns_per_operation,
                    median_ns_per_operation,
                    samples_cpu_ns_per_operation,
                    median_cpu_ns_per_operation,
                    range_percent,
                }
            },
        )
        .collect())
}

pub fn profile_runtime_config(
    reference_router: &HostRouter,
    runtime: &RuntimeConfig,
) -> io::Result<Vec<RoutingStageResult>> {
    let mut headers = HeaderMap::new();
    headers.insert(HOST, "localhost:8080".parse().unwrap());
    let path = "/api/items";
    let mut samples = [
        ("A_0_0_3_router_simple_handler", Vec::with_capacity(TRIALS)),
        (
            "B_runtime_config_compiled_router_simple_handler",
            Vec::with_capacity(TRIALS),
        ),
    ];
    let mut cpu_samples: [Vec<f64>; 2] = std::array::from_fn(|_| Vec::with_capacity(TRIALS));

    for _ in 0..TRIALS {
        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                reference_router
                    .profiling_response(black_box(&headers), black_box(path))
                    .unwrap(),
            );
        }
        samples[0].1.push(ns_per_operation(started));
        cpu_samples[0].push(cpu_ns_per_operation(cpu_started)?);

        let cpu_started = process_cpu_seconds()?;
        let started = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(
                runtime
                    .router()
                    .profiling_response(black_box(&headers), black_box(path))
                    .unwrap(),
            );
        }
        samples[1].1.push(ns_per_operation(started));
        cpu_samples[1].push(cpu_ns_per_operation(cpu_started)?);
    }

    Ok(samples
        .into_iter()
        .zip(cpu_samples)
        .map(
            |((name, mut samples_ns_per_operation), mut samples_cpu_ns_per_operation)| {
                samples_ns_per_operation.sort_by(f64::total_cmp);
                samples_cpu_ns_per_operation.sort_by(f64::total_cmp);
                let median_ns_per_operation = samples_ns_per_operation[TRIALS / 2];
                let median_cpu_ns_per_operation = samples_cpu_ns_per_operation[TRIALS / 2];
                let range_percent =
                    (samples_ns_per_operation[TRIALS - 1] - samples_ns_per_operation[0]) * 100.0
                        / median_ns_per_operation;
                RoutingStageResult {
                    name,
                    iterations: ITERATIONS,
                    samples_ns_per_operation,
                    median_ns_per_operation,
                    samples_cpu_ns_per_operation,
                    median_cpu_ns_per_operation,
                    range_percent,
                }
            },
        )
        .collect())
}

fn ns_per_operation(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1_000_000_000.0 / ITERATIONS as f64
}

fn cpu_ns_per_operation(started_seconds: f64) -> io::Result<f64> {
    Ok((process_cpu_seconds()? - started_seconds) * 1_000_000_000.0 / ITERATIONS as f64)
}

#[cfg(windows)]
#[repr(C)]
struct FileTime {
    low: u32,
    high: u32,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn GetProcessTimes(
        process: *mut c_void,
        creation: *mut FileTime,
        exit: *mut FileTime,
        kernel: *mut FileTime,
        user: *mut FileTime,
    ) -> i32;
}

#[cfg(windows)]
fn process_cpu_seconds() -> io::Result<f64> {
    let mut creation = MaybeUninit::<FileTime>::uninit();
    let mut exit = MaybeUninit::<FileTime>::uninit();
    let mut kernel = MaybeUninit::<FileTime>::uninit();
    let mut user = MaybeUninit::<FileTime>::uninit();
    // SAFETY: all pointers refer to writable FILETIME-sized storage and the
    // pseudo-handle returned by GetCurrentProcess is valid in this process.
    let succeeded = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            creation.as_mut_ptr(),
            exit.as_mut_ptr(),
            kernel.as_mut_ptr(),
            user.as_mut_ptr(),
        )
    };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetProcessTimes initialized every FILETIME after succeeding.
    let (kernel, user) = unsafe { (kernel.assume_init(), user.assume_init()) };
    let ticks = file_time_ticks(kernel) + file_time_ticks(user);
    Ok(ticks as f64 / 10_000_000.0)
}

#[cfg(windows)]
fn file_time_ticks(time: FileTime) -> u64 {
    (u64::from(time.high) << 32) | u64::from(time.low)
}

#[cfg(not(windows))]
fn process_cpu_seconds() -> io::Result<f64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "routing CPU profiling currently requires Windows GetProcessTimes",
    ))
}

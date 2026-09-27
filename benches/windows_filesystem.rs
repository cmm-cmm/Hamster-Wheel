use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use hamster_wheel::windows_fs_profile::{self, Executor, Operation, ProfileSummary};

const CONCURRENCY_LEVELS: [usize; 5] = [1, 4, 8, 16, 32];
const DEDICATED_THREAD_COUNTS: [usize; 4] = [1, 2, 4, 8];
const TRIALS: usize = 3;
const DEFAULT_WARMUP_REQUESTS: u64 = 1_000;
const DEFAULT_MEASURED_REQUESTS: u64 = 10_000;

#[tokio::main]
async fn main() -> io::Result<()> {
    let suite = std::env::args().nth(1).unwrap_or_else(|| "all".to_owned());
    let warmup = env_u64("HW_P3_WARMUP", DEFAULT_WARMUP_REQUESTS)?;
    let measured = env_u64("HW_P3_REQUESTS", DEFAULT_MEASURED_REQUESTS)?;
    let concurrency_levels = env_usize_list("HW_P3_CONCURRENCY", &CONCURRENCY_LEVELS)?;
    let dedicated_thread_counts =
        env_usize_list("HW_P3_DEDICATED_THREADS", &DEDICATED_THREAD_COUNTS)?;
    let fixture = Fixture::create()?;

    println!(
        "P3_CONFIG,suite={suite},warmup={warmup},measured={measured},trials={TRIALS},file_bytes={},concurrency={concurrency_levels:?},dedicated_threads={dedicated_thread_counts:?}",
        fixture.file_size,
    );
    println!(
        "P3_COLUMNS,suite,backend,configured_workers,file_set,operation,concurrency,run,rps,wall_s,cpu_s,effective_cores,cpu_us_per_request,threads_seen,queue_p50_us,queue_p95_us,queue_p99_us,execution_p50_us,execution_p95_us,execution_p99_us,completion_p50_us,completion_p95_us,completion_p99_us,total_p50_us,total_p95_us,total_p99_us,rps_range_pct,p99_range_pct"
    );

    match suite.as_str() {
        "all" => {
            handoff_suite(
                &fixture,
                warmup,
                measured,
                &concurrency_levels,
                &dedicated_thread_counts,
            )
            .await?;
            backend_suite(
                &fixture,
                warmup,
                measured,
                &concurrency_levels,
                &dedicated_thread_counts,
            )
            .await?;
            files_suite(&fixture, warmup, measured, &concurrency_levels).await?;
            operations_suite(&fixture, warmup, measured, &concurrency_levels).await?;
        }
        "handoff" => {
            handoff_suite(
                &fixture,
                warmup,
                measured,
                &concurrency_levels,
                &dedicated_thread_counts,
            )
            .await?
        }
        "backend" => {
            backend_suite(
                &fixture,
                warmup,
                measured,
                &concurrency_levels,
                &dedicated_thread_counts,
            )
            .await?
        }
        "files" => files_suite(&fixture, warmup, measured, &concurrency_levels).await?,
        "operations" => operations_suite(&fixture, warmup, measured, &concurrency_levels).await?,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "suite must be one of: all, handoff, backend, files, operations",
            ));
        }
    }

    Ok(())
}

async fn handoff_suite(
    fixture: &Fixture,
    warmup: u64,
    measured: u64,
    concurrency_levels: &[usize],
    dedicated_thread_counts: &[usize],
) -> io::Result<()> {
    let paths = fixture.same_file();
    for &concurrency in concurrency_levels {
        run_case(
            "handoff",
            Executor::TokioSpawnBlocking,
            "none",
            Operation::Noop,
            fixture.root(),
            Arc::clone(&paths),
            concurrency,
            warmup,
            measured,
        )
        .await?;
    }
    for &worker_count in dedicated_thread_counts {
        let executor = Executor::dedicated(worker_count)?;
        for &concurrency in concurrency_levels {
            run_case(
                "handoff",
                executor.clone(),
                "none",
                Operation::Noop,
                fixture.root(),
                Arc::clone(&paths),
                concurrency,
                warmup,
                measured,
            )
            .await?;
        }
    }
    Ok(())
}

async fn backend_suite(
    fixture: &Fixture,
    warmup: u64,
    measured: u64,
    concurrency_levels: &[usize],
    dedicated_thread_counts: &[usize],
) -> io::Result<()> {
    let paths = fixture.same_file();
    for &concurrency in concurrency_levels {
        run_case(
            "backend",
            Executor::TokioSpawnBlocking,
            "same_1",
            Operation::FullCurrentSequence,
            fixture.root(),
            Arc::clone(&paths),
            concurrency,
            warmup,
            measured,
        )
        .await?;
    }

    for &worker_count in dedicated_thread_counts {
        let executor = Executor::dedicated(worker_count)?;
        for &concurrency in concurrency_levels {
            run_case(
                "backend",
                executor.clone(),
                "same_1",
                Operation::FullCurrentSequence,
                fixture.root(),
                Arc::clone(&paths),
                concurrency,
                warmup,
                measured,
            )
            .await?;
        }
    }
    Ok(())
}

async fn files_suite(
    fixture: &Fixture,
    warmup: u64,
    measured: u64,
    concurrency_levels: &[usize],
) -> io::Result<()> {
    for (file_set, paths) in fixture.file_sets() {
        for &concurrency in concurrency_levels {
            run_case(
                "files",
                Executor::TokioSpawnBlocking,
                file_set,
                Operation::FullCurrentSequence,
                fixture.root(),
                paths.clone(),
                concurrency,
                warmup,
                measured,
            )
            .await?;
        }
    }
    Ok(())
}

async fn operations_suite(
    fixture: &Fixture,
    warmup: u64,
    measured: u64,
    concurrency_levels: &[usize],
) -> io::Result<()> {
    let paths = fixture.same_file();
    for operation in Operation::ALL {
        for &concurrency in concurrency_levels {
            run_case(
                "operations",
                Executor::TokioSpawnBlocking,
                "same_1",
                operation,
                fixture.root(),
                Arc::clone(&paths),
                concurrency,
                warmup,
                measured,
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_case(
    suite: &str,
    executor: Executor,
    file_set: &str,
    operation: Operation,
    canonical_root: Arc<PathBuf>,
    paths: Arc<Vec<PathBuf>>,
    concurrency: usize,
    warmup: u64,
    measured: u64,
) -> io::Result<()> {
    let _ = windows_fs_profile::profile(
        executor.clone(),
        operation,
        Arc::clone(&canonical_root),
        Arc::clone(&paths),
        concurrency,
        warmup,
    )
    .await?;

    let mut runs = Vec::with_capacity(TRIALS);
    for run in 1..=TRIALS {
        let cpu_start = windows::process_cpu_seconds()?;
        let summary = windows_fs_profile::profile(
            executor.clone(),
            operation,
            Arc::clone(&canonical_root),
            Arc::clone(&paths),
            concurrency,
            measured,
        )
        .await?;
        let cpu_seconds = windows::process_cpu_seconds()? - cpu_start;
        let result = TrialResult::new(summary, cpu_seconds);
        print_run(
            suite,
            &executor,
            file_set,
            operation,
            concurrency,
            run,
            &result,
        );
        runs.push(result);
    }
    print_median(suite, &executor, file_set, operation, concurrency, &runs);
    Ok(())
}

struct TrialResult {
    profile: ProfileSummary,
    cpu_seconds: f64,
    effective_cores: f64,
    cpu_us_per_request: f64,
}

impl TrialResult {
    fn new(profile: ProfileSummary, cpu_seconds: f64) -> Self {
        Self {
            effective_cores: cpu_seconds / profile.wall_seconds,
            cpu_us_per_request: cpu_seconds * 1_000_000.0 / profile.request_count as f64,
            profile,
            cpu_seconds,
        }
    }
}

fn print_run(
    suite: &str,
    executor: &Executor,
    file_set: &str,
    operation: Operation,
    concurrency: usize,
    run: usize,
    result: &TrialResult,
) {
    print_row(
        "P3_RUN",
        suite,
        executor,
        file_set,
        operation,
        concurrency,
        &run.to_string(),
        result.profile.requests_per_second,
        result.profile.wall_seconds,
        result.cpu_seconds,
        result.effective_cores,
        result.cpu_us_per_request,
        result.profile.threads_seen as f64,
        &result.profile,
        0.0,
        0.0,
    );
}

fn print_median(
    suite: &str,
    executor: &Executor,
    file_set: &str,
    operation: Operation,
    concurrency: usize,
    runs: &[TrialResult],
) {
    let profile = ProfileSummary {
        request_count: runs[0].profile.request_count,
        wall_seconds: median(runs.iter().map(|run| run.profile.wall_seconds).collect()),
        requests_per_second: median(
            runs.iter()
                .map(|run| run.profile.requests_per_second)
                .collect(),
        ),
        threads_seen: median(
            runs.iter()
                .map(|run| run.profile.threads_seen as f64)
                .collect(),
        ) as usize,
        queue_p50_us: field_median(runs, |run| run.profile.queue_p50_us),
        queue_p95_us: field_median(runs, |run| run.profile.queue_p95_us),
        queue_p99_us: field_median(runs, |run| run.profile.queue_p99_us),
        execution_p50_us: field_median(runs, |run| run.profile.execution_p50_us),
        execution_p95_us: field_median(runs, |run| run.profile.execution_p95_us),
        execution_p99_us: field_median(runs, |run| run.profile.execution_p99_us),
        completion_p50_us: field_median(runs, |run| run.profile.completion_p50_us),
        completion_p95_us: field_median(runs, |run| run.profile.completion_p95_us),
        completion_p99_us: field_median(runs, |run| run.profile.completion_p99_us),
        total_p50_us: field_median(runs, |run| run.profile.total_p50_us),
        total_p95_us: field_median(runs, |run| run.profile.total_p95_us),
        total_p99_us: field_median(runs, |run| run.profile.total_p99_us),
    };
    let cpu_seconds = field_median(runs, |run| run.cpu_seconds);
    let effective_cores = field_median(runs, |run| run.effective_cores);
    let cpu_us_per_request = field_median(runs, |run| run.cpu_us_per_request);
    let rps_values = runs
        .iter()
        .map(|run| run.profile.requests_per_second)
        .collect::<Vec<_>>();
    let p99_values = runs
        .iter()
        .map(|run| run.profile.total_p99_us)
        .collect::<Vec<_>>();

    print_row(
        "P3_MEDIAN",
        suite,
        executor,
        file_set,
        operation,
        concurrency,
        "median",
        profile.requests_per_second,
        profile.wall_seconds,
        cpu_seconds,
        effective_cores,
        cpu_us_per_request,
        profile.threads_seen as f64,
        &profile,
        range_percent(&rps_values, profile.requests_per_second),
        range_percent(&p99_values, profile.total_p99_us),
    );
}

#[allow(clippy::too_many_arguments)]
fn print_row(
    prefix: &str,
    suite: &str,
    executor: &Executor,
    file_set: &str,
    operation: Operation,
    concurrency: usize,
    run: &str,
    rps: f64,
    wall_seconds: f64,
    cpu_seconds: f64,
    effective_cores: f64,
    cpu_us_per_request: f64,
    threads_seen: f64,
    profile: &ProfileSummary,
    rps_range_pct: f64,
    p99_range_pct: f64,
) {
    println!(
        "{prefix},{suite},{},{},{file_set},{},{concurrency},{run},{rps:.3},{wall_seconds:.6},{cpu_seconds:.6},{effective_cores:.3},{cpu_us_per_request:.3},{threads_seen:.0},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{rps_range_pct:.3},{p99_range_pct:.3}",
        executor.name(),
        executor.configured_workers().unwrap_or(0),
        operation.name(),
        profile.queue_p50_us,
        profile.queue_p95_us,
        profile.queue_p99_us,
        profile.execution_p50_us,
        profile.execution_p95_us,
        profile.execution_p99_us,
        profile.completion_p50_us,
        profile.completion_p95_us,
        profile.completion_p99_us,
        profile.total_p50_us,
        profile.total_p95_us,
        profile.total_p99_us,
    );
}

fn field_median(runs: &[TrialResult], field: impl Fn(&TrialResult) -> f64) -> f64 {
    median(runs.iter().map(field).collect())
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn range_percent(values: &[f64], median: f64) -> f64 {
    let minimum = values.iter().copied().fold(f64::INFINITY, f64::min);
    let maximum = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    (maximum - minimum) * 100.0 / median
}

fn env_u64(name: &str, default: u64) -> io::Result<u64> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default);
    };
    let value = value.to_string_lossy().parse::<u64>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {name}: {error}"),
        )
    })?;
    if value == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must be greater than zero"),
        ));
    }
    Ok(value)
}

fn env_usize_list(name: &str, default: &[usize]) -> io::Result<Vec<usize>> {
    let Some(value) = std::env::var_os(name) else {
        return Ok(default.to_vec());
    };
    let values = value
        .to_string_lossy()
        .split(',')
        .map(|value| {
            value.trim().parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid {name} entry: {error}"),
                )
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    if values.is_empty() || values.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain positive comma-separated integers"),
        ));
    }
    Ok(values)
}

struct Fixture {
    directory: PathBuf,
    canonical_root: Arc<PathBuf>,
    same: Arc<Vec<PathBuf>>,
    four: Arc<Vec<PathBuf>>,
    sixteen: Arc<Vec<PathBuf>>,
    split_directories: Arc<Vec<PathBuf>>,
    file_size: usize,
}

impl Fixture {
    fn create() -> io::Result<Self> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("hamster-wheel-p3-{}-{unique}", std::process::id()));
        fs::create_dir(&directory)?;
        let contents = fs::read(Path::new("public").join("index.html"))?;

        let same = create_files(&directory.join("same"), 1, false, &contents)?;
        let four = create_files(&directory.join("four"), 4, false, &contents)?;
        let sixteen = create_files(&directory.join("sixteen"), 16, false, &contents)?;
        let split_directories = create_files(&directory.join("split"), 16, true, &contents)?;
        let canonical_root = Arc::new(fs::canonicalize(&directory)?);

        Ok(Self {
            directory,
            canonical_root,
            same: Arc::new(same),
            four: Arc::new(four),
            sixteen: Arc::new(sixteen),
            split_directories: Arc::new(split_directories),
            file_size: contents.len(),
        })
    }

    fn root(&self) -> Arc<PathBuf> {
        Arc::clone(&self.canonical_root)
    }

    fn same_file(&self) -> Arc<Vec<PathBuf>> {
        Arc::clone(&self.same)
    }

    fn file_sets(&self) -> [(&'static str, &Arc<Vec<PathBuf>>); 4] {
        [
            ("same_1", &self.same),
            ("round_robin_4", &self.four),
            ("round_robin_16", &self.sixteen),
            ("split_dirs_16", &self.split_directories),
        ]
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.directory.starts_with(std::env::temp_dir())
            && self
                .directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("hamster-wheel-p3-"))
        {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

fn create_files(
    root: &Path,
    count: usize,
    split: bool,
    contents: &[u8],
) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::with_capacity(count);
    for index in 0..count {
        let directory = if split {
            root.join(format!("directory-{index}"))
        } else {
            root.to_owned()
        };
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("file-{index}.html"));
        fs::write(&path, contents)?;
        paths.push(path);
    }
    Ok(paths)
}

#[cfg(windows)]
mod windows {
    use std::{ffi::c_void, io, mem::MaybeUninit};

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

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

    pub fn process_cpu_seconds() -> io::Result<f64> {
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

    fn file_time_ticks(time: FileTime) -> u64 {
        (u64::from(time.high) << 32) | u64::from(time.low)
    }
}

// The CPU column is a Windows measurement; elsewhere the bench still links and
// runs, and reports NaN for it, as benches/proxy.rs does.
#[cfg(not(windows))]
mod windows {
    use std::io;

    pub fn process_cpu_seconds() -> io::Result<f64> {
        Ok(f64::NAN)
    }
}

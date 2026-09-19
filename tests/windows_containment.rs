#![cfg(windows)]

use std::{
    ffi::c_void,
    fs::{self, File},
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    ptr,
    sync::{Mutex, OnceLock, atomic::AtomicU64, atomic::Ordering},
    thread,
    time::{Duration, Instant},
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PROCESS_TERMINATE: u32 = 0x0001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
const SYNCHRONIZE: u32 = 0x0010_0000;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 258;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
const RECLAIM_TIMEOUT: Duration = Duration::from_secs(5);

static TEST_LOCK: Mutex<()> = Mutex::new(());
static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(1);
static COMPATIBLE_OUTER_JOB: OnceLock<ProcessLifetimeHandle> = OnceLock::new();

#[derive(Debug)]
struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("containment-tests")
            .join(format!("{label}-{}-{id}", std::process::id()));
        fs::create_dir_all(&path).expect("create containment test directory");
        Self { path }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct ExactProcess {
    label: String,
    pid: u32,
    creation_time: u64,
    handle: OwnedHandle,
}

impl ExactProcess {
    fn open(label: impl Into<String>, pid: u32) -> Self {
        // SAFETY: OpenProcess returns either an owned handle or null. The
        // requested rights are limited to identity, waiting, and exact cleanup.
        let raw = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                pid,
            )
        };
        assert!(!raw.is_null(), "open exact process {pid}: {}", last_error());
        // SAFETY: ownership of the non-null process handle is transferred once.
        let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
        let creation_time = process_creation_time(handle.as_raw_handle());
        Self {
            label: label.into(),
            pid,
            creation_time,
            handle,
        }
    }

    fn wait_for_exit(&self, timeout: Duration) -> Option<Duration> {
        let started = Instant::now();
        // SAFETY: the process handle remains owned by self throughout the wait.
        let result = unsafe {
            WaitForSingleObject(
                self.handle.as_raw_handle(),
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };
        match result {
            WAIT_OBJECT_0 => Some(started.elapsed()),
            WAIT_TIMEOUT => None,
            other => panic!(
                "wait for exact process {} ({}) returned {other}: {}",
                self.pid,
                self.label,
                last_error()
            ),
        }
    }

    fn terminate_for_cleanup(&self) {
        if self.wait_for_exit(Duration::ZERO).is_some() {
            return;
        }
        // SAFETY: this is the exact retained process handle opened for the PID
        // created by this test, so PID reuse cannot redirect cleanup.
        unsafe {
            TerminateProcess(self.handle.as_raw_handle(), 0xC0DE);
        }
        let _ = self.wait_for_exit(RECLAIM_TIMEOUT);
    }
}

impl Drop for ExactProcess {
    fn drop(&mut self) {
        self.terminate_for_cleanup();
    }
}

#[derive(Debug)]
struct ServerProcess {
    child: Child,
    pid: u32,
    creation_time: u64,
    stdout: PathBuf,
    stderr: PathBuf,
    tracked: Vec<ExactProcess>,
}

impl ServerProcess {
    fn spawn(config: &Path, files: &TestDirectory) -> Self {
        let stdout_path = files.file("server.stdout");
        let stderr_path = files.file("server.stderr");
        let stdout = File::create(&stdout_path).expect("create server stdout capture");
        let stderr = File::create(&stderr_path).expect("create server stderr capture");
        let mut command = Command::new(env!("CARGO_BIN_EXE_hamster-wheel"));
        command
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .creation_flags(CREATE_NO_WINDOW);
        let child = command
            .spawn()
            .expect("spawn Hamster containment test server");
        let pid = child.id();
        let creation_time = process_creation_time(child.as_raw_handle());
        Self {
            child,
            pid,
            creation_time,
            stdout: stdout_path,
            stderr: stderr_path,
            tracked: Vec::new(),
        }
    }

    fn track(&mut self, label: impl Into<String>, pid: u32) {
        self.tracked.push(ExactProcess::open(label, pid));
    }

    fn wait_for_stdout(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let output = fs::read_to_string(&self.stdout).unwrap_or_default();
            if output.contains(needle) {
                return;
            }
            if let Some(status) = self.child.try_wait().expect("inspect Hamster process") {
                panic!(
                    "Hamster exited before {needle:?}: status={status}; stdout={output:?}; stderr={:?}",
                    fs::read_to_string(&self.stderr).unwrap_or_default()
                );
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn terminate_abnormally(&mut self) {
        self.child
            .kill()
            .expect("force-terminate exact Hamster process");
        let status = self.child.wait().expect("wait for terminated Hamster");
        for process in &self.tracked {
            let elapsed = process.wait_for_exit(RECLAIM_TIMEOUT);
            eprintln!(
                "containment evidence: hamster_pid={} hamster_creation={} mechanism=TerminateProcess label={} pid={} creation={} exited={} reclaim_ms={}",
                self.pid,
                self.creation_time,
                process.label,
                process.pid,
                process.creation_time,
                elapsed.is_some(),
                elapsed.map_or(RECLAIM_TIMEOUT.as_millis(), |value| value.as_millis()),
            );
            assert!(
                elapsed.is_some(),
                "{} PID {} survived abnormal Hamster termination; parent status {status}",
                process.label,
                process.pid
            );
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Debug)]
struct PoolSpec<'a> {
    host: &'a str,
    script: &'a Path,
    workers: usize,
    max_requests: Option<u64>,
    env: Vec<(&'a str, PathBuf)>,
}

fn write_config(files: &TestDirectory, address: SocketAddr, pools: &[PoolSpec<'_>]) -> PathBuf {
    let php = Path::new(env!("CARGO_MANIFEST_DIR")).join("php/8.5/php.exe");
    assert!(php.is_file(), "containment tests require {}", php.display());
    let mut source = format!("[server]\nlisten = \"{address}\"\n");
    for pool in pools {
        source.push_str(&format!(
            "\n[[hosts]]\nname = \"{}\"\n\n[[hosts.locations]]\npath = \"/\"\nhandler = \"php_worker\"\n\n[hosts.locations.php]\nexecutable = {}\nscript = {}\nworkers = {}\nqueue_capacity = 8\nrequest_timeout = 30\n",
            pool.host,
            toml_literal(&php),
            toml_literal(pool.script),
            pool.workers,
        ));
        if let Some(max_requests) = pool.max_requests {
            source.push_str(&format!("max_requests = {max_requests}\n"));
        }
        if !pool.env.is_empty() {
            source.push_str("\n[hosts.locations.php.env]\n");
            for (name, value) in &pool.env {
                source.push_str(&format!("{name} = {}\n", toml_literal(value)));
            }
        }
    }
    let config = files.file("hamster.toml");
    fs::write(&config, source).expect("write containment config");
    config
}

fn reserve_address() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve test port");
    listener.local_addr().expect("reserved test address")
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/php-worker")
        .join(name)
}

fn toml_literal(path: &Path) -> String {
    format!("'{}'", path.display())
}

fn wait_for_pids(path: &Path, count: usize, server: &mut ServerProcess) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let pids = fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect::<Vec<_>>();
        if pids.len() >= count {
            return pids;
        }
        if let Some(status) = server.child.try_wait().expect("inspect Hamster process") {
            panic!(
                "Hamster exited while waiting for PID marker: {status}; stderr={:?}",
                fs::read_to_string(&server.stderr).unwrap_or_default()
            );
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for PID marker"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn request(address: SocketAddr, host: &str, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(address).expect("connect to Hamster");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set request timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    response
}

fn url_encode_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[test]
#[ignore = "requires repository php/8.5/php.exe"]
fn abnormal_parent_reclaims_busy_php_and_ordinary_descendant() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let files = TestDirectory::new("busy-descendant");
    let worker_marker = files.file("worker.pid");
    let descendant_marker = files.file("descendant.pid");
    let started_marker = files.file("started.pid");
    let powershell = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let config = write_config(
        &files,
        reserve_address(),
        &[PoolSpec {
            host: "descendant.test",
            script: &fixture("containment_descendant_worker.php"),
            workers: 1,
            max_requests: None,
            env: vec![
                ("HAMSTER_TEST_PID_FILE", worker_marker.clone()),
                (
                    "HAMSTER_TEST_DESCENDANT_PID_FILE",
                    descendant_marker.clone(),
                ),
                ("HAMSTER_TEST_DESCENDANT_EXECUTABLE", powershell),
            ],
        }],
    );
    let address = config_address(&config);
    let mut server = ServerProcess::spawn(&config, &files);
    server.wait_for_stdout("Hamster Wheel listening");
    let worker_pid = wait_for_pids(&worker_marker, 1, &mut server)[0];
    let descendant_pid = wait_for_pids(&descendant_marker, 1, &mut server)[0];
    server.track("pool=descendant.test slot=0 generation=1 PHP", worker_pid);
    server.track("PHP-created ordinary descendant", descendant_pid);

    let request_path = format!(
        "/delay?ms=10000&start_marker={}",
        url_encode_path(&started_marker)
    );
    let client = thread::spawn(move || request(address, "descendant.test", &request_path));
    let started_pid = wait_for_file_pid(&started_marker, &mut server);
    assert_eq!(started_pid, worker_pid);
    server.terminate_abnormally();
    let _ = client.join();
}

#[test]
#[ignore = "requires repository php/8.5/php.exe"]
fn abnormal_parent_reclaims_bootstrapping_php() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let files = TestDirectory::new("bootstrap");
    let marker = files.file("bootstrap.pid");
    let address = reserve_address();
    let config = write_config(
        &files,
        address,
        &[PoolSpec {
            host: "bootstrap.test",
            script: &fixture("containment_slow_ready.php"),
            workers: 1,
            max_requests: None,
            env: vec![("HAMSTER_TEST_PID_FILE", marker.clone())],
        }],
    );
    let mut server = ServerProcess::spawn(&config, &files);
    let pid = wait_for_pids(&marker, 1, &mut server)[0];
    server.track("pool=bootstrap.test slot=0 generation=1 BOOTSTRAPPING", pid);
    server.terminate_abnormally();
    assert!(
        TcpStream::connect(address).is_err(),
        "listener bound before PHP READY"
    );
}

#[test]
#[ignore = "requires repository php/8.5/php.exe"]
fn abnormal_parent_reclaims_replacement_generation_and_multiple_pools() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let files = TestDirectory::new("replacement-multipool");
    let first_marker = files.file("first-pool.pids");
    let second_marker = files.file("second-pool.pids");
    let address = reserve_address();
    let record = fixture("record_pid_worker.php");
    let config = write_config(
        &files,
        address,
        &[
            PoolSpec {
                host: "recycle.test",
                script: &record,
                workers: 1,
                max_requests: Some(1),
                env: vec![("HAMSTER_TEST_PID_FILE", first_marker.clone())],
            },
            PoolSpec {
                host: "stable.test",
                script: &record,
                workers: 1,
                max_requests: None,
                env: vec![("HAMSTER_TEST_PID_FILE", second_marker.clone())],
            },
        ],
    );
    let mut server = ServerProcess::spawn(&config, &files);
    server.wait_for_stdout("Hamster Wheel listening");
    let first_generation = wait_for_pids(&first_marker, 1, &mut server)[0];
    let stable_pid = wait_for_pids(&second_marker, 1, &mut server)[0];
    server.track(
        "pool=recycle.test slot=0 generation=1 initial",
        first_generation,
    );
    let response = request(address, "recycle.test", "/state");
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let replacement_pid = wait_for_pids(&first_marker, 2, &mut server)[1];
    assert_ne!(first_generation, replacement_pid);
    server.track(
        "pool=recycle.test slot=0 generation=2 replacement",
        replacement_pid,
    );
    server.track("pool=stable.test slot=0 generation=1", stable_pid);
    server.terminate_abnormally();
}

#[test]
#[ignore = "requires repository php/8.5/php.exe"]
fn compatible_outer_job_allows_inner_hamster_containment() {
    let _serial = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    establish_compatible_outer_job();
    let files = TestDirectory::new("nested-job");
    let marker = files.file("worker.pid");
    let address = reserve_address();
    let config = write_config(
        &files,
        address,
        &[PoolSpec {
            host: "nested.test",
            script: &fixture("record_pid_worker.php"),
            workers: 1,
            max_requests: None,
            env: vec![("HAMSTER_TEST_PID_FILE", marker.clone())],
        }],
    );
    let mut server = ServerProcess::spawn(&config, &files);
    server.wait_for_stdout("Hamster Wheel listening");
    let pid = wait_for_pids(&marker, 1, &mut server)[0];
    server.track("compatible outer job / inner Hamster job PHP", pid);
    server.terminate_abnormally();
}

fn wait_for_file_pid(path: &Path, server: &mut ServerProcess) -> u32 {
    wait_for_pids(path, 1, server)[0]
}

fn config_address(config: &Path) -> SocketAddr {
    let source = fs::read_to_string(config).expect("read generated config");
    let listen = source
        .lines()
        .find_map(|line| line.strip_prefix("listen = \"")?.strip_suffix('"'))
        .expect("generated listen address");
    listen.parse().expect("parse generated listen address")
}

#[derive(Debug)]
struct ProcessLifetimeHandle {
    _handle: OwnedHandle,
}

fn establish_compatible_outer_job() {
    COMPATIBLE_OUTER_JOB.get_or_init(|| {
        // SAFETY: the private job starts empty and the returned handle is
        // transferred exactly once to OwnedHandle.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        assert!(
            !raw.is_null(),
            "create compatible outer job: {}",
            last_error()
        );
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: exact C-layout buffer, valid handles, synchronous calls.
        assert_ne!(
            unsafe {
                SetInformationJobObject(
                    job.as_raw_handle(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                    ptr::from_ref(&limits).cast(),
                    size_of::<JobObjectExtendedLimitInformation>() as u32,
                )
            },
            0,
            "configure compatible outer job: {}",
            last_error()
        );
        assert_ne!(
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) },
            0,
            "assign containment test process to compatible outer job: {}",
            last_error()
        );
        ProcessLifetimeHandle { _handle: job }
    });
}

fn process_creation_time(handle: RawHandle) -> u64 {
    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    // SAFETY: all FILETIME output pointers are valid for this synchronous call.
    assert_ne!(
        unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) },
        0,
        "query process creation time: {}",
        last_error()
    );
    (u64::from(creation.high_date_time) << 32) | u64::from(creation.low_date_time)
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

#[repr(C)]
#[derive(Debug, Default)]
struct FileTime {
    low_date_time: u32,
    high_date_time: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct IoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[repr(C)]
#[derive(Debug, Default)]
struct JobObjectBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[repr(C)]
#[derive(Debug, Default)]
struct JobObjectExtendedLimitInformation {
    basic_limit_information: JobObjectBasicLimitInformation,
    io_info: IoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit_handle: i32, process_id: u32) -> RawHandle;
    fn WaitForSingleObject(handle: RawHandle, milliseconds: u32) -> u32;
    fn TerminateProcess(process: RawHandle, exit_code: u32) -> i32;
    fn GetProcessTimes(
        process: RawHandle,
        creation_time: *mut FileTime,
        exit_time: *mut FileTime,
        kernel_time: *mut FileTime,
        user_time: *mut FileTime,
    ) -> i32;
    fn CreateJobObjectW(job_attributes: *const c_void, name: *const u16) -> RawHandle;
    fn SetInformationJobObject(
        job: RawHandle,
        information_class: i32,
        information: *const c_void,
        information_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
    fn GetCurrentProcess() -> RawHandle;
}

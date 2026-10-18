//! Shared by integration tests that start processes of their own.

/// Makes every process this test binary starts from now on die with it.
///
/// Tests that run the real server binary in a new console own it only through
/// `kill_on_drop`, which runs when the test unwinds but not when the test
/// process itself is ended: a timeout, a stop from the terminal, or a killed
/// `cargo test`. The server has its own console, so Ctrl+C in the test's
/// terminal does not reach it either, and it survived with its PHP workers.
/// Joining this process to a kill-on-close Job Object makes the kernel end
/// all descendants when the last handle closes, which is when this process
/// exits for any reason. Descendants that create their own jobs (the server
/// does, for its workers) nest inside this one.
///
/// Idempotent; the job handle is deliberately kept for the process lifetime.
#[cfg(windows)]
pub fn end_children_with_this_process() {
    use std::{ffi::c_void, sync::OnceLock};

    #[repr(C)]
    #[derive(Default)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct BasicLimits {
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
    #[derive(Default)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> isize;
        fn SetInformationJobObject(
            job: isize,
            class: i32,
            information: *const c_void,
            length: u32,
        ) -> i32;
        fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
        fn GetCurrentProcess() -> isize;
    }

    static JOB: OnceLock<isize> = OnceLock::new();
    JOB.get_or_init(|| {
        // SAFETY: plain Win32 calls on a handle this function owns; every
        // result is checked.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            assert_ne!(job, 0, "create a test Job Object");
            let mut limits = ExtendedLimits::default();
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            assert_ne!(
                SetInformationJobObject(
                    job,
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    std::ptr::from_ref(&limits).cast(),
                    size_of::<ExtendedLimits>() as u32,
                ),
                0,
                "configure the test Job Object"
            );
            assert_ne!(
                AssignProcessToJobObject(job, GetCurrentProcess()),
                0,
                "join the test process to its Job Object"
            );
            job
        }
    });
}

/// Linux and macOS have no Job Object to join. Children still die through
/// `kill_on_drop` when the test unwinds; a test process ended by a signal
/// leaves them to be reparented, as any other process's children would be.
#[cfg(not(windows))]
#[allow(dead_code, reason = "some test binaries call it only on Windows")]
pub fn end_children_with_this_process() {}

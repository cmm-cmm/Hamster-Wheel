//! Windows abnormal-parent containment for PHP application processes.
//!
//! Hamster joins this process-lifetime job before any PHP worker can start.
//! Ordinary children then inherit membership from Hamster, including worker
//! replacement generations and their direct process descendants.

use std::{
    ffi::c_void,
    fmt, io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    ptr,
    sync::{Arc, OnceLock},
};

const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;

static PROCESS_CONTAINMENT: OnceLock<Result<ProcessLifetimeJob, Arc<ContainmentError>>> =
    OnceLock::new();

#[cfg(test)]
static FORCE_SETUP_FAILURE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[derive(Debug)]
pub struct ContainmentError {
    operation: &'static str,
    source: io::Error,
}

impl ContainmentError {
    fn last_os_error(operation: &'static str) -> Self {
        Self {
            operation,
            source: io::Error::last_os_error(),
        }
    }

    #[cfg(test)]
    fn injected() -> Self {
        Self {
            operation: "establish Windows PHP child containment",
            source: io::Error::other("injected containment setup failure"),
        }
    }
}

impl fmt::Display for ContainmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.operation, self.source)
    }
}

impl std::error::Error for ContainmentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Owns the only Hamster-created handle to the unnamed containment job.
///
/// This value intentionally lives in a static `OnceLock`. Rust does not run
/// destructors for statics at process exit, so the handle cannot be dropped
/// during normal server lifetime. Windows closes it while tearing the process
/// down. Closing it earlier would be incorrect because Hamster itself is a job
/// member and `KILL_ON_JOB_CLOSE` would terminate Hamster along with its
/// children.
#[derive(Debug)]
struct ProcessLifetimeJob {
    _handle: OwnedHandle,
}

impl ProcessLifetimeJob {
    fn establish() -> Result<Self, ContainmentError> {
        // SAFETY: null security attributes and name create a private unnamed
        // job with a non-inheritable handle. A null result is reported through
        // GetLastError.
        let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw_job.is_null() {
            return Err(ContainmentError::last_os_error(
                "create Windows PHP containment job",
            ));
        }

        // SAFETY: `raw_job` is a newly owned, non-null job handle and is
        // transferred exactly once to `OwnedHandle`.
        let job = unsafe { OwnedHandle::from_raw_handle(raw_job) };
        let mut limits = JobObjectExtendedLimitInformation::default();
        limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // Configure the complete policy before assigning Hamster. If this
        // fails, dropping `job` is safe because the process is not a member.
        // SAFETY: the buffer has the exact C layout and remains valid for the
        // duration of the synchronous call.
        let configured = unsafe {
            SetInformationJobObject(
                job.as_raw_handle(),
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                ptr::from_ref(&limits).cast(),
                size_of::<JobObjectExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            return Err(ContainmentError::last_os_error(
                "configure KILL_ON_JOB_CLOSE for Windows PHP containment job",
            ));
        }

        // This is deliberately the final fallible operation. Once Hamster is
        // a member, dropping the final job handle would terminate this process.
        // SAFETY: both handles are valid for this synchronous call; the current
        // process pseudo-handle must not and will not be closed.
        let assigned =
            unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) };
        if assigned == 0 {
            return Err(ContainmentError::last_os_error(
                "assign Hamster to Windows PHP containment job",
            ));
        }

        Ok(Self { _handle: job })
    }
}

pub(crate) fn establish() -> Result<(), Arc<ContainmentError>> {
    #[cfg(test)]
    if FORCE_SETUP_FAILURE.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(Arc::new(ContainmentError::injected()));
    }

    match PROCESS_CONTAINMENT.get_or_init(|| ProcessLifetimeJob::establish().map_err(Arc::new)) {
        Ok(_) => Ok(()),
        Err(error) => Err(Arc::clone(error)),
    }
}

#[cfg(test)]
pub(crate) fn force_setup_failure(enabled: bool) {
    FORCE_SETUP_FAILURE.store(enabled, std::sync::atomic::Ordering::SeqCst);
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

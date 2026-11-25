use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    ptr,
};
use tokio::process::Child;

const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
const ERROR_ACCESS_DENIED: i32 = 5;

// The Win32 ABI layouts let this crate use job objects with its existing locked dependencies.
#[repr(C)]
#[derive(Default)]
struct BasicLimitInformation {
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
struct ExtendedLimitInformation {
    basic_limit_information: BasicLimitInformation,
    io_counters: [u64; 6],
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> RawHandle;
    fn SetInformationJobObject(job: RawHandle, class: i32, info: *const c_void, len: u32) -> i32;
    fn AssignProcessToJobObject(job: RawHandle, process: RawHandle) -> i32;
}

pub(super) fn create() -> io::Result<OwnedHandle> {
    // A null security descriptor creates a non-inheritable handle.
    let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // CreateJobObjectW returned a new handle owned by this process.
    let job = unsafe { OwnedHandle::from_raw_handle(handle) };
    let limits = ExtendedLimitInformation {
        basic_limit_information: BasicLimitInformation {
            limit_flags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        },
        ..Default::default()
    };
    let configured = unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            ptr::from_ref(&limits).cast(),
            size_of::<ExtendedLimitInformation>() as u32,
        )
    };
    if configured == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

pub(super) fn assign(job: OwnedHandle, child: &Child) -> io::Result<Option<OwnedHandle>> {
    let Some(process) = child.raw_handle() else {
        return Ok(None);
    };
    // Both handles remain valid across the call.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } != 0 {
        return Ok(Some(job));
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) {
        // An enclosing job may forbid nesting. Tokio still owns the immediate child.
        tracing::debug!(%error, "could not put sidecar in a Windows job object");
        Ok(None)
    } else {
        Err(error)
    }
}

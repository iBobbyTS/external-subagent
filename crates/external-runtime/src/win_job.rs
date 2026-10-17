//! Win32 Job Object process supervision (Windows only).
//!
//! Windows has no process groups, so a supervised "group" is exactly one Job
//! Object and `pgid == pid` by construction. Every process created through
//! [`crate::gated_spawn`] is started suspended, bound to a fresh Job Object
//! configured with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, and only then resumed.
//! That ordering (SUSPENDED -> CreateJobObject -> AssignProcessToJobObject ->
//! ResumeThread) is what makes binding race-free: a descendant cannot be
//! created before the binding is in place, and closing the job handle reaps the
//! whole tree atomically, which is what turns daemon death into a cascade kill.
//!
//! Job handles live in a process-global registry keyed by the owning pid
//! (`pid -> HANDLE`). The registry is the authoritative source for
//! `observe_process_group`; a missing handle means the group is gone (the job
//! was closed, which already killed every member).
use std::{
    collections::HashMap,
    io,
    os::windows::io::AsRawHandle,
    process::{Child, Command},
    sync::{LazyLock, Mutex},
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, HANDLE, INVALID_HANDLE_VALUE, ERROR_ACCESS_DENIED,
        ERROR_BROKEN_PIPE, ERROR_INVALID_HANDLE, ERROR_INVALID_PARAMETER, ERROR_MORE_DATA,
        ERROR_NO_DATA, ERROR_PIPE_NOT_CONNECTED,
    },
    Storage::FileSystem::WriteFile,
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, QueryInformationJobObject,
            SetInformationJobObject, TerminateJobObject, JobObjectBasicProcessIdList,
            JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_PROCESS_ID_LIST,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::{SetNamedPipeHandleState, PIPE_NOWAIT},
        Threading::{
            GetProcessTimes, OpenProcess, OpenThread, ResumeThread, TerminateProcess,
            CREATE_SUSPENDED, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_QUOTA,
            PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
        },
    },
};

/// `pid -> job HANDLE` (stored as `isize` so the map stays `Send`).
static REGISTRY: LazyLock<Mutex<HashMap<u32, isize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn registry() -> std::sync::MutexGuard<'static, HashMap<u32, isize>> {
    REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn raw_os(error: &io::Error) -> Option<u32> {
    error.raw_os_error().map(|code| code as u32)
}

fn missing_process(error: &io::Error) -> bool {
    matches!(
        raw_os(error),
        Some(ERROR_INVALID_PARAMETER) | Some(ERROR_INVALID_HANDLE)
    )
}

/// Bind a freshly created (suspended) process to a new kill-on-close job and
/// resume it, returning the job handle. The child must have been spawned with
/// `CREATE_SUSPENDED`; on any failure the job is closed (which kills the
/// suspended child) before the error is returned.
fn bind_job(pid: u32) -> io::Result<isize> {
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }

        let process = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid,
        );
        let process = match process.is_null() {
            true => {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(error);
            }
            false => process,
        };
        let assigned = AssignProcessToJobObject(job, process);
        CloseHandle(process);
        if assigned == 0 {
            let error = io::Error::last_os_error();
            CloseHandle(job);
            return Err(error);
        }

        if let Err(error) = resume_primary_thread(pid) {
            // The child is still suspended and already inside the job; closing
            // the job is the deterministic cleanup.
            CloseHandle(job);
            return Err(error);
        }
        Ok(job as isize)
    }
}

/// Resume the primary thread of a process started with `CREATE_SUSPENDED`.
///
/// std's `Child` only exposes the process handle and closes the thread handle
/// returned by `CreateProcessW`, so the primary thread is located through a
/// system thread snapshot. A newly created suspended process has exactly one
/// thread, so the first entry owned by `pid` is the primary thread.
fn resume_primary_thread(pid: u32) -> io::Result<()> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut primary = 0u32;
        if Thread32First(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32OwnerProcessID == pid {
                    primary = entry.th32ThreadID;
                    break;
                }
                if Thread32Next(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        if primary == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "suspended child has no primary thread in the snapshot",
            ));
        }
        let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, primary);
        if thread.is_null() {
            return Err(io::Error::last_os_error());
        }
        let previous = ResumeThread(thread);
        CloseHandle(thread);
        if previous == u32::MAX {
            return Err(io::Error::last_os_error());
        }
        if previous == 0 {
            return Err(io::Error::other(
                "child primary thread was not suspended before resume",
            ));
        }
        Ok(())
    }
}

/// Spawn `command` under a new kill-on-close Job Object. Holds the shared spawn
/// gate for the whole suspended-create/bind/resume sequence.
pub(super) fn spawn_supervised(command: &mut Command) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    let _gate = super::SPAWN_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    command.creation_flags(CREATE_SUSPENDED);
    let mut child = command.spawn()?;
    let pid = child.id();
    match bind_job(pid) {
        Ok(job) => {
            registry().insert(pid, job);
            Ok(child)
        }
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(error)
        }
    }
}

fn filetime_to_u64(time: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

pub(super) fn observe(pid: u32) -> io::Result<super::ProcessIdentity> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if missing_process(&error) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("process {pid} does not exist"),
                ));
            }
            return Err(error);
        }
        let mut creation: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut exit: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut kernel: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
        let mut user: windows_sys::Win32::Foundation::FILETIME = std::mem::zeroed();
        let ok = GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user);
        let close = CloseHandle(handle);
        if ok == 0 {
            let error = io::Error::last_os_error();
            if missing_process(&error) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("process {pid} vanished during observation"),
                ));
            }
            return Err(error);
        }
        if close == 0 {
            // The identity is already read; a failed handle close is a leak,
            // not a correctness failure, but surface it as a real error.
            return Err(io::Error::last_os_error());
        }
        let token = filetime_to_u64(creation);
        if token == 0 {
            return Err(io::Error::other("incomplete Windows process identity"));
        }
        Ok(super::ProcessIdentity {
            pid,
            pgid: pid as i32,
            uid: WINDOWS_GROUP_UID,
            start_token: format!("windows:creation_time:{token}"),
        })
    }
}

/// Every member of a supervised group shares this synthetic uid so the shared
/// group validation (`member.uid == leader.uid`) is always satisfied on
/// Windows, where there is no per-process group membership to read.
pub(super) const WINDOWS_GROUP_UID: u32 = 0;

pub(super) fn job_handle(pid: u32) -> Option<isize> {
    registry().get(&pid).copied()
}

fn query_job_pids(job: HANDLE) -> io::Result<Vec<u32>> {
    let header = std::mem::size_of::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
    let stride = std::mem::size_of::<usize>();
    let mut capacity = 64usize;
    loop {
        let bytes = header + capacity.saturating_sub(1) * stride;
        let mut buffer = vec![0u8; bytes];
        let mut returned = 0u32;
        let ok = unsafe {
            QueryInformationJobObject(
                job,
                JobObjectBasicProcessIdList,
                buffer.as_mut_ptr().cast(),
                bytes as u32,
                &mut returned,
            )
        };
        if ok != 0 {
            let list = buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>();
            let count = unsafe { (*list).NumberOfProcessIdsInList } as usize;
            let count = count.min(capacity);
            let ids = unsafe { std::slice::from_raw_parts((*list).ProcessIdList.as_ptr(), count) };
            return Ok(ids.iter().map(|value| *value as u32).collect());
        }
        let error = io::Error::last_os_error();
        if raw_os(&error) == Some(ERROR_MORE_DATA) && returned as usize > header {
            let needed = (returned as usize - header) / stride + 1;
            capacity = needed.max(capacity * 2).min(1 << 20);
            continue;
        }
        return Err(error);
    }
}

pub(super) fn observe_group(pgid: i32) -> io::Result<Vec<super::ProcessIdentity>> {
    let Some(job) = job_handle(pgid as u32) else {
        // No job handle: the job was closed (which already reaped every member)
        // or the daemon restarted, so there is no live process to report. This
        // matches the "group is gone" arm every consumer keys on.
        return Ok(Vec::new());
    };
    let pids = query_job_pids(job as HANDLE)?;
    let mut members = Vec::with_capacity(pids.len());
    for pid in pids {
        match observe(pid) {
            Ok(mut member) => {
                // Windows has no process groups: every supervised member is
                // reported under the group's leader pid so group validation and
                // signalling stay pgid-keyed exactly like the Unix arms.
                member.pgid = pgid;
                members.push(member);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    members.sort_by_key(|member| member.pid);
    Ok(members)
}

/// Terminate every member of the supervised group. `TerminateJobObject` is the
/// Windows equivalent of a group kill; an already-closed job (no handle) is
/// treated as success, mirroring ESRCH tolerance.
pub(super) fn terminate_group(pgid: i32) -> io::Result<()> {
    if pgid <= 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process-group id must be positive",
        ));
    }
    let Some(job) = job_handle(pgid as u32) else {
        return Ok(());
    };
    unsafe {
        if TerminateJobObject(job as HANDLE, 1) == 0 {
            let error = io::Error::last_os_error();
            if matches!(
                raw_os(&error),
                Some(ERROR_ACCESS_DENIED) | Some(ERROR_INVALID_HANDLE)
            ) {
                return Ok(());
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Terminate a single process, tolerating a target that has already exited.
pub(super) fn terminate_process(pid: u32) -> io::Result<()> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "process id must be a positive signal-safe pid",
        ));
    }
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            let error = io::Error::last_os_error();
            if missing_process(&error) || raw_os(&error) == Some(ERROR_ACCESS_DENIED) {
                return Ok(());
            }
            return Err(error);
        }
        let terminated = TerminateProcess(handle, 1);
        let close = CloseHandle(handle);
        if terminated == 0 {
            let error = io::Error::last_os_error();
            if matches!(
                raw_os(&error),
                Some(ERROR_ACCESS_DENIED) | Some(ERROR_INVALID_HANDLE)
            ) {
                return Ok(());
            }
            return Err(error);
        }
        if close == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Remove a supervised group from the registry and close its job handle. With
/// `KILL_ON_JOB_CLOSE` this atomically reaps any surviving members, so it is the
/// deterministic cleanup the daemon applies to catalog probe processes.
pub(super) fn release(pgid: i32) -> io::Result<()> {
    if pgid <= 0 {
        return Ok(());
    }
    let handle = registry().remove(&(pgid as u32));
    if let Some(handle) = handle {
        if unsafe { CloseHandle(handle as HANDLE) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// A named-pipe handle temporarily switched to `PIPE_NOWAIT` so writes cannot
/// block the driver thread. Restores blocking mode on drop.
pub(super) struct NonblockingPipe {
    handle: HANDLE,
}

impl NonblockingPipe {
    pub(super) fn install(handle: HANDLE) -> io::Result<Self> {
        let mode = PIPE_NOWAIT;
        if unsafe { SetNamedPipeHandleState(handle, &mode, std::ptr::null(), std::ptr::null()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    pub(super) fn restore(&mut self) -> io::Result<()> {
        // PIPE_WAIT is 0; byte mode is 0. Restoring to 0 returns the handle to
        // the blocking byte-stream state it had before install().
        let mode = 0u32;
        if unsafe { SetNamedPipeHandleState(self.handle, &mode, std::ptr::null(), std::ptr::null()) } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for NonblockingPipe {
    fn drop(&mut self) {
        let mode = 0u32;
        unsafe {
            SetNamedPipeHandleState(self.handle, &mode, std::ptr::null(), std::ptr::null());
        }
    }
}

fn wait_before(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "runtime write deadline elapsed",
        ));
    }
    Ok(())
}

/// Write every byte of `frame` to `handle` (a `PIPE_NOWAIT` pipe) or fail with
/// `TimedOut` once `deadline` passes. Never blocks indefinitely.
pub(super) fn write_all_before(handle: HANDLE, frame: &[u8], deadline: Instant) -> io::Result<()> {
    let mut offset = 0usize;
    while offset < frame.len() {
        wait_before(deadline)?;
        let chunk = &frame[offset..];
        let len = chunk.len().min(u32::MAX as usize) as u32;
        let mut written = 0u32;
        let ok = unsafe { WriteFile(handle, chunk.as_ptr(), len, &mut written, std::ptr::null_mut()) };
        if ok != 0 {
            if written == 0 {
                // Nonblocking pipe buffer full: wait for the reader to drain.
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            offset += written as usize;
            continue;
        }
        let error = io::Error::last_os_error();
        match raw_os(&error) {
            // Full buffer under PIPE_NOWAIT.
            Some(ERROR_NO_DATA) => thread::sleep(Duration::from_millis(1)),
            Some(ERROR_BROKEN_PIPE) | Some(ERROR_PIPE_NOT_CONNECTED) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "runtime stdin pipe is closed",
                ))
            }
            _ => return Err(error),
        }
    }
    Ok(())
}

/// Raw pipe handle for a `std` child stdin, plus the with-handle helper used by
/// `Driver::write_frame_before` on Windows.
pub(super) fn stdin_handle(input: &std::process::ChildStdin) -> HANDLE {
    input.as_raw_handle() as HANDLE
}

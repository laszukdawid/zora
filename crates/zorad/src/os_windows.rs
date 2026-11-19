//! Process identity and job containment on Windows. A Job Object with `KILL_ON_JOB_CLOSE` stands in for a Unix
//! process group: whatever the harness starts joins its job, and when the last handle closes (the guard exits or
//! dies) Windows kills the whole job. Job names come from the job id, so recovery finds them without trusting pids.

use crate::guard::JobSpec;
use std::ffi::c_void;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, FILETIME, GetLastError,
    HANDLE_FLAG_INHERIT, STILL_ACTIVE, SetHandleInformation,
};
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, GetExitCodeProcess,
    GetProcessTimes, OpenProcess, OpenThread, PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread,
    THREAD_SUSPEND_RESUME,
};

// Job object access rights (winnt.h).
const JOB_OBJECT_QUERY: u32 = 4;
const JOB_OBJECT_TERMINATE: u32 = 8;

/// Empty: named jobs disappear at reboot, so recovery needs no boot identity to rule out stale processes.
pub fn boot_id() -> String {
    String::new()
}

fn owned(h: *mut c_void) -> Option<OwnedHandle> {
    // SAFETY: a non-null handle just returned to us, owned by nobody else.
    (!h.is_null() && h as isize != -1).then(|| unsafe { OwnedHandle::from_raw_handle(h) })
}

/// Creation time (100 ns units since 1601) of a running process.
pub fn start_time(pid: i32) -> Option<i64> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: plain query calls on a handle we own.
    unsafe {
        let h = owned(OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            0,
            pid as u32,
        ))?;
        let mut code = 0u32;
        if GetExitCodeProcess(h.as_raw_handle(), &mut code) == 0 || code != STILL_ACTIVE as u32 {
            return None;
        }
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        if GetProcessTimes(
            h.as_raw_handle(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        ) == 0
        {
            return None;
        }
        Some(((created.dwHighDateTime as i64) << 32) | created.dwLowDateTime as i64)
    }
}

pub fn exit_signal(_status: &ExitStatus) -> Option<i32> {
    None
}

unsafe extern "system" fn ignore_ctrl(_event: u32) -> i32 {
    1
}

/// The guard ignores Ctrl+C, Ctrl+Break and console close, so it can always reap its job.
pub fn ignore_termination_signals() {
    // SAFETY: registers a handler that only returns TRUE.
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn job_name(id: &str) -> String {
    format!("Local\\zora-job-{id}")
}

struct Job(OwnedHandle);

impl Job {
    /// A new job whose processes all die when its last handle closes.
    fn create(name: Option<&str>) -> io::Result<Job> {
        let name = name.map(wide);
        // SAFETY: the name is NUL-terminated and outlives the call; the info struct is fully initialized.
        unsafe {
            let h = CreateJobObjectW(
                std::ptr::null(),
                name.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
            );
            let existed = GetLastError() == ERROR_ALREADY_EXISTS;
            let job = Job(owned(h).ok_or_else(io::Error::last_os_error)?);
            if existed {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "job object already exists",
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            if SetInformationJobObject(
                job.0.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    /// The named job, or `None` once it is gone (all its processes have exited).
    fn open(name: &str) -> io::Result<Option<Job>> {
        let name = wide(name);
        // SAFETY: the name is NUL-terminated and outlives the call.
        let h =
            unsafe { OpenJobObjectW(JOB_OBJECT_QUERY | JOB_OBJECT_TERMINATE, 0, name.as_ptr()) };
        match owned(h) {
            Some(h) => Ok(Some(Job(h))),
            None => {
                let e = io::Error::last_os_error();
                match e.raw_os_error().map(|c| c as u32) {
                    Some(ERROR_FILE_NOT_FOUND) | Some(ERROR_PATH_NOT_FOUND) => Ok(None),
                    _ => Err(e),
                }
            }
        }
    }

    fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call.
        if unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), process) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn empty(&self) -> bool {
        // SAFETY: the info struct is a valid, writable buffer of the given size.
        unsafe {
            let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
            QueryInformationJobObject(
                self.0.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&mut info as *mut JOBOBJECT_BASIC_ACCOUNTING_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                std::ptr::null_mut(),
            ) != 0
                && info.ActiveProcesses == 0
        }
    }

    fn terminate(&self) {
        // SAFETY: valid job handle.
        unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) };
    }

    fn wait_empty(&self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if self.empty() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.empty()
    }
}

/// A job: its leader and everything started from it.
#[derive(Clone)]
pub struct Group {
    job: Arc<Job>,
    leader: u32,
    /// Whether the leader shares our console, so Ctrl+Break can reach its process group.
    console: bool,
}

impl Group {
    pub fn id(&self) -> i32 {
        self.leader as i32
    }

    /// The closest thing to SIGTERM: Ctrl+Break to the leader's process group. A plugin is on its own console,
    /// which Ctrl+Break can't reach, so it is terminated (it has had `stop`, or broke the protocol).
    pub fn term(&self) {
        if !self.console {
            self.kill();
        } else if self.leader != 0 {
            // SAFETY: no pointers involved.
            unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.leader) };
        }
    }

    pub fn kill(&self) {
        self.job.terminate();
    }

    /// Ctrl+Break, wait `grace`, terminate the job, wait up to 2 s. True once the job is empty. Blocking.
    pub fn reap_blocking(&self, grace: Duration) -> bool {
        if self.job.empty() {
            return true;
        }
        self.term();
        if self.job.wait_empty(grace) {
            return true;
        }
        self.kill();
        self.job.wait_empty(Duration::from_secs(2))
    }
}

/// Resumes every thread of a process created suspended (a new process has exactly one).
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: the snapshot and thread handles are owned here; THREADENTRY32 is sized before use.
    unsafe {
        let snap = owned(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0))
            .ok_or_else(io::Error::last_os_error)?;
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = false;
        let mut more = Thread32First(snap.as_raw_handle(), &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid
                && let Some(t) = owned(OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID))
            {
                resumed |= ResumeThread(t.as_raw_handle()) != u32::MAX;
            }
            more = Thread32Next(snap.as_raw_handle(), &mut entry) != 0;
        }
        if resumed {
            Ok(())
        } else {
            Err(io::Error::other("no thread to resume"))
        }
    }
}

/// A job created suspended inside its own Job Object; nothing runs until `release`.
pub struct Held {
    pub child: Child,
    pub group: Group,
}

impl Held {
    pub fn release(&mut self) {
        if resume(self.child.id()).is_err() {
            self.group.kill(); // never leave a suspended job behind
        }
    }
}

/// Windows has no exec, so there is no gate: the harness itself starts suspended and joins the job before it runs.
pub fn spawn_held(spec: &JobSpec) -> Result<Held, String> {
    let job = Job::create(Some(&job_name(&spec.name)))
        .map_err(|e| format!("cannot create job object: {e}"))?;
    let mut cmd = crate::guard::command(spec.argv[0].as_ref(), &spec.argv[1..], spec);
    cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot execute {}: {e}", spec.argv[0]))?;
    if let Err(e) = job.assign(child.as_raw_handle()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot contain the job: {e}"));
    }
    let leader = child.id();
    Ok(Held {
        child,
        group: Group {
            job: Arc::new(job),
            leader,
            console: true,
        },
    })
}

/// The guard gets its own hidden console and process group, out of reach of Ctrl+C meant for the core.
pub fn detach(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Nothing extra to kill: the guard's death closes its job handle, which kills the job.
pub fn kill_detached(_pid: i32) {}

/// Starts a plugin suspended, puts it in its own job, then lets it run.
pub fn spawn_grouped(
    cmd: &mut tokio::process::Command,
) -> io::Result<(tokio::process::Child, Group)> {
    cmd.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    let job = Job::create(None)?;
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    let contained = match child.raw_handle() {
        Some(h) => job.assign(h).and_then(|_| resume(pid)),
        None => Err(io::Error::other("plugin exited at start")),
    };
    if let Err(e) = contained {
        let _ = child.start_kill();
        return Err(e);
    }
    Ok((
        child,
        Group {
            job: Arc::new(job),
            leader: pid,
            console: false,
        },
    ))
}

/// Whether job `id` has no process left. Its name is unique to the job, so this needs no pid or start time.
pub fn job_gone(id: &str, _pgid: i32) -> bool {
    matches!(Job::open(&job_name(id)), Ok(None))
}

/// Kills what is left of job `id` and waits for it to be empty. Blocking.
pub fn reap_job(id: &str, _pgid: i32, _gate_start: i64, grace: Duration) -> bool {
    match Job::open(&job_name(id)) {
        Ok(None) => true,
        Ok(Some(job)) => {
            job.terminate();
            job.wait_empty(grace + Duration::from_secs(2))
        }
        Err(_) => false,
    }
}

/// Terminates a running job (Windows has no SIGTERM for another console); its guard reports the outcome.
pub fn cancel_job(id: &str, _pgid: i32, _gate_start: i64) -> bool {
    match Job::open(&job_name(id)) {
        Ok(Some(job)) => {
            job.terminate();
            true
        }
        _ => false,
    }
}

/// Ctrl+C, Ctrl+Break and closing the console stop the service in order.
pub fn on_shutdown(f: impl Fn() + Send + Sync + Clone + 'static) {
    use tokio::signal::windows;
    macro_rules! watch {
        ($make:expr) => {
            if let Ok(mut s) = $make {
                let f = f.clone();
                tokio::spawn(async move {
                    while s.recv().await.is_some() {
                        f();
                    }
                });
            }
        };
    }
    watch!(windows::ctrl_c());
    watch!(windows::ctrl_break());
    watch!(windows::ctrl_close());
}

/// The attached plugin talks over our stdin and stdout (docs/protocol.md). They are made non-inheritable so
/// that no child (plugin, guard, harness) holds the connection open.
pub fn attached_plugin() -> Result<(tokio::io::Stdin, tokio::io::Stdout), String> {
    for std in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
        // SAFETY: standard handles of this process; only the inherit flag changes.
        let ok = unsafe {
            let h = GetStdHandle(std);
            !h.is_null() && h as isize != -1 && SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) != 0
        };
        if !ok {
            return Err("stdin/stdout are not pipes".into());
        }
    }
    Ok((tokio::io::stdin(), tokio::io::stdout()))
}

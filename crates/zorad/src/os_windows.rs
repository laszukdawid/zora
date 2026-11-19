//! Process identity and job containment on Windows. A Job Object with `KILL_ON_JOB_CLOSE` stands in for a Unix
//! process group: processes are created inside it (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), whatever they start joins
//! it, and when the last handle closes (the guard exits or dies) Windows kills the whole job. Job names come from
//! the job id, so recovery finds them without trusting pids.

use crate::guard::JobSpec;
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, FILETIME, GetLastError,
    HANDLE, HANDLE_FLAG_INHERIT, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::System::Console::{
    CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent, GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    CreateEventW, CreateProcessW, DeleteProcThreadAttributeList, EVENT_MODIFY_STATE,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenEventW, OpenProcess,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    SetEvent, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

// Job object access rights (winnt.h).
const JOB_OBJECT_QUERY: u32 = 4;
const JOB_OBJECT_TERMINATE: u32 = 8;

fn owned(h: HANDLE) -> Option<OwnedHandle> {
    // SAFETY: a non-null handle just returned to us, owned by nobody else.
    (!h.is_null() && h as isize != -1).then(|| unsafe { OwnedHandle::from_raw_handle(h) })
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn session_id() -> u32 {
    let mut session = u32::MAX;
    // SAFETY: writes one u32.
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session) };
    session
}

/// The kernel's boot counter, or empty when it can't be read.
fn boot_counter() -> String {
    let key = wide(
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters",
    );
    let name = wide("BootId");
    let (mut value, mut size) = (0u32, 4u32);
    // SAFETY: the strings are NUL-terminated; the value buffer holds the DWORD asked for.
    let rc = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut value as *mut u32).cast(),
            &mut size,
        )
    };
    if rc == 0 {
        value.to_string()
    } else {
        String::new()
    }
}

/// `<boot counter>|s<session>`: job names are per Windows session, so recovery must know where a job ran.
pub fn boot_id() -> String {
    format!("{}|s{}", boot_counter(), session_id())
}

pub enum Owner {
    Here,
    OtherBoot,
    /// Recorded somewhere this process can't check; the reason says where.
    Elsewhere(String),
}

/// Where a job recorded with `recorded` (its `boot_id`) ran, relative to this process.
pub fn owner(recorded: Option<&str>) -> Owner {
    let Some(recorded) = recorded else {
        return Owner::Here;
    };
    let (rb, rs) = recorded.split_once('|').unwrap_or((recorded, ""));
    let current = boot_id();
    let (cb, cs) = current.split_once('|').unwrap_or((&current, ""));
    if !rb.is_empty() && !cb.is_empty() && rb != cb {
        Owner::OtherBoot
    } else if rs == cs {
        Owner::Here
    } else {
        Owner::Elsewhere(format!(
            "was started in another Windows session ({}), where this one can't see it",
            rs.trim_start_matches('s')
        ))
    }
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

unsafe extern "system" fn ignore_ctrl(_event: u32) -> i32 {
    1
}

/// The guard ignores Ctrl+C, Ctrl+Break and console close, so it can always reap its job.
pub fn ignore_termination_signals() {
    // SAFETY: registers a handler that only returns TRUE.
    unsafe { SetConsoleCtrlHandler(Some(ignore_ctrl), 1) };
}

fn job_name(id: &str) -> String {
    format!("Local\\zora-job-{id}")
}

fn cancel_name(id: &str) -> String {
    format!("Local\\zora-cancel-{id}")
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

    /// The named job in this session, or `None` once it is gone (all its processes have exited).
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

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: valid job handle.
        if unsafe { TerminateJobObject(self.0.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
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
    /// Set by `zorad jobs cancel` (guard jobs only).
    cancel: Option<Arc<OwnedHandle>>,
}

impl Group {
    pub fn id(&self) -> i32 {
        self.leader as i32
    }

    /// The closest thing to SIGTERM: Ctrl+Break to the leader's process group. Where Ctrl+Break can't be
    /// delivered (a plugin has its own console, which already got `stop`), the job is terminated.
    pub fn term(&self) {
        // SAFETY: no pointers involved.
        if !self.console || unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, self.leader) } == 0
        {
            self.kill();
        }
    }

    pub fn kill(&self) {
        let _ = self.job.terminate();
    }

    /// Ends the group after its leader exited: the job handle names only its own processes.
    pub fn finish_blocking(&self, grace: Duration, leader: Leader) -> (bool, ExitInfo) {
        let ok = self.reap_blocking(grace);
        (ok, Some((exit_code(&leader.0), None)))
    }

    /// For a group that already failed to reap. The job handle can't name another process, so it is safe to
    /// terminate again.
    pub fn retry_reap(&self) -> bool {
        self.reap_blocking(Duration::ZERO)
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

    /// Whether `zorad jobs cancel` asked for this job to stop.
    pub fn cancel_requested(&self) -> bool {
        self.cancel.as_ref().is_some_and(|e| {
            // SAFETY: valid event handle.
            unsafe { WaitForSingleObject(e.as_raw_handle(), 0) == WAIT_OBJECT_0 }
        })
    }
}

/// MSVC command-line quoting (what `CommandLineToArgvW` and the C runtime parse back).
fn quote(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\x0b', '"']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                out.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        out.push(c);
    }
    out.extend(std::iter::repeat_n('\\', backslashes));
    out.push('"');
}

fn command_line(program: &str, args: &[String]) -> io::Result<Vec<u16>> {
    let lower = program.to_ascii_lowercase();
    if program.contains(['"', '\0']) || lower.ends_with(".bat") || lower.ends_with(".cmd") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a program that can be started directly",
        ));
    }
    let mut line = format!("\"{program}\"");
    for a in args {
        if a.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NUL in argument",
            ));
        }
        line.push(' ');
        quote(a, &mut line);
    }
    Ok(wide(&line))
}

/// A sorted, case-insensitively unique environment block (later entries win). Refuses a name that is empty or
/// holds `=` (except Windows' own leading one, as in `=C:`) or NUL, and a value with NUL: either would cut the
/// block short or smuggle in another variable.
fn env_block(env: &[(String, String)]) -> io::Result<Vec<u16>> {
    let mut vars: Vec<(String, &String, &String)> = Vec::new();
    for (k, v) in env {
        let name = k.strip_prefix('=').unwrap_or(k);
        if name.is_empty() || name.contains(['=', '\0']) || v.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid environment variable {k:?}"),
            ));
        }
        let key = k.to_uppercase();
        vars.retain(|(u, _, _)| *u != key);
        vars.push((key, k, v));
    }
    vars.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block: Vec<u16> = Vec::new();
    for (_, k, v) in vars {
        block.extend(format!("{k}={v}").encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

/// An anonymous pipe: (our end, the child's inheritable end). `child_reads` picks the direction.
fn pipe(child_reads: bool) -> io::Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    // SAFETY: valid out-pointers and attributes.
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (r, w) = (owned(r).unwrap(), owned(w).unwrap());
    let (ours, theirs) = if child_reads { (w, r) } else { (r, w) };
    // SAFETY: valid handle; only the inherit flag changes.
    unsafe { SetHandleInformation(ours.as_raw_handle(), HANDLE_FLAG_INHERIT, 0) };
    Ok((ours, theirs))
}

fn null_device() -> io::Result<OwnedHandle> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("NUL")?;
    let h = OwnedHandle::from(f);
    // SAFETY: valid handle; only the inherit flag changes.
    if unsafe { SetHandleInformation(h.as_raw_handle(), HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(h)
}

/// A process created suspended, already inside its job.
struct Spawned {
    process: OwnedHandle,
    thread: OwnedHandle,
    pid: u32,
    stdin: Option<File>,
    stdout: File,
    stderr: File,
}

/// CreateProcessW with the job (`PROC_THREAD_ATTRIBUTE_JOB_LIST`, so no instruction runs outside it) and exactly
/// the three standard handles inherited (`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`).
fn create_in_job(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: Option<&[(String, String)]>,
    job: &Job,
    stdin_pipe: bool,
    flags: u32,
) -> io::Result<Spawned> {
    let mut cmdline = command_line(program, args)?;
    let app = std::path::Path::new(program)
        .is_absolute()
        .then(|| wide(program));
    let cwd = cwd.map(wide);
    let mut env = env.map(env_block).transpose()?;
    let (stdin_ours, stdin_theirs) = if stdin_pipe {
        let (o, t) = pipe(true)?;
        (Some(o), t)
    } else {
        (None, null_device()?)
    };
    let (stdout_ours, stdout_theirs) = pipe(false)?;
    let (stderr_ours, stderr_theirs) = pipe(false)?;
    let inherited: [HANDLE; 3] = [
        stdin_theirs.as_raw_handle(),
        stdout_theirs.as_raw_handle(),
        stderr_theirs.as_raw_handle(),
    ];
    let job_handle: HANDLE = job.0.as_raw_handle();
    // SAFETY: the attribute list buffer is sized by the first call and outlives CreateProcessW; every pointer
    // passed stays valid until the list is deleted.
    unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &mut size);
        let mut buf = vec![0u64; size.div_ceil(8)];
        let list: LPPROC_THREAD_ATTRIBUTE_LIST = buf.as_mut_ptr().cast();
        if InitializeProcThreadAttributeList(list, 2, 0, &mut size) == 0 {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            if UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                (&job_handle as *const HANDLE).cast(),
                std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
                || UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    inherited.as_ptr().cast(),
                    std::mem::size_of_val(&inherited),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut si: STARTUPINFOEXW = std::mem::zeroed();
            si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
            si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = inherited[0];
            si.StartupInfo.hStdOutput = inherited[1];
            si.StartupInfo.hStdError = inherited[2];
            si.lpAttributeList = list;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            if CreateProcessW(
                app.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
                cmdline.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags
                    | CREATE_SUSPENDED
                    | EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_UNICODE_ENVIRONMENT,
                env.as_mut()
                    .map_or(std::ptr::null(), |e| e.as_mut_ptr().cast_const().cast()),
                cwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                &si.StartupInfo,
                &mut pi,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(pi)
        })();
        DeleteProcThreadAttributeList(list);
        let pi = result?;
        Ok(Spawned {
            process: owned(pi.hProcess).unwrap(),
            thread: owned(pi.hThread).unwrap(),
            pid: pi.dwProcessId,
            stdin: stdin_ours.map(File::from),
            stdout: File::from(stdout_ours),
            stderr: File::from(stderr_ours),
        })
    }
}

/// Resumes a process created suspended; anything but a first resume of its one primary thread is a failure.
fn resume(thread: &OwnedHandle) -> Result<(), String> {
    // SAFETY: valid thread handle from CreateProcessW.
    match unsafe { ResumeThread(thread.as_raw_handle()) } {
        1 => Ok(()),
        u32::MAX => Err(format!(
            "cannot resume the job: {}",
            io::Error::last_os_error()
        )),
        n => Err(format!("cannot resume the job (suspend count {n})")),
    }
}

/// Exit code once the process has exited.
fn exit_code(process: &OwnedHandle) -> Option<i32> {
    let mut code = 0u32;
    // SAFETY: valid process handle.
    let ok = unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut code) } != 0;
    (ok && code != STILL_ACTIVE as u32).then_some(code as i32)
}

/// The harness, created suspended inside its named job; nothing runs until `release`.
pub struct Held {
    pub group: Group,
    process: OwnedHandle,
    thread: OwnedHandle,
    stdout: Option<File>,
    stderr: Option<File>,
}

impl Held {
    pub fn take_output(&mut self) -> (File, File) {
        (self.stdout.take().unwrap(), self.stderr.take().unwrap())
    }

    pub fn release(&mut self) -> Result<(), String> {
        resume(&self.thread)
    }

    /// Blocks until the leader exits.
    pub fn wait_exit(self) -> Leader {
        // SAFETY: valid process handle.
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), u32::MAX) };
        Leader(self.process)
    }
}

/// Windows has no exec, so there is no gate: the harness itself is created suspended inside its job and resumed
/// on `go`.
pub fn spawn_held(spec: &JobSpec) -> Result<Held, String> {
    let job = Job::create(Some(&job_name(&spec.name)))
        .map_err(|e| format!("cannot create job object: {e}"))?;
    let cancel_name = wide(&cancel_name(&spec.name));
    // SAFETY: the name is NUL-terminated; a manual-reset event, initially unset.
    let cancel = unsafe {
        let h = CreateEventW(std::ptr::null(), 1, 0, cancel_name.as_ptr());
        let existed = GetLastError() == ERROR_ALREADY_EXISTS;
        match owned(h) {
            Some(h) if !existed => h,
            _ => return Err("cannot create the job's cancel event".into()),
        }
    };
    let s = create_in_job(
        &spec.argv[0],
        &spec.argv[1..],
        Some(&spec.cwd),
        Some(&spec.env),
        &job,
        false,
        CREATE_NEW_PROCESS_GROUP,
    )
    .map_err(|e| format!("cannot execute {}: {e}", spec.argv[0]))?;
    Ok(Held {
        group: Group {
            job: Arc::new(job),
            leader: s.pid,
            console: true,
            cancel: Some(Arc::new(cancel)),
        },
        process: s.process,
        thread: s.thread,
        stdout: Some(s.stdout),
        stderr: Some(s.stderr),
    })
}

/// The guard gets its own hidden console and process group, out of reach of Ctrl+C meant for the core; its
/// harness shares that console, so the guard can send it Ctrl+Break.
pub fn detach(cmd: &mut tokio::process::Command) {
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

/// Nothing extra to kill: the guard's death closes its job handle, which kills the job.
pub fn kill_detached(_pid: i32) {}

/// A leader's (exit code, signal), when known.
pub type ExitInfo = Option<(Option<i32>, Option<i32>)>;

/// A job's leader that has exited.
pub struct Leader(OwnedHandle);

/// Stops a plugin and its job. True once the job is verifiably empty.
pub async fn stop_plugin(child: &mut PluginChild, group: &Group) -> bool {
    // Wait for the leader first, then the job; the job handle can't name another process.
    group.term();
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    let group = group.clone();
    tokio::task::spawn_blocking(move || group.reap_blocking(Duration::from_secs(3)))
        .await
        .unwrap_or(false)
}

/// A plugin process, with the parts of `tokio::process::Child` the plugin host uses.
pub struct PluginChild {
    process: OwnedHandle,
    pub stdin: Option<tokio::fs::File>,
    pub stdout: Option<tokio::fs::File>,
    pub stderr: Option<tokio::fs::File>,
}

impl PluginChild {
    pub async fn wait(&mut self) -> io::Result<i32> {
        loop {
            // SAFETY: valid process handle.
            if unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } == WAIT_OBJECT_0 {
                return Ok(exit_code(&self.process).unwrap_or(-1));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn kill(&mut self) -> io::Result<()> {
        // SAFETY: valid process handle.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
        self.wait().await.map(|_| ())
    }
}

impl Drop for PluginChild {
    fn drop(&mut self) {
        // SAFETY: valid process handle; terminating an exited process is harmless.
        unsafe { TerminateProcess(self.process.as_raw_handle(), 1) };
    }
}

/// Starts a plugin inside its own job, on its own hidden console, with piped stdio.
pub fn spawn_plugin(exe: &str, args: &[String]) -> io::Result<(PluginChild, Group)> {
    let job = Job::create(None)?;
    let s = create_in_job(
        exe,
        args,
        None,
        None,
        &job,
        true,
        CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW,
    )?;
    if let Err(e) = resume(&s.thread) {
        let _ = job.terminate();
        return Err(io::Error::other(e));
    }
    let child = PluginChild {
        process: s.process,
        stdin: s.stdin.map(tokio::fs::File::from_std),
        stdout: Some(tokio::fs::File::from_std(s.stdout)),
        stderr: Some(tokio::fs::File::from_std(s.stderr)),
    };
    Ok((
        child,
        Group {
            job: Arc::new(job),
            leader: s.pid,
            console: false,
            cancel: None,
        },
    ))
}

/// Whether job `id` has no process left. Only meaningful in the job's own session (see `owner`).
pub fn job_gone(id: &str, _pgid: i32) -> bool {
    matches!(Job::open(&job_name(id)), Ok(None))
}

/// Kills what is left of job `id` and waits for it to be empty. Blocking; same-session only (see `owner`).
pub fn reap_job(id: &str, _pgid: i32, _gate_start: i64, grace: Duration) -> bool {
    match Job::open(&job_name(id)) {
        Ok(None) => true,
        Ok(Some(job)) => job.terminate().is_ok() && job.wait_empty(grace + Duration::from_secs(2)),
        Err(_) => false,
    }
}

/// Whether job `id`'s cancel event is set (the core's own check, independent of the guard).
pub fn cancel_requested(id: &str, _marker: &std::path::Path) -> bool {
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let name = wide(&cancel_name(id));
    // SAFETY: the name is NUL-terminated; the handle is owned below.
    let Some(event) = owned(unsafe { OpenEventW(SYNCHRONIZE, 0, name.as_ptr()) }) else {
        return false;
    };
    // SAFETY: valid event handle.
    unsafe { WaitForSingleObject(event.as_raw_handle(), 0) == WAIT_OBJECT_0 }
}

/// Windows keeps no marker files (the cancel event goes away with its guard).
pub fn prune_cancel_markers(_dir: &std::path::Path, _live: &std::collections::HashSet<String>) {}

/// Asks the job's guard to cancel it, so the outcome is recorded as cancelled.
pub fn cancel_job(
    job: &zora_core::store::JobRecord,
    _marker: &std::path::Path,
) -> Result<(), String> {
    let name = wide(&cancel_name(&job.id));
    // SAFETY: the name is NUL-terminated; the handle is owned below.
    let event = owned(unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) })
        .ok_or("is not running in this Windows session")?;
    // SAFETY: valid event handle.
    if unsafe { SetEvent(event.as_raw_handle()) } == 0 {
        return Err(format!(
            "cannot be cancelled: {}",
            io::Error::last_os_error()
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_round_trip_through_msvc_quoting() {
        let mut s = String::new();
        quote(r#"say "hi" \ & %PATH% a\\"#, &mut s);
        assert_eq!(s, r#""say \"hi\" \ & %PATH% a\\\\""#);
        let mut s = String::new();
        quote("", &mut s);
        assert_eq!(s, r#""""#);
    }

    #[test]
    fn environment_block_refuses_names_and_values_that_would_split_it() {
        let var = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
        assert!(env_block(&var("A", "1")).is_ok());
        assert!(env_block(&var("=C:", "C:\\x")).is_ok());
        for (k, v) in [
            ("A=B", "1"),
            ("", "1"),
            ("=", "1"),
            ("A\0B", "1"),
            ("A", "1\0B=2"),
        ] {
            assert!(env_block(&var(k, v)).is_err(), "{k:?}={v:?}");
        }
    }
}

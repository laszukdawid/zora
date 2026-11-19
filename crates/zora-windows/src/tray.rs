//! `zora-tray`: the notification-area icon, its menu and notifications (balloons, which Windows 10 and 11 show as
//! toasts). It is a `notify` plugin started by zorad, so it learns the listening state (`start`/`listening`) and
//! every event (`notify`) over the protocol, like the macOS menu bar and the Linux tray. Changes go through
//! `zorad pause|resume|settings`, never by editing files.
//!
//! Everything Win32 runs on the main thread's message loop; a 200 ms timer drains the core's messages and the
//! results of `zorad` calls, which run on worker threads.

use crate::menu::{self, Action, Item, Options};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::os::windows::process::CommandExt;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows_sys::Win32::UI::Shell::{
    BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, NIF_ICON, NIF_INFO, NIF_MESSAGE,
    NIF_TIP, NIIF_ERROR, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    SHBrowseForFolderW, SHGetPathFromIDListW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, BS_DEFPUSHBUTTON, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, GetCursorPos, GetMessageW,
    GetWindowTextLengthW, GetWindowTextW, HICON, HMENU, IDCANCEL, IDOK, IsDialogMessageW,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SW_SHOW, SendMessageW, SetForegroundWindow, SetTimer,
    ShowWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WM_SETFONT, WM_TIMER,
    WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD, WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use zora_plugin::proto::{CoreMsg, Out, clock_reply};
use zora_plugin::tray::{Icon, Request, TrayModel};

const WM_TRAY: u32 = WM_APP + 1;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Copies `s` into a fixed buffer, cut to fit with its terminating NUL.
fn fill(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

/// Results from worker threads.
enum Ev {
    PauseDone(Request, Result<(), String>),
    Error(String),
}

struct App {
    hwnd: HWND,
    model: TrayModel,
    out: Arc<Out>,
    core: Receiver<Option<CoreMsg>>,
    done_tx: Sender<Ev>,
    done_rx: Receiver<Ev>,
    opts: Arc<Options>,
    icons: Vec<(Icon, HICON)>,
    shown: Option<(Icon, String)>,
    added: bool,
    last_tick: Instant,
    stopped: bool,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TASKBAR_CREATED: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Runs `f` on the app state. Never call it around anything that runs a nested message loop (menus, dialogs).
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok()?.as_mut().map(f))
}

/// A filled circle in the state's colour, 16×16, transparent around it.
fn make_icon(icon: Icon) -> HICON {
    let (r, g, b) = match icon {
        Icon::Idle => (0x60, 0x7d, 0x8b),
        Icon::Paused => (0xb0, 0xb0, 0xb0),
        Icon::Listening => (0x1e, 0x88, 0xe5),
        Icon::Running => (0xfb, 0x8c, 0x00),
        Icon::Done => (0x43, 0xa0, 0x47),
        Icon::Failed => (0xe5, 0x39, 0x35),
        Icon::Degraded => (0xff, 0xb3, 0x00),
    };
    let mut xor = vec![0u8; 16 * 16 * 4];
    let mut and = [0xffu8; 16 * 2];
    for y in 0..16 {
        for x in 0..16 {
            let (dx, dy) = (x as f32 - 7.5, y as f32 - 7.5);
            if dx * dx + dy * dy <= 6.5 * 6.5 {
                let i = (y * 16 + x) * 4;
                xor[i..i + 4].copy_from_slice(&[b, g, r, 0xff]);
                and[y * 2 + x / 8] &= !(0x80 >> (x % 8));
            }
        }
    }
    // SAFETY: both bitmaps have the sizes CreateIcon expects for 16×16 at 32 bpp.
    unsafe {
        CreateIcon(
            GetModuleHandleW(std::ptr::null()),
            16,
            16,
            1,
            32,
            and.as_ptr(),
            xor.as_ptr(),
        )
    }
}

fn notify_data(hwnd: HWND) -> NOTIFYICONDATAW {
    // SAFETY: a plain C struct; zero is a valid starting value.
    let mut d: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    d.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    d.hWnd = hwnd;
    d.uID = 1;
    d
}

impl App {
    /// Shows the model's icon and tooltip; adds the icon the first time (and after Explorer restarts).
    fn refresh(&mut self) {
        let view = self.model.view();
        if self.stopped
            || (self.added && self.shown.as_ref() == Some(&(view.icon, view.tooltip.clone())))
        {
            return;
        }
        let mut d = notify_data(self.hwnd);
        d.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        d.uCallbackMessage = WM_TRAY;
        d.hIcon = match self.icons.iter().find(|(i, _)| *i == view.icon) {
            Some((_, h)) => *h,
            None => {
                let h = make_icon(view.icon);
                self.icons.push((view.icon, h));
                h
            }
        };
        fill(&mut d.szTip, &view.tooltip);
        // SAFETY: `d` is a fully initialized NOTIFYICONDATAW for this window.
        let ok =
            unsafe { Shell_NotifyIconW(if self.added { NIM_MODIFY } else { NIM_ADD }, &d) } != 0;
        if ok {
            self.added = true;
            self.shown = Some((view.icon, view.tooltip));
        }
    }

    fn balloon(&self, title: &str, body: &str, error: bool) {
        if !self.added {
            return;
        }
        let mut d = notify_data(self.hwnd);
        d.uFlags = NIF_INFO;
        fill(&mut d.szInfoTitle, title);
        fill(&mut d.szInfo, if body.is_empty() { " " } else { body });
        d.dwInfoFlags = if error { NIIF_ERROR } else { NIIF_INFO };
        // SAFETY: `d` is a fully initialized NOTIFYICONDATAW for this window.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &d) };
    }

    fn remove_icon(&mut self) {
        if self.added {
            let d = notify_data(self.hwnd);
            // SAFETY: as above.
            unsafe { Shell_NotifyIconW(NIM_DELETE, &d) };
            self.added = false;
        }
    }

    /// The timer: core messages, worker results, the model's clock.
    fn drain(&mut self) {
        while let Ok(msg) = self.core.try_recv() {
            match msg {
                None => {
                    self.remove_icon();
                    // SAFETY: plain call on this thread.
                    unsafe { PostQuitMessage(0) };
                    return;
                }
                Some(CoreMsg::Start {
                    listening,
                    until,
                    epoch,
                }) => {
                    self.model.set_core_listening(listening, until, epoch, None);
                }
                Some(CoreMsg::Listening {
                    listening,
                    until,
                    epoch,
                    confirmed,
                }) => {
                    self.model
                        .set_core_listening(listening, until, epoch, confirmed);
                }
                Some(CoreMsg::Notify(n)) => {
                    let show = self
                        .model
                        .handle(&n.kind, &n.title, &n.body, Instant::now());
                    if show && self.opts.notifications {
                        self.balloon(
                            &n.title,
                            &n.body,
                            n.kind == "jobFailed" || n.kind == "degraded",
                        );
                    }
                    self.out.send(json!({"t": "ack", "id": n.id, "ok": true}));
                }
                Some(CoreMsg::Clock { id }) => {
                    self.out.send(clock_reply(id));
                }
                Some(CoreMsg::Stop) => {
                    self.remove_icon();
                    self.stopped = true;
                    self.out.send(json!({"t": "stopped"}));
                }
                Some(_) => {}
            }
        }
        while let Ok(ev) = self.done_rx.try_recv() {
            match ev {
                Ev::PauseDone(req, r) => {
                    if let Err(e) = &r {
                        eprintln!("zora-tray: zorad {:?} failed: {e}", req.args());
                    }
                    self.model.finish(&req, r, Instant::now());
                }
                Ev::Error(e) => self.balloon("Zora", &e, true),
            }
        }
        if self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.last_tick = Instant::now();
            self.model.tick(Instant::now());
        }
        self.refresh();
    }
}

/// Runs `zorad` with `args`, at most 10 s.
fn zorad(opts: &Options, args: &[String]) -> Result<String, String> {
    let mut cmd = std::process::Command::new(&opts.zorad);
    cmd.args(opts.zorad_args(args))
        .stdin(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);
    run_bounded(cmd, &opts.zorad)
}

/// A Job Object that kills whatever is still in it when it is closed or terminated.
struct KillJob(windows_sys::Win32::Foundation::HANDLE);

impl KillJob {
    fn new() -> Option<KillJob> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        // SAFETY: a new unnamed job; the info struct is fully initialized.
        unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                return None;
            }
            let job = KillJob(h);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                h,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) != 0;
            ok.then_some(job)
        }
    }

    fn assign(&self, child: &std::process::Child) -> bool {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: both handles are valid for the call.
        unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                self.0,
                child.as_raw_handle(),
            ) != 0
        }
    }

    fn terminate(&self) {
        // SAFETY: valid job handle.
        unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) };
    }
}

impl Drop for KillJob {
    fn drop(&mut self) {
        // SAFETY: closing the only handle kills anything left in the job.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

/// Runs `cmd` for at most 10 s, inside a job. On timeout the whole job is terminated and the process reaped
/// before this returns, so a late `zorad` can never land after a later request; output readers are waited for
/// at most 2 s more, in case something outside the job holds the pipes.
fn run_bounded(mut cmd: std::process::Command, name: &str) -> Result<String, String> {
    use std::io::Read;
    let job = KillJob::new().ok_or_else(|| format!("cannot run {name}: no job object"))?;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {name}: {e}"))?;
    if !job.assign(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot run {name} inside a job"));
    }
    let drain = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut r) = r {
                let _ = r.read_to_end(&mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    };
    let out = drain(
        child
            .stdout
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let err = drain(
        child
            .stderr
            .take()
            .map(|r| Box::new(r) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Ok(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                job.terminate();
                let _ = child.wait();
                break Err(format!("{name} did not answer within 10 s and was stopped"));
            }
            Err(e) => {
                job.terminate();
                let _ = child.wait();
                break Err(format!("cannot run {name}: {e}"));
            }
        }
    };
    drop(job);
    let grace = Duration::from_secs(2);
    let (out, err) = (
        out.recv_timeout(grace).unwrap_or_default(),
        err.recv_timeout(grace).unwrap_or_default(),
    );
    match status? {
        s if s.success() => Ok(String::from_utf8_lossy(&out).into_owned()),
        s => {
            let err = String::from_utf8_lossy(&err).trim().to_string();
            Err(if err.is_empty() {
                format!("{name} exited with {s}")
            } else {
                err
            })
        }
    }
}

/// After a pause or resume that failed or timed out: what `zorad status` says now, for the notice. The next
/// request is enabled only after this.
fn reconcile(opts: &Options, failed: String) -> String {
    match zorad(opts, &["status".into()]) {
        Ok(s) => match s.lines().find_map(|l| l.strip_prefix("listening: ")) {
            Some(state) => format!("{failed}; listening is now {state}"),
            None => failed,
        },
        Err(e) => format!("{failed}; and its state couldn't be read: {e}"),
    }
}

fn append(menu: HMENU, items: &[Item]) {
    for item in items {
        // SAFETY: `menu` is a live menu; strings outlive each call.
        unsafe {
            match item {
                Item::Separator => {
                    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
                }
                Item::Label(text) => {
                    AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, wide(text).as_ptr());
                }
                Item::Command {
                    action,
                    label,
                    enabled,
                } => {
                    let flags = if *enabled {
                        MF_STRING
                    } else {
                        MF_STRING | MF_GRAYED
                    };
                    AppendMenuW(menu, flags, action.id() as usize, wide(label).as_ptr());
                }
                Item::Submenu(label, sub) => {
                    let child = CreatePopupMenu();
                    append(child, sub);
                    AppendMenuW(menu, MF_POPUP, child as usize, wide(label).as_ptr());
                }
            }
        }
    }
}

/// Shows the menu at the cursor and returns the chosen action. Runs a nested loop: no app borrow held here.
fn show_menu(hwnd: HWND, items: &[Item]) -> Option<Action> {
    // SAFETY: standard popup-menu sequence on this thread's window; the menu is destroyed afterwards.
    unsafe {
        let menu = CreatePopupMenu();
        append(menu, items);
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let id = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        DestroyMenu(menu);
        // Lets the menu close properly when the user clicks elsewhere (documented TrackPopupMenu quirk).
        PostMessageW(hwnd, WM_NULL, 0, 0);
        Action::from_id(id as u32)
    }
}

fn act(action: Action) {
    let Some((opts, tx, toggle)) =
        with_app(|a| (a.opts.clone(), a.done_tx.clone(), a.model.toggle_request()))
    else {
        return;
    };
    if let Some(req) = menu::pause_request(action, toggle) {
        if with_app(|a| a.model.begin()) == Some(true) {
            std::thread::spawn(move || {
                let r = zorad(&opts, &req.args())
                    .map(|_| ())
                    .map_err(|e| reconcile(&opts, e));
                let _ = tx.send(Ev::PauseDone(req, r));
            });
        }
        return;
    }
    match action {
        Action::Names => {
            let current = match zorad(&opts, &["settings".into()]) {
                Ok(s) => serde_json::from_str::<Value>(&s).unwrap_or_default(),
                Err(e) => {
                    let _ = tx.send(Ev::Error(format!("Zora couldn't read its settings: {e}")));
                    return;
                }
            };
            let prompt =
                "Names Zora answers to, separated by commas. Say “Hey <name> … Do it <name>”.";
            if let Some(text) = input_box("Zora Settings", prompt, &menu::names_text(&current)) {
                save(opts, tx, json!({"wakeNames": menu::parse_names(&text)}));
            }
        }
        Action::Folder => {
            if let Some(dir) = pick_folder() {
                save(opts, tx, json!({"workspace": dir}));
            }
        }
        Action::Restart => {
            std::thread::spawn(move || {
                let mut cmd = std::process::Command::new("schtasks.exe");
                cmd.args(["/Run", "/TN", &opts.task])
                    .creation_flags(CREATE_NO_WINDOW);
                if let Err(e) = run_bounded(cmd, "schtasks") {
                    let _ = tx.send(Ev::Error(format!(
                        "Zora couldn't restart (scheduled task \"{}\"): {e}",
                        opts.task
                    )));
                }
            });
        }
        Action::Toggle | Action::PauseHour => {}
    }
}

fn save(opts: Arc<Options>, tx: Sender<Ev>, change: Value) {
    std::thread::spawn(move || {
        if let Err(e) = zorad(
            &opts,
            &["settings".into(), "set".into(), change.to_string()],
        ) {
            let _ = tx.send(Ev::Error(format!("Zora couldn't save that: {e}")));
        }
    });
}

fn pick_folder() -> Option<String> {
    let title = wide("Project folder for Zora's commands");
    // SAFETY: BROWSEINFOW is zero-initialized then filled; the returned item list is freed with CoTaskMemFree.
    unsafe {
        let mut bi: BROWSEINFOW = std::mem::zeroed();
        bi.lpszTitle = title.as_ptr();
        bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
        let pidl = SHBrowseForFolderW(&bi);
        if pidl.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0;
        CoTaskMemFree(pidl as *const _);
        let len = buf.iter().position(|c| *c == 0).unwrap_or(0);
        (ok && len > 0).then(|| String::from_utf16_lossy(&buf[..len]))
    }
}

// MARK: the input box

thread_local! {
    static INPUT: RefCell<(HWND, Option<Option<String>>)> = const { RefCell::new((std::ptr::null_mut(), None)) };
}

unsafe extern "system" fn input_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let finish = |value: Option<String>| {
        INPUT.with(|i| i.borrow_mut().1 = Some(value));
    };
    match msg {
        WM_COMMAND if (wparam & 0xffff) as i32 == IDOK => {
            let edit = INPUT.with(|i| i.borrow().0);
            // SAFETY: reads the edit control's text into a buffer of its length.
            let text = unsafe {
                let n = GetWindowTextLengthW(edit).max(0) as usize;
                let mut buf = vec![0u16; n + 1];
                let got = GetWindowTextW(edit, buf.as_mut_ptr(), buf.len() as i32).max(0) as usize;
                String::from_utf16_lossy(&buf[..got])
            };
            finish(Some(text));
            0
        }
        WM_COMMAND if (wparam & 0xffff) as i32 == IDCANCEL => {
            finish(None);
            0
        }
        WM_CLOSE => {
            finish(None);
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// A small modal text box; `None` when cancelled.
fn input_box(title: &str, prompt: &str, initial: &str) -> Option<String> {
    let class = wide("ZoraInputBox");
    // SAFETY: standard window creation and a local message loop on this thread; the window is destroyed before
    // returning.
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(input_proc);
        wc.hInstance = inst;
        wc.lpszClassName = class.as_ptr();
        wc.hbrBackground = (15 + 1) as _; // COLOR_BTNFACE + 1
        RegisterClassW(&wc); // fails harmlessly when already registered
        let win = CreateWindowExW(
            0,
            class.as_ptr(),
            wide(title).as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            300,
            300,
            440,
            190,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if win.is_null() {
            return None;
        }
        let font = GetStockObject(DEFAULT_GUI_FONT);
        let child = |class: &str, text: &str, style: u32, x, y, w, h, id: i32| {
            let c = CreateWindowExW(
                0,
                wide(class).as_ptr(),
                wide(text).as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                win,
                id as isize as HMENU,
                inst,
                std::ptr::null(),
            );
            SendMessageW(c, WM_SETFONT, font as usize, 1);
            c
        };
        child("STATIC", prompt, 0, 12, 10, 400, 40, 0);
        let edit = child(
            "EDIT",
            initial,
            WS_BORDER | WS_TABSTOP | ES_AUTOHSCROLL as u32,
            12,
            55,
            400,
            24,
            100,
        );
        child(
            "BUTTON",
            "OK",
            WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
            242,
            100,
            80,
            28,
            IDOK,
        );
        child("BUTTON", "Cancel", WS_TABSTOP, 332, 100, 80, 28, IDCANCEL);
        INPUT.with(|i| *i.borrow_mut() = (edit, None));
        ShowWindow(win, SW_SHOW);
        SetForegroundWindow(win);
        SetFocus(edit);
        let mut msg: MSG = std::mem::zeroed();
        while INPUT.with(|i| i.borrow().1.is_none())
            && GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0
        {
            if IsDialogMessageW(win, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        DestroyWindow(win);
        INPUT.with(|i| i.borrow_mut().1.take()).flatten()
    }
}

// MARK: the hidden window and the loop

unsafe extern "system" fn tray_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            with_app(App::drain);
            0
        }
        WM_TRAY => {
            let event = (lparam & 0xffff) as u32;
            if matches!(event, WM_LBUTTONUP | WM_RBUTTONUP | WM_CONTEXTMENU)
                && let Some(items) = with_app(|a| menu::build(&a.model.view()))
                && let Some(action) = show_menu(hwnd, &items)
            {
                act(action);
                with_app(App::refresh);
            }
            0
        }
        m if m != 0 && m == TASKBAR_CREATED.with(|t| t.get()) => {
            with_app(|a| {
                a.added = false;
                a.refresh();
            });
            0
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Runs the tray until the core closes the connection. `core` carries the core's messages, then `None` at EOF.
pub fn run(config: &Value, out: Arc<Out>, core: Receiver<Option<CoreMsg>>) -> i32 {
    let class = wide("ZoraTray");
    // The folder picker needs a single-threaded apartment; entered once, left when `run` returns.
    let _com = match crate::com::Apartment::enter(false) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("zora-tray: {e}");
            return 1;
        }
    };
    // SAFETY: window and message-loop setup on this thread.
    unsafe {
        let inst = GetModuleHandleW(std::ptr::null());
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(tray_proc);
        wc.hInstance = inst;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        TASKBAR_CREATED.with(|t| t.set(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr())));
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("Zora").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            inst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            eprintln!("zora-tray: cannot create its window");
            return 1;
        }
        let (done_tx, done_rx) = channel();
        let app = App {
            hwnd,
            model: TrayModel::new(),
            out,
            core,
            done_tx,
            done_rx,
            opts: Arc::new(Options::from_config(config)),
            icons: Vec::new(),
            shown: None,
            added: false,
            last_tick: Instant::now(),
            stopped: false,
        };
        APP.with(|a| *a.borrow_mut() = Some(app));
        with_app(App::refresh);
        SetTimer(hwnd, 1, 200, None);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        with_app(App::remove_icon);
    }
    0
}

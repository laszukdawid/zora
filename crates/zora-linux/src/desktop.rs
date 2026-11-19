//! `zora-desktop`: desktop notifications (org.freedesktop.Notifications over zbus) and the tray icon
//! (StatusNotifierItem via ksni). It is a `notify` plugin started by zorad, so it learns the listening state
//! (`start`/`listening`) and every event (`notify`) over the protocol, like the macOS menu bar does. Changes go
//! through `zorad pause|resume|settings`, never by editing files.

use ksni::TrayMethods;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use zbus::zvariant;
use zora_plugin::proto::{CoreMsg, Notify, Out, clock_reply};
use zora_plugin::tray::{Icon, Request, TrayModel, agent_choices};

const CUSTOM: &str = "custom";

#[derive(Debug, Clone)]
pub enum Action {
    Pause(Request),
    Names,
    Agent,
    Folder,
    Restart,
}

pub struct Options {
    pub tray: bool,
    pub notifications: bool,
    pub zorad: String,
    pub zorad_config: Option<String>,
}

impl Options {
    pub fn from_config(config: &Value) -> Options {
        let zorad = config["zorad"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("zorad")))
                    .filter(|p| p.is_file())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "zorad".into())
            });
        Options {
            tray: config["tray"].as_bool().unwrap_or(true),
            notifications: config["notifications"].as_bool().unwrap_or(true),
            zorad,
            zorad_config: config["zoradConfig"].as_str().map(String::from),
        }
    }
}

// MARK: tray

struct ZoraTray {
    model: Arc<Mutex<TrayModel>>,
    tx: UnboundedSender<Action>,
}

fn icon_name(icon: Icon) -> &'static str {
    match icon {
        Icon::Idle => "audio-input-microphone-symbolic",
        Icon::Paused => "microphone-disabled-symbolic",
        Icon::Listening => "microphone-sensitivity-high-symbolic",
        Icon::Running => "emblem-synchronizing-symbolic",
        Icon::Done => "emblem-ok-symbolic",
        Icon::Failed => "dialog-warning-symbolic",
        Icon::Degraded => "dialog-error-symbolic",
    }
}

/// A coloured dot (a ring while paused) for hosts whose icon theme lacks the names above.
fn pixmap(icon: Icon) -> ksni::Icon {
    let (r, g, b) = match icon {
        Icon::Idle | Icon::Paused => (0x9a, 0x9a, 0x9a),
        Icon::Listening => (0xe0, 0x1b, 0x24),
        Icon::Running => (0x35, 0x84, 0xe4),
        Icon::Done => (0x2e, 0xc2, 0x7e),
        Icon::Failed => (0xff, 0x78, 0x00),
        Icon::Degraded => (0xf6, 0xd3, 0x2d),
    };
    let size = 22i32;
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let c = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let inside = if icon == Icon::Paused {
                (6.5..=9.5).contains(&d)
            } else {
                d <= 9.5
            };
            data.extend_from_slice(&[if inside { 0xff } else { 0 }, r, g, b]);
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

impl ZoraTray {
    fn send(tx: &UnboundedSender<Action>, a: Action) {
        let _ = tx.send(a);
    }
}

impl ksni::Tray for ZoraTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        "zora".into()
    }

    fn title(&self) -> String {
        "Zora".into()
    }

    fn icon_name(&self) -> String {
        icon_name(self.model.lock().unwrap().view().icon).into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![pixmap(self.model.lock().unwrap().view().icon)]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Zora".into(),
            description: self.model.lock().unwrap().view().tooltip,
            ..Default::default()
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let (view, toggle) = {
            let m = self.model.lock().unwrap();
            (m.view(), m.toggle_request())
        };
        let mut items: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: view.state_line.clone(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: view.pause_label.into(),
                enabled: view.pause_enabled,
                activate: Box::new(move |t: &mut Self| {
                    if let Some(r) = toggle.clone() {
                        Self::send(&t.tx, Action::Pause(r));
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Pause for 1 Hour".into(),
                enabled: view.pause_enabled,
                visible: view.show_pause_hour,
                activate: Box::new(|t: &mut Self| {
                    Self::send(&t.tx, Action::Pause(Request::PauseHour))
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Settings".into(),
                submenu: vec![
                    StandardItem {
                        label: "Names Zora Answers To…".into(),
                        activate: Box::new(|t: &mut Self| Self::send(&t.tx, Action::Names)),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Coding Agent…".into(),
                        activate: Box::new(|t: &mut Self| Self::send(&t.tx, Action::Agent)),
                        ..Default::default()
                    }
                    .into(),
                    StandardItem {
                        label: "Project Folder…".into(),
                        activate: Box::new(|t: &mut Self| Self::send(&t.tx, Action::Folder)),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
        ];
        if !view.history.is_empty() {
            items.push(MenuItem::Separator);
            for h in view.history {
                items.push(
                    StandardItem {
                        label: h,
                        enabled: false,
                        ..Default::default()
                    }
                    .into(),
                );
            }
        }
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Restart Zora".into(),
                activate: Box::new(|t: &mut Self| Self::send(&t.tx, Action::Restart)),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

// MARK: notifications

struct Notifier {
    conn: Option<zbus::Connection>,
    /// The "listening" banner, replaced by the next one and withdrawn on a voice cancel.
    listening_id: u32,
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl Notifier {
    async fn conn(&mut self) -> Result<zbus::Connection, String> {
        if let Some(c) = &self.conn {
            return Ok(c.clone());
        }
        let c = zbus::Connection::session()
            .await
            .map_err(|e| format!("no D-Bus session bus: {e}"))?;
        self.conn = Some(c.clone());
        Ok(c)
    }

    async fn call<B>(&mut self, method: &str, body: &B) -> Result<zbus::Message, String>
    where
        B: serde::Serialize + zvariant::DynamicType,
    {
        let conn = self.conn().await?;
        let call = conn.call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            method,
            body,
        );
        match tokio::time::timeout(Duration::from_secs(5), call).await {
            Ok(Ok(m)) => Ok(m),
            Ok(Err(e)) => {
                if matches!(e, zbus::Error::InputOutput(_)) {
                    self.conn = None;
                }
                Err(e.to_string())
            }
            Err(_) => Err("the notification service did not answer".into()),
        }
    }

    async fn post(&mut self, title: &str, body: &str, replaces: u32) -> Result<u32, String> {
        let hints: HashMap<&str, zvariant::Value> =
            HashMap::from([("desktop-entry", zvariant::Value::from("zora"))]);
        let actions: Vec<&str> = vec![];
        let reply = self
            .call(
                "Notify",
                &(
                    "Zora",
                    replaces,
                    "audio-input-microphone",
                    title,
                    escape(body),
                    actions,
                    hints,
                    -1i32,
                ),
            )
            .await?;
        reply.body().deserialize::<u32>().map_err(|e| e.to_string())
    }

    async fn close(&mut self, id: u32) -> Result<(), String> {
        if id == 0 {
            return Ok(());
        }
        self.call("CloseNotification", &(id,)).await.map(|_| ())
    }

    /// Mirrors the macOS app: a voice cancel withdraws the listening banner and shows nothing.
    async fn notify(&mut self, n: &Notify, show: bool) -> Result<(), String> {
        if n.kind == "captureDiscarded" && n.body == "cancelled" {
            let id = std::mem::take(&mut self.listening_id);
            return self.close(id).await;
        }
        if !show {
            return Ok(());
        }
        if n.kind == "captureStarted" {
            self.listening_id = self.post(&n.title, &n.body, self.listening_id).await?;
            return Ok(());
        }
        self.post(&n.title, &n.body, 0).await.map(|_| ())
    }
}

// MARK: zorad and dialogs

struct Cli {
    zorad: String,
    config: Option<String>,
}

impl Cli {
    async fn run(&self, args: &[String]) -> Result<String, String> {
        let mut cmd = tokio::process::Command::new(&self.zorad);
        if let Some(c) = &self.config {
            cmd.args(["--config", c]);
        }
        cmd.args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
            Err(_) => Err(
                "zorad did not answer within 10 s; the change may or may not have been saved"
                    .into(),
            ),
            Ok(Err(e)) => Err(format!("cannot run {}: {e}", self.zorad)),
            Ok(Ok(o)) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).into()),
            Ok(Ok(o)) => {
                let e = String::from_utf8_lossy(&o.stderr).trim().to_string();
                Err(if e.is_empty() {
                    format!("zorad exited with {}", o.status)
                } else {
                    e
                })
            }
        }
    }
}

/// zorad may start before the desktop session exports its display variables to systemd; take them from there.
async fn session_env() -> Vec<(String, String)> {
    let mut env = vec![];
    let want = [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_CURRENT_DESKTOP",
    ];
    if want[..2].iter().any(|k| std::env::var_os(k).is_some()) {
        return env;
    }
    if let Ok(o) = tokio::process::Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .await
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some((k, v)) = line.split_once('=')
                && want.contains(&k)
            {
                env.push((k.to_string(), v.to_string()));
            }
        }
    }
    env
}

#[derive(Clone, Copy)]
enum Dialog {
    Zenity,
    KDialog,
}

fn which(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(name).is_file()))
}

fn dialog_tool() -> Option<Dialog> {
    let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.contains("KDE"));
    match (which("zenity"), which("kdialog")) {
        (_, true) if kde => Some(Dialog::KDialog),
        (true, _) => Some(Dialog::Zenity),
        (_, true) => Some(Dialog::KDialog),
        _ => None,
    }
}

/// Exit 1 is a cancel unless the dialog also complained: toolkits print warnings on a normal cancel (GTK's
/// `Gtk-WARNING`, `Gtk-Message`, theme parsing errors), so only a known failure inside one, or a line that isn't a
/// toolkit diagnostic at all, means it failed (bad options, no display).
fn looks_like_usage_error(stderr: &[u8]) -> bool {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|l| !l.is_empty())
        .any(|l| {
            // A warning can still be fatal (GTK 3: "Gtk-WARNING **: cannot open display").
            let fatal = [
                "cannot open display",
                "could not open display",
                "unable to open display",
                "could not connect to display",
                "unknown option",
                "usage:",
            ]
            .iter()
            .any(|w| l.contains(w));
            fatal
                || !["warning", "-message", "debug"]
                    .iter()
                    .any(|w| l.contains(w))
        })
}

/// Runs a dialog; `None` when the user cancelled (exit 1, as both zenity and kdialog report it). Any other
/// failure is an error, so a dialog that couldn't open isn't mistaken for a cancel.
async fn dialog(args: Vec<String>, tool: Dialog) -> Result<Option<String>, String> {
    let program = match tool {
        Dialog::Zenity => "zenity",
        Dialog::KDialog => "kdialog",
    };
    let out = tokio::process::Command::new(program)
        .args(&args)
        .envs(session_env().await)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    match out.status.code() {
        Some(0) => {}
        // kdialog also exits 1 on bad arguments, with a usage message; a cancel prints none.
        Some(1) if !looks_like_usage_error(&out.stderr) => return Ok(None),
        code => {
            let err = String::from_utf8_lossy(&out.stderr);
            let why = code.map_or("was killed".to_string(), |c| format!("exited with {c}"));
            return Err(format!("{program} {why}: {}", err.trim()));
        }
    }
    Ok(Some(
        String::from_utf8_lossy(&out.stdout)
            .trim_end_matches('\n')
            .to_string(),
    ))
}

/// An error dialog; when even that can't open, a notification instead (and always a log line).
async fn show_error(tool: Dialog, notifier: &mut Notifier, text: &str) {
    eprintln!("zora-desktop: {text}");
    let args = match tool {
        Dialog::Zenity => vec![
            "--error".into(),
            "--title=Zora".into(),
            "--no-markup".into(),
            format!("--text={text}"),
        ],
        Dialog::KDialog => vec![
            "--title".into(),
            "Zora".into(),
            "--error".into(),
            text.into(),
        ],
    };
    if let Err(e) = dialog(args, tool).await {
        eprintln!("zora-desktop: error dialog failed: {e}");
        let _ = notifier.post("Zora", text, 0).await;
    }
}

fn input_args(tool: Dialog, prompt: &str, current: &str) -> Vec<String> {
    match tool {
        Dialog::Zenity => vec![
            "--entry".into(),
            "--title=Zora Settings".into(),
            format!("--text={prompt}"),
            format!("--entry-text={current}"),
        ],
        Dialog::KDialog => vec![
            "--title".into(),
            "Zora Settings".into(),
            "--inputbox".into(),
            prompt.into(),
            // The current value is positional; `--` keeps one starting with a dash from reading as an option.
            "--".into(),
            current.into(),
        ],
    }
}

fn agent_args(tool: Dialog, current: &str) -> Vec<String> {
    let text = "Which coding agent runs your voice commands?";
    let choices = agent_choices(current);
    match tool {
        Dialog::Zenity => {
            let mut a: Vec<String> = [
                "--list",
                "--radiolist",
                "--title=Zora Settings",
                "--height=300",
                "--column=",
                "--column=id",
                "--column=Agent",
                "--hide-column=2",
                "--print-column=2",
            ]
            .map(String::from)
            .to_vec();
            a.insert(3, format!("--text={text}"));
            // Rows are tagged by index, so an id containing `|` (zenity's separator) still maps back exactly.
            // GLib keeps a `--` among the row cells, so a label starting with a dash (a configured id such as
            // `--help`) gets a leading space instead, which keeps it from reading as an option.
            for (i, (_, label, on)) in choices.into_iter().enumerate() {
                let label = if label.starts_with('-') {
                    format!(" {label}")
                } else {
                    label
                };
                a.extend([
                    if on { "TRUE" } else { "FALSE" }.into(),
                    i.to_string(),
                    label,
                ]);
            }
            a
        }
        Dialog::KDialog => {
            let mut a: Vec<String> = vec![
                "--title".into(),
                "Zora Settings".into(),
                "--radiolist".into(),
                text.into(),
                "--".into(),
            ];
            for (i, (_, label, on)) in choices.into_iter().enumerate() {
                a.extend([i.to_string(), label, if on { "on" } else { "off" }.into()]);
            }
            a
        }
    }
}

/// The harness id for what `agent_args`' dialog printed (zenity repeats the row on a double-click: `1|1`).
fn agent_picked(output: &str, current: &str) -> Option<String> {
    let i: usize = output.split('|').next()?.trim().parse().ok()?;
    agent_choices(current).into_iter().nth(i).map(|c| c.0)
}

fn folder_args(tool: Dialog, current: &str) -> Vec<String> {
    let start = if current.is_empty() {
        std::env::var("HOME").unwrap_or_default()
    } else {
        current.to_string()
    };
    match tool {
        Dialog::Zenity => vec![
            "--file-selection".into(),
            "--directory".into(),
            "--title=Zora: project folder".into(),
            format!("--filename={}/", start.trim_end_matches('/')),
        ],
        Dialog::KDialog => vec![
            "--title".into(),
            "Zora: project folder".into(),
            "--getexistingdirectory".into(),
            "--".into(),
            start,
        ],
    }
}

/// Comma- or newline-separated names, as `zorad settings set` expects them.
pub fn parse_names(input: &str) -> Vec<String> {
    input
        .split([',', '\n'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

async fn edit_setting(cli: &Cli, action: &Action, notifier: &mut Notifier) {
    let Some(tool) = dialog_tool() else {
        let hint = match action {
            Action::Names => "zorad names add <name>",
            Action::Agent => "zorad settings set '{\"harness\": \"codex\"}'",
            _ => "zorad settings set '{\"workspace\": \"~/projects/x\"}'",
        };
        let _ = notifier
            .post(
                "Zora: no dialog tool",
                &format!(
                    "Install zenity or kdialog to edit settings from the tray, or run: {hint}"
                ),
                0,
            )
            .await;
        return;
    };
    let current: Value = match cli.run(&["settings".into()]).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(e) => {
            return show_error(
                tool,
                notifier,
                &format!("Zora couldn't read its settings:\n{e}"),
            )
            .await;
        }
    };
    let change = match action {
        Action::Names => {
            let names: Vec<&str> = current["wakeNames"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let prompt =
                "Names Zora answers to, separated by commas.\nSay “Hey <name> … Do it <name>”.";
            match dialog(input_args(tool, prompt, &names.join(", ")), tool).await {
                Ok(Some(s)) => json!({"wakeNames": parse_names(&s)}),
                Ok(None) => return,
                Err(e) => return show_error(tool, notifier, &e).await,
            }
        }
        Action::Agent => {
            let current_id = current["harness"].as_str().unwrap_or_default();
            let id = match dialog(agent_args(tool, current_id), tool).await {
                Ok(Some(s)) => match agent_picked(&s, current_id) {
                    Some(id) => id,
                    None => return,
                },
                Ok(None) => return,
                Err(e) => return show_error(tool, notifier, &e).await,
            };
            if id != CUSTOM {
                json!({"harness": id})
            } else {
                let prompt = "Command for your agent. Your words go where {prompt} is, or at the end.\n\
                              No shell: quote words that contain spaces. Example: opencode run {prompt}";
                let cmd = current["customCommand"].as_str().unwrap_or_default();
                match dialog(input_args(tool, prompt, cmd), tool).await {
                    Ok(Some(s)) if !s.trim().is_empty() => {
                        json!({"harness": id, "customCommand": s.trim()})
                    }
                    Ok(_) => return,
                    Err(e) => return show_error(tool, notifier, &e).await,
                }
            }
        }
        Action::Folder => {
            // The folder belongs to the agent shown now; without its id the change could land on another one.
            let Some(agent) = current["harness"].as_str().filter(|h| !h.is_empty()) else {
                return show_error(
                    tool,
                    notifier,
                    "Zora couldn't tell which agent the folder is for.",
                )
                .await;
            };
            let agent = agent.to_string();
            let ws = current["workspace"].as_str().unwrap_or_default();
            let ws = match ws.strip_prefix("~/") {
                Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
                None => ws.to_string(),
            };
            match dialog(folder_args(tool, &ws), tool).await {
                Ok(Some(s)) if !s.trim().is_empty() => {
                    json!({"workspace": s.trim(), "expect": {"harness": agent}})
                }
                Ok(_) => return,
                Err(e) => return show_error(tool, notifier, &e).await,
            }
        }
        _ => return,
    };
    match cli
        .run(&["settings".into(), "set".into(), change.to_string()])
        .await
    {
        Ok(out) => {
            if let Some(warning) = zora_plugin::tray::unapplied(&out) {
                show_error(tool, notifier, &warning).await;
            }
        }
        Err(e) => show_error(tool, notifier, &format!("Zora couldn't save that:\n{e}")).await,
    }
}

// MARK: main loop

fn ack(id: u64, result: Result<(), String>) -> Value {
    let mut ack = json!({"t": "ack", "id": id, "ok": result.is_ok()});
    if let Err(e) = result {
        eprintln!("zora-desktop: notification failed: {e}");
        ack["error"] = json!(e);
    }
    ack
}

enum Ev {
    Core(CoreMsg),
    Eof,
    Action(Action),
    PauseDone(Request, Result<(), String>),
}

pub async fn run(config: Value, out: Arc<Out>, mut core_rx: UnboundedReceiver<CoreMsg>) -> i32 {
    let opts = Options::from_config(&config);
    let cli = Arc::new(Cli {
        zorad: opts.zorad.clone(),
        config: opts.zorad_config.clone(),
    });
    let model = Arc::new(Mutex::new(TrayModel::new()));
    let (action_tx, mut action_rx) = tokio::sync::mpsc::unbounded_channel::<Action>();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<Ev>();
    let mut tray: Option<ksni::Handle<ZoraTray>> = None;
    let mut next_tray_try = Instant::now();
    // Notifications go through one worker in order, so a slow notification service never holds up pause,
    // stop or the end of input. The core keeps at most 16 unacknowledged, so the queue never fills in practice.
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::channel::<(Notify, bool)>(64);
    {
        let out = out.clone();
        let enabled = opts.notifications;
        tokio::spawn(async move {
            let mut notifier = Notifier {
                conn: None,
                listening_id: 0,
            };
            while let Some((n, show)) = notify_rx.recv().await {
                let result = if enabled {
                    notifier.notify(&n, show).await
                } else {
                    Ok(())
                };
                out.send(ack(n.id, result));
            }
        });
    }
    let mut ticker = tokio::time::interval(Duration::from_secs(1));

    loop {
        if opts.tray
            && tray.as_ref().is_none_or(|t| t.is_closed())
            && Instant::now() >= next_tray_try
        {
            let t = ZoraTray {
                model: model.clone(),
                tx: action_tx.clone(),
            };
            match t.assume_sni_available(true).spawn().await {
                Ok(h) => tray = Some(h),
                Err(e) => {
                    eprintln!("zora-desktop: no tray icon yet: {e}");
                    tray = None;
                    next_tray_try = Instant::now() + Duration::from_secs(30);
                }
            }
        }
        let ev = tokio::select! {
            m = core_rx.recv() => m.map(Ev::Core).unwrap_or(Ev::Eof),
            Some(a) = action_rx.recv() => Ev::Action(a),
            Some(d) = done_rx.recv() => d,
            _ = ticker.tick() => {
                if model.lock().unwrap().tick(Instant::now())
                    && let Some(t) = &tray
                {
                    t.update(|_| {}).await;
                }
                continue;
            }
        };
        match ev {
            Ev::Eof => break,
            Ev::Core(CoreMsg::Start {
                listening,
                until,
                epoch,
            }) => {
                model
                    .lock()
                    .unwrap()
                    .set_core_listening(listening, until, epoch, None);
            }
            Ev::Core(CoreMsg::Listening {
                listening,
                until,
                epoch,
                confirmed,
            }) => {
                model
                    .lock()
                    .unwrap()
                    .set_core_listening(listening, until, epoch, confirmed);
            }
            Ev::Core(CoreMsg::Notify(n)) => {
                let show = model
                    .lock()
                    .unwrap()
                    .handle(&n.kind, &n.title, &n.body, Instant::now());
                let id = n.id;
                if notify_tx.try_send((n, show)).is_err() {
                    out.send(ack(id, Err("too many notifications waiting".into())));
                }
            }
            Ev::Core(CoreMsg::Clock { id }) => {
                out.send(clock_reply(id));
            }
            Ev::Core(CoreMsg::Stop) => {
                if let Some(t) = tray.take() {
                    t.shutdown().await;
                }
                out.send(json!({"t": "stopped"}));
            }
            Ev::Core(_) => {}
            Ev::Action(Action::Pause(req)) => {
                if model.lock().unwrap().begin() {
                    let (cli, tx) = (cli.clone(), done_tx.clone());
                    tokio::spawn(async move {
                        let r = cli.run(&req.args()).await.map(|_| ());
                        let _ = tx.send(Ev::PauseDone(req, r));
                    });
                }
            }
            Ev::PauseDone(req, r) => {
                if let Err(e) = &r {
                    eprintln!("zora-desktop: zorad {:?} failed: {e}", req.args());
                }
                model.lock().unwrap().finish(&req, r, Instant::now());
            }
            Ev::Action(a @ (Action::Names | Action::Agent | Action::Folder)) => {
                // Dialogs run beside the loop so notifications keep flowing; at most one at a time.
                let cli = cli.clone();
                tokio::spawn(async move {
                    static OPEN: Mutex<bool> = Mutex::new(false);
                    {
                        let mut open = OPEN.lock().unwrap();
                        if *open {
                            return;
                        }
                        *open = true;
                    }
                    let mut n = Notifier {
                        conn: None,
                        listening_id: 0,
                    };
                    edit_setting(&cli, &a, &mut n).await;
                    *OPEN.lock().unwrap() = false;
                });
            }
            Ev::Action(Action::Restart) => {
                let _ = tokio::process::Command::new("systemctl")
                    .args(["--user", "--no-block", "restart", "zora.service"])
                    .status()
                    .await;
            }
        }
        if let Some(t) = &tray {
            t.update(|_| {}).await;
        }
    }
    if let Some(t) = tray.take() {
        t.shutdown().await;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_split_and_trimmed() {
        assert_eq!(parse_names(" zora, nova ,,\nkai "), ["zora", "nova", "kai"]);
        assert!(parse_names(" , ").is_empty());
    }

    #[test]
    fn kdialog_values_cannot_become_options() {
        let a = input_args(Dialog::KDialog, "Names", "-x, zora");
        assert_eq!(&a[a.len() - 2..], ["--", "-x, zora"]);
        let a = folder_args(Dialog::KDialog, "-dir");
        assert_eq!(&a[a.len() - 2..], ["--", "-dir"]);
        let a = input_args(Dialog::Zenity, "Names", "-x");
        assert!(a.contains(&"--entry-text=-x".to_string()));
    }

    #[test]
    fn agent_list_marks_the_current_one() {
        let a = agent_args(Dialog::KDialog, "codex");
        assert!(a.windows(3).any(|w| w == ["1", "Codex", "on"]));
        assert!(a.windows(3).any(|w| w == ["0", "Claude Code", "off"]));
        let a = agent_args(Dialog::Zenity, "pi|team");
        assert!(a.windows(3).any(|w| w == ["TRUE", "4", "pi|team"]));
        assert_eq!(a.iter().filter(|x| *x == "TRUE").count(), 1);
        // No row cell can read as an option: zenity gets no `--` and a dash-led label is padded; kdialog gets `--`.
        let a = agent_args(Dialog::Zenity, "--help");
        assert!(!a.contains(&"--".to_string()));
        assert!(a.windows(3).any(|w| w == ["TRUE", "4", " --help"]));
        let a = agent_args(Dialog::KDialog, "--help");
        let rows = a.iter().position(|x| x == "--").unwrap();
        assert_eq!(a[rows + 1..].len(), 3 * 5);
        assert!(a.windows(3).any(|w| w == ["4", "--help", "on"]));
        // Rows come back by index, so an id containing `|` maps back exactly.
        assert_eq!(agent_picked("4|4", "pi|team").as_deref(), Some("pi|team"));
        assert_eq!(agent_picked("2", "pi|team").as_deref(), Some("pi"));
        assert_eq!(agent_picked("9", "claude"), None);
        assert_eq!(agent_picked("pi", "claude"), None);
    }

    #[test]
    fn exit_1_is_a_cancel_only_without_complaints() {
        assert!(!looks_like_usage_error(b""));
        assert!(!looks_like_usage_error(
            b"(zenity:42): Gtk-WARNING **: no parent\nGtk-Message: 10:00: GtkDialog mapped\n"
        ));
        assert!(looks_like_usage_error(b"kdialog: Unknown option 'x'.\n"));
        assert!(looks_like_usage_error(b"cannot open display\n"));
        assert!(looks_like_usage_error(
            b"(zenity:42): Gtk-WARNING **: cannot open display: :0\n"
        ));
        assert!(!looks_like_usage_error(
            b"(zenity:42): Gtk-WARNING **: Theme parsing error: gtk.css:12: unknown property\n"
        ));
    }

    #[test]
    fn body_markup_is_escaped() {
        assert_eq!(escape("a <b> & c"), "a &lt;b&gt; &amp; c");
    }

    #[test]
    fn every_state_has_an_icon() {
        for i in [
            Icon::Idle,
            Icon::Paused,
            Icon::Listening,
            Icon::Running,
            Icon::Done,
            Icon::Failed,
            Icon::Degraded,
        ] {
            assert!(!icon_name(i).is_empty());
            let p = pixmap(i);
            assert_eq!(p.data.len(), (p.width * p.height * 4) as usize);
        }
    }

    #[test]
    fn options_default_to_tray_and_notifications() {
        let o = Options::from_config(&json!({"zorad": "/x/zorad"}));
        assert!(o.tray && o.notifications);
        assert_eq!(o.zorad, "/x/zorad");
        let o = Options::from_config(&json!({"tray": false}));
        assert!(!o.tray);
    }
}

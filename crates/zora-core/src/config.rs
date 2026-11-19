//! JSON config, compatible with v1 (`~/.config/zora/config.json`), plus a `plugins` section.

use crate::detector::DetectorConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Phrases {
    pub start: Vec<String>,
    pub end: Vec<String>,
    pub cancel: Vec<String>,
}

impl Default for Phrases {
    fn default() -> Self {
        Phrases {
            start: vec!["hey zora".into()],
            end: vec!["do it zora".into(), "zora do it".into()],
            cancel: [
                "zora cancel",
                "cancel zora",
                "dismiss zora",
                "zora dismiss",
                "stop zora",
                "zora stop",
                "stop it zora",
                "never mind zora",
            ]
            .map(String::from)
            .to_vec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Capture {
    pub timeout_seconds: f64,
    pub max_gap_seconds: f64,
    pub max_chars: usize,
    pub hold_back_seconds: f64,
}

impl Default for Capture {
    fn default() -> Self {
        Capture {
            timeout_seconds: 180.0,
            max_gap_seconds: 30.0,
            max_chars: 4000,
            hold_back_seconds: 1.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Dictation {
    /// macOS: bundle IDs of dictation apps.
    #[serde(rename = "bundleIDs")]
    pub bundle_ids: Vec<String>,
    /// Linux: PipeWire application.process.binary / application.name / application.id values.
    pub linux_apps: Vec<String>,
    /// Windows: process image names (e.g. `MyDictation.exe`) whose capture sessions mean dictation.
    pub windows_apps: Vec<String>,
    /// Windows: use the dictation monitor, which is not yet verified on hardware. Off, it reports health
    /// unknown, so no command runs (unless `monitor` is "none").
    pub windows_experimental: bool,
    pub pad_seconds: f64,
    /// "plugin" (default) or "none" (no dictation tool: always healthy, never dictating).
    pub monitor: String,
}

impl Default for Dictation {
    fn default() -> Self {
        Dictation {
            bundle_ids: vec!["com.typewhisper.mac".into()],
            linux_apps: vec![],
            windows_apps: vec![],
            windows_experimental: false,
            pad_seconds: 1.0,
            monitor: "plugin".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Harness {
    /// "claude", "codex", "pi" (built-in command lines) or "argv" (`executable` + `args`).
    #[serde(rename = "type")]
    pub kind: String,
    pub workspace: String,
    /// Empty: the type's own program (`claude`, `codex`, `pi`); for `argv`, `claude` as before.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub executable: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub allowed_tools: Vec<String>,
    pub extra_args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl Default for Harness {
    fn default() -> Self {
        Harness {
            kind: "claude".into(),
            workspace: String::new(),
            executable: String::new(),
            args: vec![],
            permission_mode: None,
            allowed_tools: vec![],
            extra_args: vec![],
            env: BTreeMap::new(),
        }
    }
}

/// Harness types with a built-in command line; the settings offer each one under its own id.
pub const BUILTIN_HARNESSES: [&str; 3] = ["claude", "codex", "pi"];

/// The harness id the settings use for a user-supplied command line.
pub const CUSTOM_HARNESS: &str = "custom";

impl Harness {
    /// A built-in harness type with its defaults.
    pub fn preset(kind: &str) -> Option<Harness> {
        BUILTIN_HARNESSES.contains(&kind).then(|| Harness {
            kind: kind.into(),
            ..Harness::default()
        })
    }

    /// The program to run: `executable`, else the type's own (an `argv` entry without one ran `claude`).
    pub fn program(&self) -> &str {
        match (self.executable.is_empty(), self.kind.as_str()) {
            (false, _) => &self.executable,
            (true, "argv") => "claude",
            (true, kind) => kind,
        }
    }

    /// The agent this harness starts, by type or by the program's file name.
    fn agent(&self) -> &str {
        match self.kind.as_str() {
            "argv" => {
                let file = self
                    .program()
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default();
                let stem = file.split('.').next().unwrap_or_default();
                match stem.to_ascii_lowercase().as_str() {
                    "claude" => "claude",
                    "codex" => "codex",
                    "pi" => "pi",
                    _ => "",
                }
            }
            kind => kind,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Jobs {
    pub timeout_seconds: f64,
    pub max_queue: usize,
    pub max_age_seconds: f64,
}

impl Default for Jobs {
    fn default() -> Self {
        Jobs {
            timeout_seconds: 900.0,
            max_queue: 8,
            max_age_seconds: 600.0,
        }
    }
}

/// An external plugin executable (Linux mode). On macOS the app attaches itself instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PluginSpec {
    pub command: Vec<String>,
    /// Plugin-specific settings passed in `hello.config`.
    pub config: serde_json::Value,
}

impl Default for PluginSpec {
    fn default() -> Self {
        PluginSpec {
            command: vec![],
            config: serde_json::Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub locale: String,
    /// Names Zora answers to. Phrases use "zora" as the name slot; every name here (and its sound-alikes)
    /// fills it, so "hey nova … do it nova" works when "nova" is listed.
    pub wake_names: Vec<String>,
    pub retention_hours: f64,
    pub dry_run: bool,
    pub speech_detector: bool,
    pub path: String,
    pub phrases: Phrases,
    pub capture: Capture,
    pub dictation: Dictation,
    pub default_harness: String,
    pub harnesses: BTreeMap<String, Harness>,
    pub jobs: Jobs,
    pub notifications: BTreeMap<String, Vec<String>>,
    pub plugins: BTreeMap<String, PluginSpec>,
}

impl Default for Config {
    fn default() -> Self {
        let mut notifications = BTreeMap::new();
        notifications.insert(
            "macos".into(),
            [
                "captureStarted",
                "commandQueued",
                "jobStarted",
                "jobSucceeded",
                "jobFailed",
                "captureDiscarded",
                "degraded",
            ]
            .map(String::from)
            .to_vec(),
        );
        notifications.insert("desktop".into(), notifications["macos"].clone());
        notifications.insert("log".into(), vec!["*".into()]);
        Config {
            locale: "en-US".into(),
            wake_names: vec!["zora".into(), "nova".into()],
            retention_hours: 4.0,
            dry_run: false,
            speech_detector: true,
            path: DEFAULT_PATH.into(),
            phrases: Phrases::default(),
            capture: Capture::default(),
            dictation: Dictation::default(),
            default_harness: "claude".into(),
            harnesses: BTreeMap::from([("claude".to_string(), Harness::default())]),
            jobs: Jobs::default(),
            notifications,
            plugins: BTreeMap::new(),
        }
    }
}

/// Matches the installed Claude Code (`claude --help`); `bypassPermissions` is deliberately excluded.
pub const ALLOWED_PERMISSION_MODES: [&str; 5] =
    ["acceptEdits", "auto", "dontAsk", "manual", "plan"];

#[cfg(unix)]
const DEFAULT_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:~/.local/bin:/usr/bin:/bin";
/// The native Claude Code installer, npm globals, then the system.
#[cfg(windows)]
const DEFAULT_PATH: &str = "~/.local/bin;~/AppData/Roaming/npm;C:/Program Files/nodejs;C:/Program Files/Git/cmd;C:/Windows/System32;C:/Windows;C:/Windows/System32/WindowsPowerShell/v1.0";

/// Separator of the `path` setting and of `PATH`.
pub const PATH_SEPARATOR: &str = if cfg!(windows) { ";" } else { ":" };

/// `HOME`; on Windows `USERPROFILE` first.
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    if let Some(p) = std::env::var_os("USERPROFILE").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn expand_tilde(p: &str) -> String {
    let rest = p
        .strip_prefix("~/")
        .or_else(|| p.strip_prefix("~\\").filter(|_| cfg!(windows)));
    if let Some(rest) = rest {
        if let Some(home) = home_dir() {
            return home.join(rest).to_string_lossy().into_owned();
        }
    } else if p == "~"
        && let Some(home) = home_dir()
    {
        return home.to_string_lossy().into_owned();
    }
    p.to_string()
}

/// A Windows known folder from its environment variable, else its usual place under the profile.
#[cfg(windows)]
pub fn windows_dir(var: &str, fallback: &[&str]) -> PathBuf {
    std::env::var_os(var)
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = home_dir().unwrap_or_else(|| PathBuf::from("."));
            fallback.iter().fold(home, |p, c| p.join(c))
        })
}

/// Creates a new file only its owner can read: mode 0600 on Unix. Windows has no mode; the file inherits the ACL
/// of its folder, which under the user profile (`%APPDATA%`, `%LOCALAPPDATA%`) grants only the user and the system.
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

/// Creates a folder (and its parents) only its owner can enter: mode 0700 on Unix; see `create_private_file`.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

impl Config {
    /// `~/.config/zora/config.json`; on Windows `%APPDATA%\Zora\config.json`.
    pub fn default_path() -> PathBuf {
        #[cfg(windows)]
        return windows_dir("APPDATA", &["AppData", "Roaming"])
            .join("Zora")
            .join("config.json");
        #[cfg(unix)]
        home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config/zora/config.json")
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let data =
            std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        serde_json::from_slice(&data).map_err(|e| format!("cannot parse {}: {e}", path.display()))
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        write_file(
            path,
            &serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?,
        )
    }

    /// Writes the default config unless a file is already there, under the same lock as `update`, so it never
    /// replaces a config another writer just created. Returns whether it wrote.
    pub fn create_default(path: &Path) -> Result<bool, String> {
        let _lock = lock_config(path)?;
        if path.exists() {
            return Ok(false);
        }
        Config::default().write(path)?;
        Ok(true)
    }

    /// Changes the config file in place: `edit` patches the file's own JSON, so keys it doesn't touch (including
    /// ones this version doesn't know) are kept. `accept` sees the config before and after and may refuse. An
    /// exclusive lock on `<config>.lock` is held from the read to the rename, so concurrent writers serialize.
    pub fn update(
        path: &Path,
        edit: impl FnOnce(&mut serde_json::Value, &Config) -> Result<(), UpdateError>,
        accept: impl FnOnce(&Config, &Config) -> Result<(), UpdateError>,
    ) -> Result<Config, UpdateError> {
        let lock = lock_config(path).map_err(UpdateError::Io)?;
        let mut doc = match std::fs::read(path) {
            Ok(data) => serde_json::from_slice(&data)
                .map_err(|e| UpdateError::Io(format!("cannot parse {}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                serde_json::to_value(Config::default())
                    .map_err(|e| UpdateError::Io(e.to_string()))?
            }
            Err(e) => {
                return Err(UpdateError::Io(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        if !doc.is_object() {
            return Err(UpdateError::Io(format!(
                "{} is not a JSON object",
                path.display()
            )));
        }
        let before: Config = serde_json::from_value(doc.clone())
            .map_err(|e| UpdateError::Io(format!("cannot parse {}: {e}", path.display())))?;
        edit(&mut doc, &before)?;
        let after: Config = serde_json::from_value(doc.clone())
            .map_err(|e| UpdateError::Invalid(vec![e.to_string()]))?;
        accept(&before, &after)?;
        let data = serde_json::to_vec_pretty(&doc).map_err(|e| UpdateError::Io(e.to_string()))?;
        write_file(path, &data).map_err(UpdateError::Io)?;
        drop(lock);
        Ok(after)
    }
}

/// Why `Config::update` changed nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateError {
    /// The file no longer holds the values the caller expected (someone else changed it).
    Conflict(String),
    /// The change would make the config invalid.
    Invalid(Vec<String>),
    Io(String),
}

/// Takes the exclusive lock on `<config>.lock`; it is released when the returned file is dropped.
fn lock_config(path: &Path) -> Result<std::fs::File, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("json.lock"))
        .map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    Ok(lock)
}

/// Writes through a private temp file and an atomic rename; a replaced file keeps its own mode.
fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    // Unique per writer, so concurrent saves never write into the same temp file.
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    // Private from the first byte (it may hold harness env values); a replaced file keeps its own mode.
    {
        use std::io::Write;
        let mut f = create_private_file(&tmp).map_err(|e| e.to_string())?;
        f.write_all(data).map_err(|e| e.to_string())?;
    }
    if let Ok(meta) = std::fs::metadata(path)
        && let Err(e) = std::fs::set_permissions(&tmp, meta.permissions())
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })
}

impl Config {
    pub fn detector_config(&self) -> DetectorConfig {
        DetectorConfig::new(
            &self.phrases.start,
            &self.phrases.end,
            &self.phrases.cancel,
            self.capture.timeout_seconds,
            self.capture.max_gap_seconds,
            self.capture.max_chars,
        )
        .with_names(&self.wake_names)
    }

    pub fn search_path(&self) -> Vec<String> {
        self.path
            .split(PATH_SEPARATOR)
            .filter(|s| !s.is_empty())
            .map(expand_tilde)
            .collect()
    }

    /// Human-readable problems; empty means valid.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.locale.trim().is_empty() {
            problems.push("locale must not be empty".to_string());
        }
        if self.retention_hours <= 0.0 {
            problems.push("retentionHours must be > 0".into());
        }
        let d = self.detector_config();
        if d.start.is_empty() || d.end.is_empty() {
            problems.push("phrases.start and phrases.end must not be empty".into());
        }
        if self.wake_names.is_empty() {
            problems.push("wakeNames must list at least one name".into());
        }
        for n in &self.wake_names {
            let t = crate::detector::tokens(n);
            if t.len() != 1 || t[0].chars().count() < 3 {
                problems.push(format!(
                    "name '{n}' must be one word of at least 3 letters (wakeNames)"
                ));
            }
        }
        if self.capture.hold_back_seconds < 0.0 {
            problems.push("capture.holdBackSeconds must be >= 0".into());
        }
        if self.jobs.max_queue < 1 {
            problems.push("jobs.maxQueue must be >= 1".into());
        }
        if !matches!(self.dictation.monitor.as_str(), "plugin" | "none") {
            problems.push("dictation.monitor must be 'plugin' or 'none'".into());
        }
        let Some(harness) = self.harnesses.get(&self.default_harness) else {
            problems.push(format!(
                "defaultHarness '{}' is not defined in harnesses",
                self.default_harness
            ));
            return problems;
        };
        for (name, h) in &self.harnesses {
            problems.extend(policy_problems(name, h));
        }
        if !self.dry_run {
            let ws = expand_tilde(&harness.workspace);
            if harness.workspace.is_empty() || !Path::new(&ws).is_dir() {
                problems.push(format!(
                    "harnesses.{}.workspace must be an existing directory (got '{}')",
                    self.default_harness, harness.workspace
                ));
            } else if harness.kind == "codex"
                && !harness
                    .extra_args
                    .iter()
                    .any(|a| a == "--skip-git-repo-check")
                && !Path::new(&ws).ancestors().any(|d| d.join(".git").exists())
            {
                problems.push(format!(
                    "harnesses.{}.workspace must be inside a Git repository for Codex (got '{}'), or add \
                     --skip-git-repo-check to its extraArgs",
                    self.default_harness, harness.workspace
                ));
            }
        }
        problems
    }
}

pub fn policy_problems(name: &str, h: &Harness) -> Vec<String> {
    let mut problems = Vec::new();
    let mut everything: Vec<&str> = h
        .args
        .iter()
        .chain(&h.extra_args)
        .chain(&h.allowed_tools)
        .map(String::as_str)
        .collect();
    everything.push(h.permission_mode.as_deref().unwrap_or(""));
    everything.push(&h.executable);
    // Claude Code's and Codex's ways of skipping their permission checks or sandbox, anywhere.
    const BYPASS: [&str; 4] = [
        "dangerously-",
        "bypassPermissions",
        "danger-full-access",
        "--yolo",
    ];
    // Options that switch off an agent's own safety rules, or move or widen where it may write: Codex's
    // execution-policy rules, project trust (which lets MCP commands run outside the sandbox), and working folder;
    // Pi's trust in project-local extensions and settings. Nobody is there to review the project's files.
    let agent_flags: &[&str] = match h.agent() {
        "codex" => &[
            "--ignore-rules",
            "--trust",
            "--cd",
            "-C",
            "--add-dir",
            "--worktree",
        ],
        "pi" => &["--approve", "-a"],
        _ => &[],
    };
    let args: Vec<&str> = h
        .args
        .iter()
        .chain(&h.extra_args)
        .map(String::as_str)
        .collect();
    if everything
        .iter()
        .any(|a| BYPASS.iter().any(|b| a.contains(b)))
        || args.iter().any(|a| {
            agent_flags.iter().any(|f| {
                // `--flag`, `--flag=x`, and a short flag with its value attached (`-C/dir`).
                a == f || a.starts_with(&format!("{f}=")) || (f.len() == 2 && a.starts_with(f))
            })
        })
    {
        problems.push(format!(
            "harnesses.{name}: permission-bypass flags are not allowed"
        ));
    }
    if h.allowed_tools
        .iter()
        .any(|t| t.is_empty() || t.starts_with('-'))
    {
        problems.push(format!(
            "harnesses.{name}.allowedTools entries must be tool names, not options"
        ));
    }
    if h.env
        .iter()
        .any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0'))
    {
        problems.push(format!(
            "harnesses.{name}.env names must be non-empty without '=', and no name or value may contain NUL"
        ));
    }
    if h.kind != "claude" && (h.permission_mode.is_some() || !h.allowed_tools.is_empty()) {
        problems.push(format!(
            "harnesses.{name}: permissionMode and allowedTools apply only to type 'claude'"
        ));
    }
    match h.kind.as_str() {
        "claude" => {
            if let Some(mode) = &h.permission_mode
                && !ALLOWED_PERMISSION_MODES.contains(&mode.as_str())
            {
                problems.push(format!(
                    "harnesses.{name}.permissionMode '{mode}' not in {ALLOWED_PERMISSION_MODES:?}"
                ));
            }
            if h.args
                .iter()
                .chain(&h.extra_args)
                .any(|a| a.starts_with("--permission-mode"))
            {
                problems.push(format!(
                    "harnesses.{name}: set permissionMode instead of passing --permission-mode"
                ));
            }
        }
        "codex" | "pi" => {}
        "argv" => {
            if !h.args.iter().any(|a| a == "{prompt}") {
                problems.push(format!("harnesses.{name}.args must contain \"{{prompt}}\""));
            }
        }
        other => problems.push(format!(
            "harnesses.{name}.type '{other}' must be 'claude', 'codex', 'pi' or 'argv'"
        )),
    }
    problems
}

/// Splits a command line typed in the settings into argv, without a shell: whitespace separates words, '…' quotes
/// literally, and "…" quotes with the Windows rules: backslashes are literal unless they precede a `"`, where each
/// pair gives one backslash and an odd one makes the quote literal. So Windows and UNC paths need no escaping.
pub fn split_command(line: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => w.push(c),
                        None => return Err("unclosed ' in the command".into()),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => {
                            let mut n = 1;
                            while chars.next_if_eq(&'\\').is_some() {
                                n += 1;
                            }
                            if chars.peek() == Some(&'"') {
                                w.extend(std::iter::repeat_n('\\', n / 2));
                                if n % 2 == 1 {
                                    w.push(chars.next().unwrap());
                                }
                            } else {
                                w.extend(std::iter::repeat_n('\\', n));
                            }
                        }
                        Some(c) => w.push(c),
                        None => return Err("unclosed \" in the command".into()),
                    }
                }
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Ok(words)
}

/// The inverse of `split_command`, quoting only the words that need it.
pub fn join_command(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && !a.contains(|c: char| c.is_whitespace() || c == '\'' || c == '"') {
                return a.clone();
            }
            if !a.contains('\'') {
                return format!("'{a}'");
            }
            // Backslashes double only before a quote, or before the closing one.
            let mut out = String::from('"');
            let mut slashes = 0;
            for c in a.chars() {
                match c {
                    '\\' => slashes += 1,
                    '"' => {
                        out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                        out.push('"');
                        slashes = 0;
                    }
                    c => {
                        out.extend(std::iter::repeat_n('\\', slashes));
                        out.push(c);
                        slashes = 0;
                    }
                }
            }
            out.extend(std::iter::repeat_n('\\', slashes * 2));
            out.push('"');
            out
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A custom harness from a command line; `{prompt}` goes last when the line doesn't place it.
pub fn custom_harness(line: &str, workspace: &str) -> Result<Harness, String> {
    let mut argv = split_command(line)?;
    match argv.first().map(|p| p.trim()) {
        None => return Err("the custom command must not be empty".into()),
        Some("" | "{prompt}") => return Err("the custom command must start with a program".into()),
        Some(_) => {}
    }
    if !argv.iter().skip(1).any(|a| a == "{prompt}") {
        argv.push("{prompt}".into());
    }
    Ok(Harness {
        kind: "argv".into(),
        workspace: workspace.into(),
        executable: argv.remove(0),
        args: argv,
        ..Harness::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn update_patches_the_file_and_keeps_what_it_does_not_know() {
        let dir = std::env::temp_dir().join(format!("zora-config-{}", uuid::Uuid::new_v4()));
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{"futureKey": {"a": 1}, "wakeNames": ["zora"], "retentionHours": 2}"#,
        )
        .unwrap();
        let after = Config::update(
            &path,
            |doc, _| {
                doc["wakeNames"] = json!(["zora", "nova"]);
                Ok(())
            },
            |_, _| Ok(()),
        )
        .unwrap();
        assert_eq!(after.wake_names, ["zora", "nova"]);
        let doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            doc,
            json!({"futureKey": {"a": 1}, "wakeNames": ["zora", "nova"], "retentionHours": 2})
        );
        // A refused change writes nothing.
        let before = std::fs::read(&path).unwrap();
        let refused = Config::update(
            &path,
            |doc, _| {
                doc["wakeNames"] = json!([]);
                Ok(())
            },
            |_, _| Err(UpdateError::Invalid(vec!["no".into()])),
        );
        assert!(refused.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

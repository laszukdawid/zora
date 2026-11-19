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
    pub pad_seconds: f64,
    /// "plugin" (default) or "none" (no dictation tool: always healthy, never dictating).
    pub monitor: String,
}

impl Default for Dictation {
    fn default() -> Self {
        Dictation {
            bundle_ids: vec!["com.typewhisper.mac".into()],
            linux_apps: vec![],
            pad_seconds: 1.0,
            monitor: "plugin".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Harness {
    #[serde(rename = "type")]
    pub kind: String,
    pub workspace: String,
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
            executable: "claude".into(),
            args: vec![],
            permission_mode: None,
            allowed_tools: vec![],
            extra_args: vec![],
            env: BTreeMap::new(),
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
        let data = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
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
            f.write_all(&data).map_err(|e| e.to_string())?;
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
    if everything
        .iter()
        .any(|a| a.contains("dangerously-skip-permissions") || a.contains("bypassPermissions"))
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
        "argv" => {
            if !h.args.iter().any(|a| a == "{prompt}") {
                problems.push(format!("harnesses.{name}.args must contain \"{{prompt}}\""));
            }
        }
        other => problems.push(format!(
            "harnesses.{name}.type '{other}' must be 'claude' or 'argv'"
        )),
    }
    problems
}

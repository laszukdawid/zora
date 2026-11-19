//! Builds harness argv (no shell; `{prompt}` is one argument) and summarizes results.

use crate::config::{Config, Harness, expand_tilde};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSpec {
    pub id: String,
    pub workspace: String,
    pub executable: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub claude_json: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub summary: String,
}

impl HarnessSpec {
    pub fn from_config(id: &str, h: &Harness, config: &Config) -> HarnessSpec {
        let (args, claude_json) = if h.kind == "claude" {
            // Plain text: Claude prints only its final answer. (The JSON format is a long event list whose
            // shape changes between versions and can exceed the captured output.)
            let mut a: Vec<String> = ["-p", "{prompt}", "--output-format", "text"]
                .map(String::from)
                .to_vec();
            if let Some(mode) = &h.permission_mode {
                a.extend(["--permission-mode".to_string(), mode.clone()]);
            }
            if !h.allowed_tools.is_empty() {
                a.push("--allowedTools".into());
                a.extend(h.allowed_tools.iter().cloned());
            }
            a.extend(h.extra_args.iter().cloned());
            (a, true)
        } else {
            (h.args.clone(), false)
        };
        HarnessSpec {
            id: id.to_string(),
            workspace: expand_tilde(&h.workspace),
            executable: h.executable.clone(),
            args,
            env: h.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            claude_json,
            dry_run: config.dry_run,
        }
    }

    pub fn argv(&self, prompt: &str) -> Vec<String> {
        self.args
            .iter()
            .map(|a| {
                if a == "{prompt}" {
                    prompt.to_string()
                } else {
                    a.clone()
                }
            })
            .collect()
    }

    pub fn summarize(&self, exit_code: i32, stdout: &str, stderr: &str) -> HarnessResult {
        if self.claude_json {
            // Claude Code prints either one result object or (newer versions) an array of events whose last
            // `"type": "result"` element carries the answer.
            #[derive(serde::Deserialize)]
            struct Out {
                #[serde(rename = "type")]
                kind: Option<String>,
                result: Option<String>,
                is_error: Option<bool>,
            }
            let trimmed = stdout.trim();
            let out = serde_json::from_str::<Out>(trimmed)
                .ok()
                .or_else(|| {
                    serde_json::from_str::<Vec<serde_json::Value>>(trimmed)
                        .ok()?
                        .into_iter()
                        .rev()
                        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("result"))
                        .and_then(|e| serde_json::from_value::<Out>(e).ok())
                })
                .or_else(|| last_result_object(trimmed));
            if let Some(out) = out.filter(|o| o.kind.as_deref().is_none_or(|k| k == "result")) {
                let ok = exit_code == 0 && out.is_error != Some(true);
                let text = out.result.unwrap_or_default().trim().to_string();
                let summary = if text.is_empty() {
                    if ok { "done" } else { "failed" }.to_string()
                } else {
                    text
                };
                return HarnessResult {
                    success: ok,
                    exit_code: Some(exit_code),
                    summary,
                };
            }
        }
        if self.claude_json {
            // Plain-text answer: the whole reply is the summary; the exit code says whether it worked.
            let text = stdout.trim();
            if !text.is_empty() {
                return HarnessResult {
                    success: exit_code == 0,
                    exit_code: Some(exit_code),
                    summary: if exit_code == 0 {
                        text.to_string()
                    } else {
                        format!("exit {exit_code}: {text}")
                    },
                };
            }
        }
        last_line_summary(exit_code, stdout, stderr)
    }
}

/// Output may be truncated at the front (only the tail is kept), so the array is not valid JSON as a whole:
/// parse the object that starts at the last `{"type":"result"` marker.
fn last_result_object<T: serde::de::DeserializeOwned>(text: &str) -> Option<T> {
    let at = text.rfind("{\"type\":\"result\"")?;
    serde_json::Deserializer::from_str(&text[at..])
        .into_iter::<T>()
        .next()?
        .ok()
}

pub fn last_line_summary(exit_code: i32, stdout: &str, stderr: &str) -> HarnessResult {
    let source = if stdout.trim().is_empty() {
        stderr
    } else {
        stdout
    };
    let line = source
        .lines()
        .rfind(|l| !l.is_empty())
        .unwrap_or("no output")
        .to_string();
    HarnessResult {
        success: exit_code == 0,
        exit_code: Some(exit_code),
        summary: if exit_code == 0 {
            line
        } else {
            format!("exit {exit_code}: {line}")
        },
    }
}

/// The command line that starts `executable`: the resolved program, or on Windows what an npm shim starts.
pub fn resolve_command(executable: &str, search_path: &[String]) -> Result<Vec<String>, String> {
    let exe = resolve_executable(executable, search_path)
        .ok_or_else(|| format!("{executable} not found on PATH"))?;
    #[cfg(windows)]
    {
        let lower = exe.to_ascii_lowercase();
        if lower.ends_with(".cmd") || lower.ends_with(".bat") {
            return windows_shim(&exe, search_path);
        }
    }
    Ok(vec![exe])
}

/// Resolves a bare executable name against the configured search path.
#[cfg(unix)]
pub fn resolve_executable(executable: &str, search_path: &[String]) -> Option<String> {
    let expanded = expand_tilde(executable);
    let is_exec = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    };
    if expanded.contains('/') {
        return is_exec(Path::new(&expanded)).then_some(expanded);
    }
    search_path
        .iter()
        .map(|d| Path::new(d).join(&expanded))
        .find(|p| is_exec(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Like Unix, plus the `PATHEXT` suffixes when the name has none of them.
#[cfg(windows)]
pub fn resolve_executable(executable: &str, search_path: &[String]) -> Option<String> {
    let expanded = expand_tilde(executable);
    let candidates = |base: &Path| -> Vec<std::path::PathBuf> {
        let has_ext = base.extension().is_some_and(|e| {
            ["exe", "com", "cmd", "bat"]
                .iter()
                .any(|x| e.eq_ignore_ascii_case(x))
        });
        if has_ext {
            return vec![base.to_path_buf()];
        }
        [".exe", ".com", ".cmd", ".bat"]
            .iter()
            .map(|x| std::path::PathBuf::from(format!("{}{x}", base.display())))
            .collect()
    };
    if expanded.contains(['/', '\\', ':']) {
        return candidates(Path::new(&expanded))
            .into_iter()
            .find(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned());
    }
    search_path
        .iter()
        .flat_map(|d| candidates(&Path::new(d).join(&expanded)))
        .find(|p| p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
}

/// cmd.exe parses a batch file's arguments again, so a batch file never receives the prompt. An npm shim
/// (`claude.cmd`) is replaced by what it starts: its `.exe`, or `node` with its script. Other batch files are refused.
#[cfg(windows)]
fn windows_shim(shim: &str, search_path: &[String]) -> Result<Vec<String>, String> {
    let refuse = || {
        format!(
            "{shim} is a batch file, which cannot receive the prompt safely; point the harness at a .exe \
             (for Claude Code, the native installer's claude.exe)"
        )
    };
    let text = std::fs::read(shim).map_err(|_| refuse())?;
    let target = npm_shim_target(&String::from_utf8_lossy(&text)).ok_or_else(refuse)?;
    let dir = Path::new(shim).parent().unwrap_or(Path::new("."));
    let target = dir.join(target);
    if !target.is_file() {
        return Err(refuse());
    }
    let target = target.to_string_lossy().into_owned();
    if target.to_ascii_lowercase().ends_with(".exe") {
        return Ok(vec![target]);
    }
    let node = dir.join("node.exe");
    let node = if node.is_file() {
        node.to_string_lossy().into_owned()
    } else {
        resolve_executable("node.exe", search_path)
            .ok_or_else(|| format!("node not found on PATH (needed by {shim})"))?
    };
    Ok(vec![node, target])
}

/// What an npm `cmd-shim` starts, relative to the shim's folder: the first quoted `"%dp0%\…"` path ending in
/// `.js`, `.cjs`, `.mjs` or `.exe` (other than `node.exe`).
pub fn npm_shim_target(cmd_text: &str) -> Option<String> {
    cmd_text.split("\"%dp0%\\").skip(1).find_map(|rest| {
        let path = &rest[..rest.find('"')?];
        let lower = path.to_ascii_lowercase();
        let ok = [".js", ".cjs", ".mjs", ".exe"]
            .iter()
            .any(|x| lower.ends_with(x))
            && !lower.ends_with("node.exe")
            && !path.contains(['%', '\n', '\r']);
        ok.then(|| path.to_string())
    })
}

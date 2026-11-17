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
            let mut a: Vec<String> = ["-p", "{prompt}", "--output-format", "json"]
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

/// Resolves a bare executable name against the configured search path.
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

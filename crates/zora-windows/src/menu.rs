//! The tray menu as data, built from the shared `TrayModel` view: the same items as the macOS menu bar and the
//! Linux tray (state, Pause/Resume, Pause for 1 Hour, Settings, recent results, Restart Zora).

use serde_json::Value;
use zora_plugin::tray::{AGENTS, Request, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    PauseHour,
    Names,
    Agent,
    Folder,
    Restart,
}

impl Action {
    pub const ALL: [Action; 6] = [
        Action::Toggle,
        Action::PauseHour,
        Action::Names,
        Action::Agent,
        Action::Folder,
        Action::Restart,
    ];

    /// Menu command ids (non-zero, as `TrackPopupMenu` returns 0 for "nothing chosen").
    pub fn id(self) -> u32 {
        Action::ALL.iter().position(|a| *a == self).unwrap() as u32 + 1
    }

    pub fn from_id(id: u32) -> Option<Action> {
        Action::ALL.get((id as usize).checked_sub(1)?).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Greyed-out text: the state line and recent results.
    Label(String),
    Separator,
    Command {
        action: Action,
        label: String,
        enabled: bool,
    },
    Submenu(String, Vec<Item>),
}

pub fn build(view: &View) -> Vec<Item> {
    let cmd = |action, label: &str, enabled| Item::Command {
        action,
        label: label.into(),
        enabled,
    };
    let mut items = vec![
        Item::Label(view.state_line.clone()),
        Item::Separator,
        cmd(Action::Toggle, view.pause_label, view.pause_enabled),
    ];
    if view.show_pause_hour {
        items.push(cmd(
            Action::PauseHour,
            "Pause for 1 Hour",
            view.pause_enabled,
        ));
    }
    items.push(Item::Submenu(
        "Settings".into(),
        vec![
            cmd(Action::Names, "Names Zora Answers To…", true),
            cmd(Action::Agent, "Coding Agent…", true),
            cmd(Action::Folder, "Project Folder…", true),
        ],
    ));
    if !view.history.is_empty() {
        items.push(Item::Separator);
        items.extend(view.history.iter().cloned().map(Item::Label));
    }
    items.push(Item::Separator);
    items.push(cmd(Action::Restart, "Restart Zora", true));
    items
}

/// The pause request for a chosen action (`Toggle` asks the model, which knows whether Zora is paused).
pub fn pause_request(action: Action, toggle: Option<Request>) -> Option<Request> {
    match action {
        Action::Toggle => toggle,
        Action::PauseHour => Some(Request::PauseHour),
        _ => None,
    }
}

/// Comma- or newline-separated names, as `zorad settings set` expects them.
pub fn parse_names(input: &str) -> Vec<String> {
    input
        .split([',', '\n', '\r'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The current wake names from `zorad settings`, comma-separated for the input box.
pub fn names_text(settings: &Value) -> String {
    settings["wakeNames"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>())
        .unwrap_or_default()
        .join(", ")
}

/// The agent box's text: the built-in id, or the custom command line when the custom harness is chosen.
pub fn agent_text(settings: &Value) -> String {
    match settings["harness"].as_str().unwrap_or_default() {
        "custom" => settings["customCommand"]
            .as_str()
            .unwrap_or_default()
            .into(),
        id => id.into(),
    }
}

/// What the agent box asks for: a built-in agent's name, a harness configured by hand (its exact id), or a whole
/// command line for a custom one. Unchanged text changes nothing.
pub fn agent_change(text: &str, settings: &Value) -> Option<Value> {
    let t = text.trim();
    if t.is_empty() || t == agent_text(settings) {
        return None;
    }
    // An exact configured id first, so a hand-written "Codex" entry wins over the built-in alias.
    if t != "custom"
        && settings["harnessWorkspaces"]
            .as_object()
            .is_some_and(|m| m.contains_key(t))
    {
        return Some(serde_json::json!({"harness": t}));
    }
    let lower = t.to_lowercase();
    Some(
        match AGENTS
            .iter()
            .find(|(id, label)| *id != "custom" && (lower == *id || lower == label.to_lowercase()))
        {
            Some((id, _)) => serde_json::json!({"harness": id}),
            None => serde_json::json!({"harness": "custom", "customCommand": t}),
        },
    )
}

/// The tray plugin's options from `hello.config`.
pub struct Options {
    pub zorad: String,
    pub zorad_config: Option<String>,
    pub notifications: bool,
    /// The scheduled task `Restart Zora` runs (packaging/windows/install.ps1 creates it).
    pub task: String,
}

impl Options {
    pub fn from_config(config: &Value) -> Options {
        let zorad = config["zorad"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("zorad.exe")))
                    .filter(|p| p.is_file())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "zorad".into())
            });
        Options {
            zorad,
            zorad_config: config["zoradConfig"].as_str().map(String::from),
            notifications: config["notifications"].as_bool().unwrap_or(true),
            task: config["task"].as_str().unwrap_or(r"\Zora\Zora").to_string(),
        }
    }

    /// Arguments for a `zorad` call, with `--config` when one is set.
    pub fn zorad_args(&self, args: &[String]) -> Vec<String> {
        let mut a = args.to_vec();
        if let Some(c) = &self.zorad_config {
            a.extend(["--config".into(), c.clone()]);
        }
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;
    use zora_plugin::tray::TrayModel;

    fn labels(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                Item::Label(s) => format!("label:{s}"),
                Item::Separator => "-".into(),
                Item::Command { label, enabled, .. } => {
                    format!("{label}{}", if *enabled { "" } else { " (off)" })
                }
                Item::Submenu(s, sub) => format!("{s}[{}]", labels(sub).join("|")),
            })
            .collect()
    }

    #[test]
    fn menu_matches_the_other_trays() {
        let mut m = TrayModel::new();
        m.set_listening(true, None);
        let items = build(&m.view());
        assert_eq!(
            labels(&items),
            [
                "label:Zora: listening for your wake phrase",
                "-",
                "Pause Listening",
                "Pause for 1 Hour",
                "Settings[Names Zora Answers To…|Coding Agent…|Project Folder…]",
                "-",
                "Restart Zora"
            ]
        );
        m.handle("jobSucceeded", "Done", "added tests", Instant::now());
        m.set_listening(false, None);
        let items = build(&m.view());
        let l = labels(&items);
        assert!(l.contains(&"Resume Listening".to_string()), "{l:?}");
        assert!(!l.contains(&"Pause for 1 Hour".to_string()), "{l:?}");
        assert!(l.contains(&"label:Done: added tests".to_string()), "{l:?}");
    }

    #[test]
    fn agent_box_picks_a_builtin_or_a_custom_command() {
        let s =
            json!({"harness": "work-agent", "harnessWorkspaces": {"work-agent": "", "claude": ""}});
        assert_eq!(
            agent_change(" Codex ", &s),
            Some(json!({"harness": "codex"}))
        );
        assert_eq!(
            agent_change("claude code", &s),
            Some(json!({"harness": "claude"}))
        );
        assert_eq!(
            agent_change("opencode run {prompt}", &s),
            Some(json!({"harness": "custom", "customCommand": "opencode run {prompt}"}))
        );
        assert_eq!(agent_change("  ", &s), None);
        // OK on the text as shown keeps a harness set up by hand instead of running its id as a program.
        assert_eq!(agent_change("work-agent", &s), None);
        let s = json!({"harness": "claude", "harnessWorkspaces": {"work-agent": "", "claude": ""}});
        assert_eq!(
            agent_change("work-agent", &s),
            Some(json!({"harness": "work-agent"}))
        );
        let s = json!({"harness": "claude", "harnessWorkspaces": {"Codex": "", "claude": ""}});
        assert_eq!(agent_change("Codex", &s), Some(json!({"harness": "Codex"})));
        assert_eq!(agent_change("codex", &s), Some(json!({"harness": "codex"})));
        assert_eq!(agent_text(&json!({"harness": "pi"})), "pi");
        let s = json!({"harness": "custom", "customCommand": "x {prompt}"});
        assert_eq!(agent_text(&s), "x {prompt}");
        assert_eq!(agent_change("x {prompt}", &s), None);
    }

    #[test]
    fn pause_items_are_disabled_while_a_request_runs() {
        let mut m = TrayModel::new();
        m.set_listening(true, None);
        assert!(m.begin());
        assert!(labels(&build(&m.view())).contains(&"Pause Listening (off)".to_string()));
    }

    #[test]
    fn ids_round_trip_and_requests_follow_the_model() {
        for a in Action::ALL {
            assert_eq!(Action::from_id(a.id()), Some(a));
        }
        assert_eq!(Action::from_id(0), None);
        assert_eq!(
            pause_request(Action::Toggle, Some(Request::Resume)),
            Some(Request::Resume)
        );
        assert_eq!(
            pause_request(Action::PauseHour, None),
            Some(Request::PauseHour)
        );
        assert_eq!(pause_request(Action::Names, Some(Request::Pause)), None);
    }

    #[test]
    fn names_and_options() {
        assert_eq!(
            parse_names(" zora, nova\r\njarvis ,"),
            ["zora", "nova", "jarvis"]
        );
        assert_eq!(
            names_text(&json!({"wakeNames": ["zora", "nova"]})),
            "zora, nova"
        );
        let o =
            Options::from_config(&json!({"zorad": "C:/z/zorad.exe", "zoradConfig": "C:/c.json"}));
        assert_eq!(o.zorad, "C:/z/zorad.exe");
        assert_eq!(
            o.zorad_args(&["pause".into()]),
            ["pause", "--config", "C:/c.json"]
        );
        assert_eq!(o.task, r"\Zora\Zora");
        assert!(o.notifications);
    }
}

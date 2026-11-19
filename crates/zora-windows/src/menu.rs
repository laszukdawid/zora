//! The tray menu as data, built from the shared `TrayModel` view: the same items as the macOS menu bar and the
//! Linux tray (state, Pause/Resume, Pause for 1 Hour, Settings, recent results, Restart Zora).

use serde_json::Value;
use zora_plugin::tray::{Request, View};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Toggle,
    PauseHour,
    Names,
    Folder,
    Restart,
}

impl Action {
    pub const ALL: [Action; 5] = [
        Action::Toggle,
        Action::PauseHour,
        Action::Names,
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
                "Settings[Names Zora Answers To…|Project Folder…]",
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

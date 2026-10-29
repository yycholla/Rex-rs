//! Native desktop actions advertised through Rex's client action protocol.
use anyhow::{Context as _, Result, bail, ensure};
use serde_json::{Map, Value, json};

use crate::core::Direction;

#[derive(Clone, Debug, PartialEq)]
pub enum NativeAction {
    SelectSession {
        session_id: String,
        window_id: Option<String>,
    },
    NewSession,
    CloseSession,
    NewWindow,
    CloseWindow,
    NextWindow,
    PreviousWindow,
    GoToWindow {
        index: i32,
    },
    Split {
        direction: Direction,
    },
    ClosePane,
    FocusPane {
        direction: Direction,
    },
    ZoomPane,
    FontIncrease,
    FontDecrease,
    FontReset,
    ToggleSidebar,
    Copy,
    Paste,
    ReloadConfig,
    EnterMode {
        name: String,
        once: bool,
    },
    ExitMode,
    ExitAllModes,
    NextPane,
    RenameSession,
    RenameWindow,
    MovePane {
        anchor_block_id: String,
        direction: Direction,
        ratio: f64,
    },
    SwapPanes {
        other_block_id: String,
    },
    MoveWindow {
        before_window_id: Option<String>,
    },
    ArrangePane,
    DetachPane,
    RestorePane {
        block_id: String,
    },
    ToggleDetached,
    Find,
    FindNext,
    FindPrevious,
}

#[derive(Clone, Copy)]
enum Arguments {
    Empty,
    Session,
    Index,
    Split,
    Focus,
    Mode,
    MovePane,
    SwapPanes,
    MoveWindow,
    RestorePane,
}

struct ActionSpec {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    arguments: Arguments,
}

const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        name: "session.select",
        title: "Select Session",
        description: "Select a session and optionally one of its tabs.",
        arguments: Arguments::Session,
    },
    ActionSpec {
        name: "session.new",
        title: "New Session",
        description: "Create a session with a terminal tab.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "session.close",
        title: "Close Session",
        description: "Close the selected session and its programs.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.new",
        title: "New Tab",
        description: "Create a terminal tab in the selected session.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.close",
        title: "Close Tab",
        description: "Close the selected tab and its programs.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.next",
        title: "Next Tab",
        description: "Select the next tab.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.previous",
        title: "Previous Tab",
        description: "Select the previous tab.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.goto",
        title: "Go to Tab",
        description: "Select a tab by position, counting from the end for a negative index.",
        arguments: Arguments::Index,
    },
    ActionSpec {
        name: "pane.split",
        title: "Split Pane",
        description: "Create a terminal beside the focused pane.",
        arguments: Arguments::Split,
    },
    ActionSpec {
        name: "pane.close",
        title: "Close Pane",
        description: "Close the focused pane and its program.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "pane.focus",
        title: "Focus Pane",
        description: "Focus the neighboring pane in a direction.",
        arguments: Arguments::Focus,
    },
    ActionSpec {
        name: "pane.zoom",
        title: "Zoom Pane",
        description: "Toggle the focused pane between its split and full-tab size.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.font.increase",
        title: "Increase Font Size",
        description: "Increase this client's terminal font size.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.font.decrease",
        title: "Decrease Font Size",
        description: "Decrease this client's terminal font size.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.font.reset",
        title: "Reset Font Size",
        description: "Restore this client's default terminal font size.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.sidebar.toggle",
        title: "Toggle Sidebar",
        description: "Show or hide this client's session sidebar.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.copy",
        title: "Copy",
        description: "Copy the focused terminal's selected text.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.paste",
        title: "Paste",
        description: "Paste clipboard text into the focused terminal.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.config.reload",
        title: "Reload Configuration",
        description: "Reload Rex's configuration and refresh this client's bindings.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.mode.enter",
        title: "Enter Key Mode",
        description: "Enter a configured key mode, optionally for one shortcut.",
        arguments: Arguments::Mode,
    },
    ActionSpec {
        name: "client.mode.exit",
        title: "Leave Key Mode",
        description: "Leave the most recently entered key mode.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.mode.exit_all",
        title: "Leave All Key Modes",
        description: "Leave every active key mode.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "pane.next",
        title: "Next Pane",
        description: "Focus the next pane in the current tab.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "session.rename",
        title: "Rename Session",
        description: "Open the session name editor.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.tab.rename",
        title: "Rename Tab",
        description: "Open the tab name editor.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "pane.move",
        title: "Move Pane",
        description: "Move the focused pane beside another pane without restarting it. Ratio is the left/top share.",
        arguments: Arguments::MovePane,
    },
    ActionSpec {
        name: "pane.swap",
        title: "Swap Panes",
        description: "Swap the focused pane with another pane in this session.",
        arguments: Arguments::SwapPanes,
    },
    ActionSpec {
        name: "client.tab.move",
        title: "Reorder Tab",
        description: "Move the selected tab before another tab, or to the end.",
        arguments: Arguments::MoveWindow,
    },
    ActionSpec {
        name: "pane.arrange",
        title: "Move or Swap Pane…",
        description: "Choose where to move this pane or which pane to swap with.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "pane.detach",
        title: "Detach Pane",
        description: "Remove this pane from its tab while keeping its program running.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "pane.restore",
        title: "Restore Detached Block",
        description: "Place an existing detached block in a new tab without restarting it.",
        arguments: Arguments::RestorePane,
    },
    ActionSpec {
        name: "client.detached.toggle",
        title: "Detached Blocks…",
        description: "Browse detached blocks in the selected session.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.find.toggle",
        title: "Find in Terminal History",
        description: "Open or close local terminal history search.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.find.next",
        title: "Find Older Match",
        description: "Select the next discovered match toward older history.",
        arguments: Arguments::Empty,
    },
    ActionSpec {
        name: "client.find.previous",
        title: "Find Newer Match",
        description: "Select the previous discovered match toward newer content.",
        arguments: Arguments::Empty,
    },
];

impl Arguments {
    fn schema(self) -> Value {
        let (properties, required) = match self {
            Self::Empty => (json!({}), json!([])),
            Self::RestorePane => (
                json!({"block_id":{"type":"string","minLength":1}}),
                json!(["block_id"]),
            ),
            Self::MovePane => (
                json!({"anchor_block_id":{"type":"string","minLength":1},"direction":{"type":"string","enum":["left","right","above","below"]},"ratio":{"type":"number","exclusiveMinimum":0,"exclusiveMaximum":1}}),
                json!(["anchor_block_id", "direction"]),
            ),
            Self::SwapPanes => (
                json!({"other_block_id":{"type":"string","minLength":1}}),
                json!(["other_block_id"]),
            ),
            Self::MoveWindow => (
                json!({"before_window_id":{"type":"string","minLength":1}}),
                json!([]),
            ),
            Self::Mode => (
                json!({"name":{"type":"string","minLength":1},"once":{"type":"boolean"}}),
                json!(["name"]),
            ),
            Self::Session => (
                json!({"session_id":{"type":"string","minLength":1},"window_id":{"type":"string","minLength":1}}),
                json!(["session_id"]),
            ),
            Self::Index => (
                json!({"index":{"type":"integer","minimum":i32::MIN,"maximum":i32::MAX,"not":{"const":0}}}),
                json!(["index"]),
            ),
            Self::Split => (
                json!({"direction":{"type":"string","enum":["right","left","above","below"]}}),
                json!(["direction"]),
            ),
            Self::Focus => (
                json!({"direction":{"type":"string","enum":["left","right","up","down"]}}),
                json!(["direction"]),
            ),
        };
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Empty => &[],
            Self::RestorePane => &["block_id"],
            Self::MovePane => &["anchor_block_id", "direction", "ratio"],
            Self::SwapPanes => &["other_block_id"],
            Self::MoveWindow => &["before_window_id"],
            Self::Mode => &["name", "once"],
            Self::Session => &["session_id", "window_id"],
            Self::Index => &["index"],
            Self::Split | Self::Focus => &["direction"],
        }
    }
}

/// The exact native actions this desktop can perform, with their argument schemas.
#[must_use]
pub fn declarations() -> Value {
    Value::Array(
        ACTIONS
            .iter()
            .map(|spec| {
                json!({
                    "name":spec.name,"kind":"client","title":spec.title,
                    "description":spec.description,"args":spec.arguments.schema(),
                })
            })
            .collect(),
    )
}

/// Decode a declared native action, rejecting malformed or unknown arguments.
///
/// # Errors
/// Returns an error for an unknown action, unsupported keys, missing arguments,
/// incorrect types, empty IDs, invalid directions or a zero/out-of-range index.
pub fn parse(name: &str, args: &Value) -> Result<NativeAction> {
    let spec = ACTIONS
        .iter()
        .find(|spec| spec.name == name)
        .with_context(|| format!("unsupported native client action: {name}"))?;
    let empty = Map::new();
    let object =
        if args.is_null() && matches!(spec.arguments, Arguments::Empty | Arguments::MoveWindow) {
            &empty
        } else {
            args.as_object()
                .context("native action arguments must be an object")?
        };
    for key in object.keys() {
        ensure!(
            spec.arguments.keys().contains(&key.as_str()),
            "unknown argument {key} for {name}"
        );
    }
    Ok(match name {
        "session.select" => NativeAction::SelectSession {
            session_id: string(object, "session_id")?.to_owned(),
            window_id: object
                .get("window_id")
                .map(|_| string(object, "window_id").map(str::to_owned))
                .transpose()?,
        },
        "session.new" => NativeAction::NewSession,
        "session.close" => NativeAction::CloseSession,
        "client.tab.new" => NativeAction::NewWindow,
        "client.tab.close" => NativeAction::CloseWindow,
        "client.tab.next" => NativeAction::NextWindow,
        "client.tab.previous" => NativeAction::PreviousWindow,
        "client.tab.goto" => {
            let index = object
                .get("index")
                .and_then(Value::as_i64)
                .context("index must be an integer")?;
            let index =
                i32::try_from(index).context("index exceeds the supported integer range")?;
            ensure!(index != 0, "tab index must not be zero");
            NativeAction::GoToWindow { index }
        }
        "pane.split" => NativeAction::Split {
            direction: match string(object, "direction")? {
                "left" => Direction::Left,
                "right" => Direction::Right,
                "above" => Direction::Up,
                "below" => Direction::Down,
                _ => bail!("split direction must be left, right, above or below"),
            },
        },
        "pane.close" => NativeAction::ClosePane,
        "pane.focus" => NativeAction::FocusPane {
            direction: match string(object, "direction")? {
                "left" => Direction::Left,
                "right" => Direction::Right,
                "up" => Direction::Up,
                "down" => Direction::Down,
                _ => bail!("focus direction must be left, right, up or down"),
            },
        },
        "pane.zoom" => NativeAction::ZoomPane,
        "client.font.increase" => NativeAction::FontIncrease,
        "client.font.decrease" => NativeAction::FontDecrease,
        "client.font.reset" => NativeAction::FontReset,
        "client.sidebar.toggle" => NativeAction::ToggleSidebar,
        "client.copy" => NativeAction::Copy,
        "client.paste" => NativeAction::Paste,
        "client.config.reload" => NativeAction::ReloadConfig,
        "client.mode.enter" => parse_mode(object)?,
        "client.mode.exit" => NativeAction::ExitMode,
        "client.mode.exit_all" => NativeAction::ExitAllModes,
        "pane.next" => NativeAction::NextPane,
        "session.rename" => NativeAction::RenameSession,
        "client.tab.rename" => NativeAction::RenameWindow,
        "pane.arrange" => NativeAction::ArrangePane,
        "pane.detach" => NativeAction::DetachPane,
        "pane.restore" => NativeAction::RestorePane {
            block_id: string(object, "block_id")?.into(),
        },
        "client.detached.toggle" => NativeAction::ToggleDetached,
        "client.find.toggle" => NativeAction::Find,
        "client.find.next" => NativeAction::FindNext,
        "client.find.previous" => NativeAction::FindPrevious,
        "pane.move" => parse_move(object)?,
        "pane.swap" => NativeAction::SwapPanes {
            other_block_id: string(object, "other_block_id")?.into(),
        },
        "client.tab.move" => NativeAction::MoveWindow {
            before_window_id: object
                .get("before_window_id")
                .map(|_| string(object, "before_window_id").map(str::to_owned))
                .transpose()?,
        },
        _ => bail!("unsupported native client action: {name}"),
    })
}

fn parse_mode(object: &Map<String, Value>) -> Result<NativeAction> {
    let name = string(object, "name")?;
    ensure!(
        name != "root"
            && name.bytes().all(|byte| byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || b"_-".contains(&byte)),
        "invalid key mode name"
    );
    let once = object
        .get("once")
        .map(|value| value.as_bool().context("once must be a boolean"))
        .transpose()?
        .unwrap_or(false);
    Ok(NativeAction::EnterMode {
        name: name.into(),
        once,
    })
}

fn parse_move(object: &Map<String, Value>) -> Result<NativeAction> {
    let ratio = object
        .get("ratio")
        .map(|value| value.as_f64().context("ratio must be a number"))
        .transpose()?
        .unwrap_or(0.5);
    ensure!(
        ratio.is_finite() && ratio > 0. && ratio < 1.,
        "ratio must be greater than zero and less than one"
    );
    Ok(NativeAction::MovePane {
        anchor_block_id: string(object, "anchor_block_id")?.into(),
        direction: parse_placement(string(object, "direction")?)?,
        ratio,
    })
}

fn parse_placement(value: &str) -> Result<Direction> {
    match value {
        "left" => Ok(Direction::Left),
        "right" => Ok(Direction::Right),
        "above" => Ok(Direction::Up),
        "below" => Ok(Direction::Down),
        _ => bail!("direction must be left, right, above or below"),
    }
}

fn string<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    let value = args
        .get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("{key} must be a string"))?;
    ensure!(!value.trim().is_empty(), "{key} must not be empty");
    Ok(value)
}

/// Built-in shortcut layers, using the Rex key spelling rather than GPUI's.
/// Only shortcuts whose native actions are declared here are included.
#[must_use]
pub fn defaults() -> Value {
    json!([{"name":"default","bindings":[
        {"keys":["ctrl+alt+left"],"action":"pane.focus","args":{"direction":"left"}},
        {"keys":["ctrl+alt+right"],"action":"pane.focus","args":{"direction":"right"}},
        {"keys":["ctrl+alt+up"],"action":"pane.focus","args":{"direction":"up"}},
        {"keys":["ctrl+alt+down"],"action":"pane.focus","args":{"direction":"down"}},
        {"keys":["ctrl+shift+t"],"action":"client.tab.new"},
        {"keys":["ctrl+alt+shift+w"],"action":"client.tab.close"},
        {"keys":["ctrl+pagedown"],"action":"client.tab.next"},
        {"keys":["ctrl+pageup"],"action":"client.tab.previous"},
        {"keys":["ctrl+shift+f"],"action":"client.find.toggle"},
        {"keys":["ctrl+shift+c"],"action":"client.copy"},
        {"keys":["ctrl+shift+v"],"action":"client.paste"},
        {"keys":["ctrl+shift+z"],"action":"pane.zoom"},
        {"keys":["ctrl+shift+n"],"action":"session.new"},
        {"keys":["ctrl+shift+d"],"action":"pane.split","args":{"direction":"right"}},
        {"keys":["ctrl+shift+e"],"action":"pane.split","args":{"direction":"below"}},
        {"keys":["ctrl+shift+w"],"action":"pane.close"},
        {"keys":["ctrl+alt+w"],"action":"session.close"},
        {"keys":["ctrl+tab"],"action":"pane.next"},
        {"keys":["ctrl+shift+r"],"action":"session.rename"},
        {"keys":["ctrl+alt+shift+r"],"action":"client.tab.rename"}
    ]}])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn sample(arguments: Arguments) -> Value {
        match arguments {
            Arguments::Empty => json!({}),
            Arguments::RestorePane => json!({"block_id":"block:detached"}),
            Arguments::MovePane => {
                json!({"anchor_block_id":"block:anchor","direction":"above","ratio":0.333_333_333_333})
            }
            Arguments::SwapPanes => json!({"other_block_id":"block:other"}),
            Arguments::MoveWindow => json!({"before_window_id":"window:before"}),
            Arguments::Mode => json!({"name":"resize","once":true}),
            Arguments::Session => json!({"session_id":"session:test","window_id":"window:test"}),
            Arguments::Index => json!({"index":-1}),
            Arguments::Split => json!({"direction":"above"}),
            Arguments::Focus => json!({"direction":"up"}),
        }
    }

    #[test]
    fn find_actions_accept_only_empty_arguments() -> Result<()> {
        for (name, action) in [
            ("client.find.toggle", NativeAction::Find),
            ("client.find.next", NativeAction::FindNext),
            ("client.find.previous", NativeAction::FindPrevious),
        ] {
            assert_eq!(parse(name, &json!({}))?, action);
            assert!(parse(name, &json!({"query":"untrusted"})).is_err());
        }
        Ok(())
    }

    #[test]
    fn management_arguments_preserve_fractional_ratio_and_reject_malformed_targets() -> Result<()> {
        assert_eq!(
            parse(
                "pane.move",
                &json!({"anchor_block_id":"block:a","direction":"below","ratio":0.123_456_789})
            )?,
            NativeAction::MovePane {
                anchor_block_id: "block:a".into(),
                direction: Direction::Down,
                ratio: 0.123_456_789
            }
        );
        assert_eq!(
            parse("client.tab.move", &Value::Null)?,
            NativeAction::MoveWindow {
                before_window_id: None
            }
        );
        for ratio in [
            json!(0),
            json!(1),
            json!(-0.1),
            json!(1.1),
            json!("0.5"),
            Value::Null,
        ] {
            assert!(
                parse(
                    "pane.move",
                    &json!({"anchor_block_id":"block:a","direction":"right","ratio":ratio})
                )
                .is_err()
            );
        }
        assert!(
            parse(
                "pane.move",
                &json!({"anchor_block_id":"","direction":"left"})
            )
            .is_err()
        );
        assert!(parse("pane.swap", &json!({"other_block_id":null})).is_err());
        assert!(parse("client.tab.move", &json!({"before_window_id":""})).is_err());
        Ok(())
    }

    #[test]
    fn declarations_and_parser_agree_on_names_properties_and_required_arguments() -> Result<()> {
        let declarations = declarations();
        let declarations = declarations.as_array().context("declaration array")?;
        assert_eq!(declarations.len(), ACTIONS.len());
        let mut names = BTreeSet::new();
        for (spec, declaration) in ACTIONS.iter().zip(declarations) {
            assert!(names.insert(spec.name));
            assert_eq!(declaration["name"], spec.name);
            assert_eq!(declaration["kind"], "client");
            assert_eq!(declaration["args"]["additionalProperties"], false);
            let properties = declaration["args"]["properties"]
                .as_object()
                .context("schema properties")?;
            assert_eq!(
                properties
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                spec.arguments.keys().iter().copied().collect()
            );
            let args = sample(spec.arguments);
            parse(spec.name, &args)?;
            let mut extra = args.clone();
            extra["unknown"] = json!(true);
            assert!(parse(spec.name, &extra).is_err());
            for key in declaration["args"]["required"]
                .as_array()
                .context("required array")?
            {
                let mut missing = args.clone();
                missing
                    .as_object_mut()
                    .context("sample object")?
                    .remove(key.as_str().context("required key")?);
                assert!(parse(spec.name, &missing).is_err());
            }
        }
        Ok(())
    }

    #[test]
    fn every_default_binding_names_a_declared_action_with_valid_arguments() -> Result<()> {
        let defaults = defaults();
        for layer in defaults.as_array().context("default layers")? {
            for binding in layer["bindings"].as_array().context("bindings")? {
                let name = binding["action"].as_str().context("action name")?;
                parse(name, &binding["args"])?;
                let keys = binding["keys"].as_array().context("key sequence")?;
                assert_eq!(keys.len(), 1);
                assert!(
                    keys[0]
                        .as_str()
                        .is_some_and(|key| key.starts_with("ctrl+") && !key.contains('-'))
                );
            }
        }
        Ok(())
    }

    #[test]
    fn tab_positions_are_nonzero_checked_integers() -> Result<()> {
        assert_eq!(
            parse("client.tab.goto", &json!({"index":-1}))?,
            NativeAction::GoToWindow { index: -1 }
        );
        assert_eq!(
            parse("client.tab.goto", &json!({"index":i32::MIN}))?,
            NativeAction::GoToWindow { index: i32::MIN }
        );
        for index in [
            json!(0),
            json!(1.5),
            json!("1"),
            json!(null),
            json!(i64::from(i32::MAX) + 1),
            json!(u64::MAX),
        ] {
            assert!(parse("client.tab.goto", &json!({"index":index})).is_err());
        }
        Ok(())
    }

    #[test]
    fn split_and_focus_direction_vocabularies_are_distinct() -> Result<()> {
        assert_eq!(
            parse("pane.split", &json!({"direction":"below"}))?,
            NativeAction::Split {
                direction: Direction::Down
            }
        );
        assert_eq!(
            parse("pane.focus", &json!({"direction":"down"}))?,
            NativeAction::FocusPane {
                direction: Direction::Down
            }
        );
        assert!(parse("pane.split", &json!({"direction":"down"})).is_err());
        assert!(parse("pane.focus", &json!({"direction":"below"})).is_err());
        Ok(())
    }

    #[test]
    fn malformed_arguments_and_unknown_actions_are_rejected() -> Result<()> {
        for args in [
            json!([]),
            json!(""),
            json!(true),
            json!(1),
            json!({"label":"extra"}),
        ] {
            assert!(parse("session.new", &args).is_err());
        }
        assert_eq!(
            parse("session.new", &Value::Null)?,
            NativeAction::NewSession
        );
        assert!(parse("session.unknown", &json!({})).is_err());
        for args in [
            json!(null),
            json!({}),
            json!({"session_id":3}),
            json!({"session_id":" "}),
            json!({"session_id":"s","window_id":null}),
        ] {
            assert!(parse("session.select", &args).is_err());
        }
        assert_eq!(
            parse("session.select", &json!({"session_id":"s"}))?,
            NativeAction::SelectSession {
                session_id: "s".to_owned(),
                window_id: None
            }
        );
        Ok(())
    }
}

//! Client-side resolution of the server's already layered keymap.
//!
//! This module introduces no sequence timeout. A mismatch drops the consumed
//! prefix and reprocesses only the current key; Escape explicitly cancels it.
use std::{collections::BTreeMap, sync::Arc};

use anyhow::{Context as _, Result, ensure};
use serde_json::Value;

#[derive(Clone, Debug)]
struct Binding {
    keys: Vec<String>,
    normalized: Option<Vec<String>>,
    name: String,
    args: Value,
    mode: Option<String>,
}

#[derive(Clone, Debug)]
struct Mode {
    exclusive: bool,
    blocked: Vec<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<Binding>,
    modes: BTreeMap<String, Mode>,
    repeats: BTreeMap<String, bool>,
}

impl Keymap {
    /// Read a `client.keymap` result, preserving the server's binding order.
    /// Unknown key spellings are retained but cannot match local input.
    ///
    /// # Errors
    /// Rejects structurally malformed bindings, modes, or action metadata.
    pub fn parse(value: &Value) -> Result<Self> {
        ensure!(value.is_object(), "Rex keymap must be an object");
        ensure!(value.get("bindings").is_some(), "Rex keymap lacks bindings");
        let mut bindings = Vec::new();
        for binding in array(&value["bindings"])? {
            let keys = strings(&binding["keys"])?;
            let normalized = keys.iter().map(|key| normalize_key(key)).collect();
            bindings.push(Binding {
                keys,
                normalized,
                name: text(binding, "action")?.to_owned(),
                args: binding.get("args").cloned().unwrap_or(Value::Null),
                mode: optional_text(binding, "mode")?,
            });
        }
        let mut modes = BTreeMap::new();
        for mode in array(&value["modes"])? {
            let name = text(mode, "name")?;
            ensure!(valid_mode(name), "invalid Rex key mode name");
            let exclusive = mode["exclusive"]
                .as_bool()
                .context("Rex key mode lacks exclusive flag")?;
            let mut blocked = Vec::new();
            for keys in array(&mode["blocked"])? {
                let keys = strings(keys)?;
                if !keys.is_empty()
                    && let Some(keys) = keys.iter().map(|key| normalize_key(key)).collect()
                {
                    blocked.push(keys);
                }
            }
            ensure!(
                modes
                    .insert(name.to_owned(), Mode { exclusive, blocked })
                    .is_none(),
                "duplicate Rex key mode"
            );
        }
        for binding in &bindings {
            if let Some(mode) = &binding.mode {
                ensure!(
                    modes.contains_key(mode),
                    "Rex binding names an undeclared mode"
                );
            }
        }
        let mut repeats = BTreeMap::new();
        for action in array(&value["actions"])? {
            let name = text(action, "name")?;
            let repeat = match action.get("repeats") {
                None => false,
                Some(value) => value
                    .as_bool()
                    .context("Rex action repeats must be boolean")?,
            };
            repeats.insert(name.to_owned(), repeat);
        }
        Ok(Self {
            bindings,
            modes,
            repeats,
        })
    }

    /// Client declarations are absent from the result's `actions`; merge their
    /// repeat metadata before installing this keymap in a resolver.
    pub fn set_repeats(&mut self, name: &str, repeats: bool) {
        self.repeats.insert(name.to_owned(), repeats);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub name: String,
    pub args: Value,
    /// Earlier keys in the server's original spelling, for `client.key.forward`.
    pub prefix: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    Action(Action),
    Pending,
    Consumed,
    Passthrough,
}

#[derive(Clone, Debug)]
struct ActiveMode {
    name: String,
    once: bool,
}

#[derive(Clone)]
pub struct Resolver {
    keymap: Arc<Keymap>,
    modes: Vec<ActiveMode>,
    /// Each event may name both its logical key and its physical position.
    pending: Vec<Vec<String>>,
}

impl Resolver {
    #[must_use]
    pub fn new(keymap: Keymap) -> Self {
        Self {
            keymap: Arc::new(keymap),
            modes: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Installing new bindings cancels pending sequences. Existing modes survive
    /// only if the refreshed keymap still declares them.
    pub fn replace(&mut self, keymap: Keymap) {
        self.pending.clear();
        self.modes
            .retain(|mode| keymap.modes.contains_key(&mode.name));
        self.keymap = Arc::new(keymap);
    }

    /// # Errors
    /// Rejects mode names absent from the installed keymap.
    pub fn enter_mode(&mut self, name: &str, once: bool) -> Result<()> {
        ensure!(
            self.keymap.modes.contains_key(name),
            "unknown Rex key mode: {name}"
        );
        self.pending.clear();
        self.modes.push(ActiveMode {
            name: name.to_owned(),
            once,
        });
        Ok(())
    }

    pub fn exit_mode(&mut self) -> Option<String> {
        self.pending.clear();
        self.modes.pop().map(|mode| mode.name)
    }

    pub fn exit_all_modes(&mut self) {
        self.pending.clear();
        self.modes.clear();
    }

    #[must_use]
    pub fn has_mode(&self, name: &str) -> bool {
        self.keymap.modes.contains_key(name)
    }

    /// Representative normalized keys for displaying the consumed prefix.
    /// Action.prefix remains the exact server spelling used for forwarding.
    #[must_use]
    pub fn pending_sequence(&self) -> Vec<&str> {
        self.pending
            .iter()
            .filter_map(|keys| keys.first().map(String::as_str))
            .collect()
    }

    #[must_use]
    pub fn active_mode(&self) -> Option<&str> {
        self.modes.last().map(|mode| mode.name.as_str())
    }

    #[must_use]
    pub fn modes(&self) -> Vec<&str> {
        self.modes.iter().map(|mode| mode.name.as_str()).collect()
    }

    pub fn cancel_sequence(&mut self) -> bool {
        !std::mem::take(&mut self.pending).is_empty()
    }

    /// Resolve one physical event against its logical and physical key spellings.
    /// Binding precedence always follows server order, never candidate order.
    /// An unavailable action permits a lower binding; `client.key.forward` is
    /// always available and therefore ends the stack for that exact sequence.
    pub fn resolve(
        &mut self,
        candidates: &[&str],
        repeat: bool,
        mut available: impl FnMut(&str, &Value) -> bool,
    ) -> Resolution {
        let candidates: Vec<_> = candidates
            .iter()
            .filter_map(|key| normalize_key(key))
            .collect();
        let escape = candidates.iter().any(|key| key == "escape");
        if !self.pending.is_empty() && escape {
            self.pending.clear();
            return Resolution::Consumed;
        }
        // Held prefix keys must not become the next key of a sequence.
        if !self.pending.is_empty() && repeat {
            return Resolution::Consumed;
        }
        self.pending.push(candidates.clone());
        let mut result = self.evaluate(repeat, &mut available);
        if result == Resolution::Passthrough && self.pending.len() > 1 {
            self.pending.clear();
            self.pending.push(candidates);
            result = self.evaluate(repeat, &mut available);
        }
        if result != Resolution::Pending {
            self.pending.clear();
        }
        if let Resolution::Action(action) = &result
            && self.modes.last().is_some_and(|mode| mode.once)
            && !matches!(
                action.name.as_str(),
                "client.mode.exit" | "client.mode.exit_all"
            )
        {
            self.modes.pop();
        }
        result
    }

    fn evaluate(
        &mut self,
        repeat: bool,
        available: &mut impl FnMut(&str, &Value) -> bool,
    ) -> Resolution {
        for active in self.modes.iter().rev() {
            let Some(mode) = self.keymap.modes.get(&active.name) else {
                continue;
            };
            if let Some(result) = self.match_bindings(Some(&active.name), repeat, available) {
                return result;
            }
            if self.pending.len() == 1 && self.pending[0].iter().any(|key| key == "escape") {
                self.modes.pop();
                return Resolution::Consumed;
            }
            if mode
                .blocked
                .iter()
                .any(|keys| keys.len() == self.pending.len() && matches_prefix(keys, &self.pending))
            {
                return Resolution::Consumed;
            }
            if mode.exclusive {
                // Retry a failed continuation as a fresh key in the same mode.
                return if self.pending.len() > 1 {
                    Resolution::Passthrough
                } else {
                    Resolution::Consumed
                };
            }
        }
        self.match_bindings(None, repeat, available)
            .unwrap_or(Resolution::Passthrough)
    }

    fn match_bindings(
        &self,
        mode: Option<&str>,
        repeat: bool,
        available: &mut impl FnMut(&str, &Value) -> bool,
    ) -> Option<Resolution> {
        for binding in &self.keymap.bindings {
            if binding.mode.as_deref() != mode || binding.keys.is_empty() {
                continue;
            }
            let Some(keys) = &binding.normalized else {
                continue;
            };
            if !matches_prefix(keys, &self.pending)
                || (binding.name != "client.key.forward"
                    && !available(&binding.name, &binding.args))
            {
                continue;
            }
            if keys.len() > self.pending.len() {
                return Some(if repeat {
                    Resolution::Consumed
                } else {
                    Resolution::Pending
                });
            }
            if repeat
                && binding.name != "client.key.forward"
                && !self
                    .keymap
                    .repeats
                    .get(&binding.name)
                    .copied()
                    .unwrap_or(false)
            {
                return Some(Resolution::Consumed);
            }
            return Some(Resolution::Action(Action {
                name: binding.name.clone(),
                args: binding.args.clone(),
                prefix: binding.keys[..binding.keys.len() - 1].to_vec(),
            }));
        }
        None
    }
}

fn matches_prefix(keys: &[String], events: &[Vec<String>]) -> bool {
    keys.len() >= events.len()
        && keys
            .iter()
            .zip(events)
            .all(|(key, candidates)| candidates.contains(key))
}

/// Normalize only spellings this client understands. Retained future/unknown
/// keys cannot accidentally become terminal shortcuts by string equality.
#[must_use]
pub fn normalize_key(key: &str) -> Option<String> {
    let parts: Vec<_> = key.split('+').collect();
    let (base, modifiers) = parts.split_last()?;
    let mut flags = [false; 4];
    for modifier in modifiers {
        let index = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => 0,
            "alt" | "opt" | "option" => 1,
            "shift" => 2,
            "cmd" | "command" | "super" | "meta" | "win" => 3,
            _ => return None,
        };
        if flags[index] {
            return None;
        }
        flags[index] = true;
    }
    let base = base.to_lowercase();
    let logical = base.chars().count() == 1
        && !base.chars().any(|ch| ch.is_whitespace() || ch.is_control())
        || matches!(
            base.as_str(),
            "enter"
                | "escape"
                | "tab"
                | "space"
                | "backspace"
                | "delete"
                | "insert"
                | "left"
                | "right"
                | "up"
                | "down"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
        )
        || numbered(&base, "f", 1, 35);
    let physical = base
        .strip_prefix('[')
        .and_then(|base| base.strip_suffix(']'))
        .is_some_and(physical_key);
    if !logical && !physical {
        return None;
    }
    let mut normalized = String::new();
    for (flag, name) in flags.into_iter().zip(["ctrl", "alt", "shift", "cmd"]) {
        if flag {
            normalized.push_str(name);
            normalized.push('+');
        }
    }
    normalized.push_str(&base);
    Some(normalized)
}

fn numbered(key: &str, prefix: &str, low: u8, high: u8) -> bool {
    key.strip_prefix(prefix)
        .and_then(|number| number.parse::<u8>().ok())
        .is_some_and(|number| (low..=high).contains(&number))
}
fn physical_key(key: &str) -> bool {
    key.strip_prefix("key").is_some_and(|letter| {
        letter.len() == 1 && letter.bytes().all(|byte| byte.is_ascii_lowercase())
    }) || numbered(key, "digit", 0, 9)
        || numbered(key, "f", 1, 35)
        || matches!(
            key,
            "backquote"
                | "backslash"
                | "bracketleft"
                | "bracketright"
                | "comma"
                | "equal"
                | "intlbackslash"
                | "intlro"
                | "intlyen"
                | "minus"
                | "period"
                | "quote"
                | "semicolon"
                | "slash"
                | "altleft"
                | "altright"
                | "backspace"
                | "capslock"
                | "contextmenu"
                | "controlleft"
                | "controlright"
                | "enter"
                | "metaleft"
                | "metaright"
                | "shiftleft"
                | "shiftright"
                | "space"
                | "tab"
                | "convert"
                | "kanamode"
                | "lang1"
                | "lang2"
                | "lang3"
                | "lang4"
                | "lang5"
                | "nonconvert"
                | "delete"
                | "end"
                | "help"
                | "home"
                | "insert"
                | "pagedown"
                | "pageup"
                | "arrowdown"
                | "arrowleft"
                | "arrowright"
                | "arrowup"
                | "escape"
                | "fn"
                | "fnlock"
                | "printscreen"
                | "scrolllock"
                | "pause"
                | "numlock"
                | "numpadadd"
                | "numpadbackspace"
                | "numpadclear"
                | "numpadclearentry"
                | "numpadcomma"
                | "numpaddecimal"
                | "numpaddivide"
                | "numpadenter"
                | "numpadequal"
                | "numpadmemoryadd"
                | "numpadmemoryclear"
                | "numpadmemoryrecall"
                | "numpadmemorystore"
                | "numpadmemorysubtract"
                | "numpadmultiply"
                | "numpadparenleft"
                | "numpadparenright"
                | "numpadsubtract"
        )
        || numbered(key, "numpad", 0, 9)
}
fn valid_mode(name: &str) -> bool {
    !name.is_empty()
        && name != "root"
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|text| !text.is_empty())
        .with_context(|| format!("Rex keymap lacks {key}"))
}
fn optional_text(value: &Value, key: &str) -> Result<Option<String>> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) if !text.is_empty() => Ok(Some(text.clone())),
        _ => anyhow::bail!("invalid Rex keymap {key}"),
    }
}
fn array(value: &Value) -> Result<&[Value]> {
    match value {
        Value::Null => Ok(&[]),
        Value::Array(values) => Ok(values),
        _ => anyhow::bail!("Rex keymap collection must be an array"),
    }
}
fn strings(value: &Value) -> Result<Vec<String>> {
    array(value)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("Rex key must be a string")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolver(bindings: Value, modes: Value, actions: Value) -> Result<Resolver> {
        let value = Value::Object(
            [
                ("bindings".to_owned(), bindings),
                ("modes".to_owned(), modes),
                ("actions".to_owned(), actions),
            ]
            .into_iter()
            .collect(),
        );
        Ok(Resolver::new(Keymap::parse(&value)?))
    }
    fn action(name: &str) -> Resolution {
        Resolution::Action(Action {
            name: name.into(),
            args: Value::Null,
            prefix: vec![],
        })
    }

    #[test]
    fn precedence_uses_first_available_binding_and_forward_always_stops_stack() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+x"],"action":"unavailable"},
                {"keys":["ctrl+x"],"action":"winner"},
                {"keys":["ctrl+x"],"action":"lower"},
                {"keys":["ctrl+f"],"action":"client.key.forward"},
                {"keys":["ctrl+f"],"action":"lower"}
            ]),
            Value::Null,
            Value::Null,
        )?;
        assert_eq!(
            keys.resolve(&["ctrl+x"], false, |name, _| name != "unavailable"),
            action("winner")
        );
        assert_eq!(
            keys.resolve(&["ctrl+f"], false, |_, _| false),
            action("client.key.forward")
        );
        Ok(())
    }

    #[test]
    fn sequences_win_by_server_order_and_preserve_lua_prefix() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+b","x"],"action":"client.key.forward"},
                {"keys":["ctrl+b","c"],"action":"create"},
                {"keys":["ctrl+b"],"action":"single"}
            ]),
            Value::Null,
            Value::Null,
        )?;
        assert_eq!(
            keys.resolve(&["ctrl+b"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(
            keys.resolve(&["c"], false, |_, _| true),
            Resolution::Action(Action {
                name: "create".into(),
                args: Value::Null,
                prefix: vec!["ctrl+b".into()]
            })
        );
        assert_eq!(
            keys.resolve(&["ctrl+b"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(
            keys.resolve(&["x"], false, |_, _| false),
            Resolution::Action(Action {
                name: "client.key.forward".into(),
                args: Value::Null,
                prefix: vec!["ctrl+b".into()]
            })
        );
        let mut direct = resolver(
            json!([
                {"keys":["ctrl+b"],"action":"single"},{"keys":["ctrl+b","c"],"action":"create"}
            ]),
            Value::Null,
            Value::Null,
        )?;
        assert_eq!(
            direct.resolve(&["ctrl+b"], false, |_, _| true),
            action("single")
        );
        Ok(())
    }

    #[test]
    fn mismatch_reprocesses_current_key_and_escape_cancels_without_falling_through() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+b","c"],"action":"create"},{"keys":["q"],"action":"quit"}
            ]),
            Value::Null,
            Value::Null,
        )?;
        assert_eq!(
            keys.resolve(&["ctrl+b"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(keys.resolve(&["q"], false, |_, _| true), action("quit"));
        assert_eq!(
            keys.resolve(&["ctrl+b"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(
            keys.resolve(&["escape"], false, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(
            keys.resolve(&["c"], false, |_, _| true),
            Resolution::Passthrough
        );
        Ok(())
    }

    #[test]
    fn modes_stack_fallback_and_block_without_hiding_their_own_binding() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["x"],"action":"top","mode":"top"},
                {"keys":["x"],"action":"lower","mode":"lower"},
                {"keys":["x"],"action":"root"},
                {"keys":["y"],"action":"root-y"}
            ]),
            json!([
                {"name":"lower","exclusive":false},
                {"name":"top","exclusive":false,"blocked":[["x"],["y"]]}
            ]),
            Value::Null,
        )?;
        keys.enter_mode("lower", false)?;
        keys.enter_mode("top", false)?;
        assert_eq!(keys.resolve(&["x"], false, |_, _| true), action("top"));
        assert_eq!(
            keys.resolve(&["x"], false, |name, _| name != "top"),
            Resolution::Consumed
        );
        assert_eq!(
            keys.resolve(&["y"], false, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(keys.exit_mode().as_deref(), Some("top"));
        assert_eq!(keys.resolve(&["x"], false, |_, _| true), action("lower"));
        keys.exit_all_modes();
        assert_eq!(keys.resolve(&["x"], false, |_, _| true), action("root"));
        Ok(())
    }

    #[test]
    fn exclusive_modes_and_once_escape_behavior() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["x"],"action":"run","mode":"exclusive"},
                {"keys":["z"],"action":"root"}
            ]),
            json!([{"name":"exclusive","exclusive":true}]),
            Value::Null,
        )?;
        keys.enter_mode("exclusive", true)?;
        assert_eq!(
            keys.resolve(&["z"], false, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(keys.active_mode(), Some("exclusive"));
        assert_eq!(keys.resolve(&["x"], false, |_, _| true), action("run"));
        assert_eq!(keys.active_mode(), None);
        keys.enter_mode("exclusive", false)?;
        assert_eq!(
            keys.resolve(&["escape"], false, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(keys.active_mode(), None);
        Ok(())
    }

    #[test]
    fn explicit_mode_escape_binding_wins_over_implicit_exit() -> Result<()> {
        let mut keys = resolver(
            json!([{"keys":["escape"],"action":"bound","mode":"m"}]),
            json!([{"name":"m","exclusive":false}]),
            Value::Null,
        )?;
        keys.enter_mode("m", false)?;
        assert_eq!(
            keys.resolve(&["escape"], false, |_, _| true),
            action("bound")
        );
        assert_eq!(keys.active_mode(), Some("m"));
        Ok(())
    }

    #[test]
    fn repeats_never_fall_through_a_nonrepeating_winner() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["x"],"action":"single"},{"keys":["x"],"action":"repeat"},
                {"keys":["y"],"action":"repeat"}
            ]),
            Value::Null,
            json!([{"name":"repeat","repeats":true}]),
        )?;
        assert_eq!(
            keys.resolve(&["x"], true, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(keys.resolve(&["y"], true, |_, _| true), action("repeat"));
        Ok(())
    }

    #[test]
    fn exclusive_mode_mismatch_reprocesses_a_bound_current_key() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["g","g"],"action":"sequence","mode":"m"},
                {"keys":["x"],"action":"single","mode":"m"}
            ]),
            json!([{"name":"m","exclusive":true}]),
            Value::Null,
        )?;
        keys.enter_mode("m", false)?;
        assert_eq!(
            keys.resolve(&["g"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(keys.resolve(&["x"], false, |_, _| true), action("single"));
        Ok(())
    }

    #[test]
    fn explicit_exit_in_once_mode_does_not_also_pop_its_parent() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["x"],"action":"client.mode.exit","mode":"once"}
            ]),
            json!([{"name":"parent","exclusive":false},{"name":"once","exclusive":false}]),
            Value::Null,
        )?;
        keys.enter_mode("parent", false)?;
        keys.enter_mode("once", true)?;
        assert_eq!(
            keys.resolve(&["x"], false, |_, _| true),
            action("client.mode.exit")
        );
        assert_eq!(keys.exit_mode().as_deref(), Some("once"));
        assert_eq!(keys.active_mode(), Some("parent"));
        Ok(())
    }

    #[test]
    fn blocked_sequence_stops_only_that_exact_root_sequence() -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["g","g"],"action":"blocked"},
                {"keys":["g","x"],"action":"allowed"}
            ]),
            json!([{"name":"m","exclusive":false,"blocked":[["g","g"]]}]),
            Value::Null,
        )?;
        keys.enter_mode("m", false)?;
        assert_eq!(
            keys.resolve(&["g"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(
            keys.resolve(&["g"], false, |_, _| true),
            Resolution::Consumed
        );
        assert_eq!(
            keys.resolve(&["g"], false, |_, _| true),
            Resolution::Pending
        );
        assert_eq!(
            keys.resolve(&["x"], false, |_, _| true),
            Resolution::Action(Action {
                name: "allowed".into(),
                args: Value::Null,
                prefix: vec!["g".into()]
            })
        );
        Ok(())
    }

    #[test]
    fn physical_and_logical_candidates_obey_binding_order_and_unknown_keys_never_match()
    -> Result<()> {
        let mut keys = resolver(
            json!([
                {"keys":["ctrl+[KeyQ]"],"action":"physical"},
                {"keys":["ctrl+a"],"action":"logical"},
                {"keys":["ctrl+future_key"],"action":"unknown"}
            ]),
            Value::Null,
            Value::Null,
        )?;
        assert_eq!(
            keys.resolve(&["ctrl+a", "ctrl+[KeyQ]"], false, |_, _| true),
            action("physical")
        );
        assert_eq!(
            keys.resolve(&["ctrl+a"], false, |_, _| true),
            action("logical")
        );
        assert_eq!(
            keys.resolve(&["ctrl+future_key"], false, |_, _| true),
            Resolution::Passthrough
        );
        assert_eq!(normalize_key("Cmd+Shift+D"), normalize_key("shift+cmd+d"));
        Ok(())
    }

    #[test]
    fn replacing_keymap_cancels_prefix_and_removes_deleted_modes() -> Result<()> {
        let mut keys = resolver(
            json!([{"keys":["ctrl+b","c"],"action":"run","mode":"m"}]),
            json!([{"name":"m","exclusive":false}]),
            Value::Null,
        )?;
        keys.enter_mode("m", false)?;
        assert_eq!(
            keys.resolve(&["ctrl+b"], false, |_, _| true),
            Resolution::Pending
        );
        keys.replace(Keymap::parse(&json!({"bindings":[]}))?);
        assert_eq!(keys.active_mode(), None);
        assert!(!keys.cancel_sequence());
        assert!(keys.enter_mode("missing", false).is_err());
        Ok(())
    }
}

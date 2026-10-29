//! Preserve native keyboard identity without deriving physical keys from text.
use gpui::KeyDownEvent;

use crate::core::input::{KeyInput, Modifiers};

pub(super) fn shortcut_input(event: &KeyDownEvent) -> (KeyInput, Option<String>) {
    let native = event.native_key.as_ref();
    let modifiers = native.map_or(event.keystroke.modifiers, |key| key.modifiers);
    let physical_code = native
        .and_then(|key| key.physical_key.as_deref())
        .and_then(physical_code);
    let input = KeyInput {
        physical_key: physical_code.map(str::to_owned),
        key: native
            .and_then(|key| key.unshifted_key.clone())
            .unwrap_or_else(|| event.keystroke.key.clone()),
        text: event.keystroke.key_char.clone(),
        modifiers: Modifiers {
            shift: modifiers.shift,
            control: modifiers.control,
            alt: modifiers.alt,
            super_key: modifiers.platform,
        },
        repeat: event.is_held,
    };
    let physical = physical_code.map(|code| {
        let mut chord = String::new();
        for (enabled, name) in [
            (modifiers.control, "ctrl+"),
            (modifiers.alt, "alt+"),
            (modifiers.shift, "shift+"),
            (modifiers.platform, "cmd+"),
        ] {
            if enabled {
                chord.push_str(name);
            }
        }
        chord.push('[');
        chord.push_str(code);
        chord.push(']');
        chord
    });
    (input, physical)
}

// XKB names identify physical positions; layouts only change their symbols.
// Unknown/custom names intentionally have no physical candidate.
fn physical_code(name: &str) -> Option<&'static str> {
    Some(match name {
        "AD01" => "KeyQ",
        "AD02" => "KeyW",
        "AD03" => "KeyE",
        "AD04" => "KeyR",
        "AD05" => "KeyT",
        "AD06" => "KeyY",
        "AD07" => "KeyU",
        "AD08" => "KeyI",
        "AD09" => "KeyO",
        "AD10" => "KeyP",
        "AD11" => "BracketLeft",
        "AD12" => "BracketRight",
        "AC01" => "KeyA",
        "AC02" => "KeyS",
        "AC03" => "KeyD",
        "AC04" => "KeyF",
        "AC05" => "KeyG",
        "AC06" => "KeyH",
        "AC07" => "KeyJ",
        "AC08" => "KeyK",
        "AC09" => "KeyL",
        "AC10" => "Semicolon",
        "AC11" => "Quote",
        "BKSL" => "Backslash",
        "AB01" => "KeyZ",
        "AB02" => "KeyX",
        "AB03" => "KeyC",
        "AB04" => "KeyV",
        "AB05" => "KeyB",
        "AB06" => "KeyN",
        "AB07" => "KeyM",
        "AB08" => "Comma",
        "AB09" => "Period",
        "AB10" => "Slash",
        "TLDE" => "Backquote",
        "LSGT" => "IntlBackslash",
        "AE01" => "Digit1",
        "AE02" => "Digit2",
        "AE03" => "Digit3",
        "AE04" => "Digit4",
        "AE05" => "Digit5",
        "AE06" => "Digit6",
        "AE07" => "Digit7",
        "AE08" => "Digit8",
        "AE09" => "Digit9",
        "AE10" => "Digit0",
        "AE11" => "Minus",
        "AE12" => "Equal",
        "RTRN" => "Enter",
        "ESC" => "Escape",
        "BKSP" => "Backspace",
        "TAB" => "Tab",
        "SPCE" => "Space",
        "DELE" => "Delete",
        "INS" => "Insert",
        "HOME" => "Home",
        "END" => "End",
        "PGUP" => "PageUp",
        "PGDN" => "PageDown",
        "LEFT" => "ArrowLeft",
        "RGHT" => "ArrowRight",
        "UP" => "ArrowUp",
        "DOWN" => "ArrowDown",
        "KP0" => "Numpad0",
        "KP1" => "Numpad1",
        "KP2" => "Numpad2",
        "KP3" => "Numpad3",
        "KP4" => "Numpad4",
        "KP5" => "Numpad5",
        "KP6" => "Numpad6",
        "KP7" => "Numpad7",
        "KP8" => "Numpad8",
        "KP9" => "Numpad9",
        "KPDL" => "NumpadDecimal",
        "KPDV" => "NumpadDivide",
        "KPMU" => "NumpadMultiply",
        "KPSU" => "NumpadSubtract",
        "KPAD" => "NumpadAdd",
        "KPEN" => "NumpadEnter",
        "KPEQ" => "NumpadEqual",
        "NMLK" => "NumLock",
        "SCLK" => "ScrollLock",
        "PRSC" => "PrintScreen",
        "PAUS" => "Pause",
        "FK01" => "F1",
        "FK02" => "F2",
        "FK03" => "F3",
        "FK04" => "F4",
        "FK05" => "F5",
        "FK06" => "F6",
        "FK07" => "F7",
        "FK08" => "F8",
        "FK09" => "F9",
        "FK10" => "F10",
        "FK11" => "F11",
        "FK12" => "F12",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, NativeKey};

    #[test]
    fn physical_identity_is_independent_of_layout_and_text() {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: gpui::Modifiers::default(),
                key: "A".into(),
                key_char: Some("A".into()),
            },
            native_key: Some(NativeKey {
                physical_key: Some("AD01".into()),
                unshifted_key: Some("a".into()),
                modifiers: gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            }),
            is_held: false,
        };
        let (key, physical) = shortcut_input(&event);
        assert_eq!(key.key, "a");
        assert_eq!(key.physical_key.as_deref(), Some("KeyQ"));
        assert_eq!(key.text.as_deref(), Some("A"));
        assert!(key.modifiers.shift);
        assert_eq!(physical.as_deref(), Some("shift+[KeyQ]"));
        assert_eq!(physical_code("CUSTOM"), None);
    }

    #[test]
    fn shifted_symbol_keeps_raw_shift_and_commit_once() {
        let mut event = KeyDownEvent {
            keystroke: Keystroke {
                modifiers: gpui::Modifiers::default(),
                key: "!".into(),
                key_char: Some("!".into()),
            },
            native_key: Some(NativeKey {
                physical_key: Some("AE01".into()),
                unshifted_key: Some("1".into()),
                modifiers: gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                },
            }),
            is_held: true,
        };
        let (key, physical) = shortcut_input(&event);
        assert_eq!(key.key, "1");
        assert_eq!(key.text.as_deref(), Some("!"));
        assert!(key.modifiers.shift && key.repeat);
        assert_eq!(physical.as_deref(), Some("shift+[Digit1]"));
        event.native_key = None;
        let (key, physical) = shortcut_input(&event);
        assert_eq!(key.key, "!");
        assert!(physical.is_none());
        assert!(key.physical_key.is_none());
    }
}

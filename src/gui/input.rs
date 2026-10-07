//! Native keys preserve modal control bindings; text comes from the keyboard
//! layout (or an IME commit), never from physical US-key assumptions.
use crate::input::{Key, KeyCode, Modifiers};
use winit::keyboard::{Key as NativeKey, ModifiersState, NamedKey};

pub fn keys(key: &NativeKey, text: Option<&str>, modifiers: ModifiersState) -> Vec<Key> {
    let mut flags = Modifiers::empty();
    if modifiers.shift_key() {
        flags = flags.union(Modifiers::SHIFT);
    }
    if modifiers.control_key() {
        flags = flags.union(Modifiers::CONTROL);
    }
    if modifiers.alt_key() {
        flags = flags.union(Modifiers::ALT);
    }
    let code = match key {
        NativeKey::Named(named) => match named {
            NamedKey::Enter => KeyCode::Enter,
            NamedKey::Escape => KeyCode::Esc,
            NamedKey::Backspace => KeyCode::Backspace,
            NamedKey::Delete => KeyCode::Delete,
            NamedKey::Tab if modifiers.shift_key() => KeyCode::BackTab,
            NamedKey::Tab => KeyCode::Tab,
            NamedKey::ArrowLeft => KeyCode::Left,
            NamedKey::ArrowRight => KeyCode::Right,
            NamedKey::ArrowUp => KeyCode::Up,
            NamedKey::ArrowDown => KeyCode::Down,
            NamedKey::Home => KeyCode::Home,
            NamedKey::End => KeyCode::End,
            NamedKey::PageUp => KeyCode::PageUp,
            NamedKey::PageDown => KeyCode::PageDown,
            NamedKey::Space => KeyCode::Char(' '),
            _ => return Vec::new(),
        },
        NativeKey::Character(value) => {
            let value = if modifiers.control_key() || modifiers.alt_key() {
                value.as_str()
            } else {
                text.unwrap_or(value.as_str())
            };
            return value
                .chars()
                .filter(|ch| !ch.is_control())
                .map(|ch| Key {
                    code: KeyCode::Char(if modifiers.control_key() {
                        ch.to_ascii_lowercase()
                    } else {
                        ch
                    }),
                    modifiers: flags,
                })
                .collect();
        }
        _ => return Vec::new(),
    };
    vec![Key {
        code,
        modifiers: flags,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_text_shift_and_modal_controls_are_preserved() {
        assert_eq!(
            keys(
                &NativeKey::Character("D".into()),
                Some("D"),
                ModifiersState::SHIFT
            )[0]
            .code,
            KeyCode::Char('D')
        );
        assert_eq!(
            keys(
                &NativeKey::Character("v".into()),
                Some("\u{16}"),
                ModifiersState::CONTROL
            ),
            vec![Key::ctrl('v')]
        );
        assert_eq!(
            keys(
                &NativeKey::Character("é".into()),
                Some("é"),
                ModifiersState::empty()
            ),
            vec![Key::char('é')]
        );
        assert_eq!(
            keys(
                &NativeKey::Named(NamedKey::Tab),
                None,
                ModifiersState::SHIFT
            )[0]
            .code,
            KeyCode::BackTab
        );
        assert!(keys(&NativeKey::Dead(Some('´')), None, ModifiersState::empty()).is_empty());
    }
}

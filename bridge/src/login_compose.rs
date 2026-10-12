//! Compose physical login keysyms without sharing XKB pointers across threads.
use std::{cell::RefCell, ffi::OsString};
use xkbcommon::xkb::{self, compose};

thread_local! {
    // Compose tables are immutable but their Rust wrapper is not Send. Each
    // input thread owns its table; only the pending keysyms belong to the form.
    static TABLE: RefCell<Option<(OsString, Option<compose::Table>)>> = const { RefCell::new(None) };
}

pub(crate) struct ComposeInput {
    locale: OsString,
    pending: Vec<xkb::Keysym>,
}
impl Default for ComposeInput {
    fn default() -> Self {
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .filter_map(std::env::var_os)
            .find(|value| !value.is_empty())
            .unwrap_or_else(|| "en_US.UTF-8".into());
        Self {
            locale,
            pending: Vec::new(),
        }
    }
}
impl ComposeInput {
    pub(crate) fn reset(&mut self) {
        // Pending compose keys can also be part of a password.
        self.pending.fill(xkb::Keysym::new(0));
        self.pending.clear();
    }

    pub(crate) fn press(
        &mut self,
        context: &xkb::Context,
        keysym: xkb::Keysym,
        fallback: String,
    ) -> String {
        TABLE.with(|cached| {
            let mut cached = cached.borrow_mut();
            if cached
                .as_ref()
                .is_none_or(|(locale, _)| *locale != self.locale)
            {
                let table = compose::Table::new_from_locale(
                    context,
                    &self.locale,
                    compose::COMPILE_NO_FLAGS,
                )
                .or_else(|_| {
                    compose::Table::new_from_locale(
                        context,
                        "en_US.UTF-8".as_ref(),
                        compose::COMPILE_NO_FLAGS,
                    )
                })
                .ok();
                *cached = Some((self.locale.clone(), table));
            }
            let Some(table) = &cached.as_ref().unwrap().1 else {
                self.reset();
                return fallback;
            };
            let mut state = compose::State::new(table, compose::STATE_NO_FLAGS);
            for symbol in &self.pending {
                state.feed(*symbol);
            }
            let accepted = state.feed(keysym) == compose::FeedResult::Accepted;
            match state.status() {
                compose::Status::Composing => {
                    if accepted {
                        // Bound sequences even with custom service compose files.
                        if self.pending.len() >= 32 {
                            self.reset();
                        } else {
                            self.pending.push(keysym);
                        }
                    }
                    String::new()
                }
                compose::Status::Composed => {
                    self.reset();
                    state.utf8().unwrap_or_default()
                }
                compose::Status::Cancelled => {
                    // A failed sequence contributes no partial password text.
                    self.reset();
                    String::new()
                }
                compose::Status::Nothing => {
                    self.reset();
                    fallback
                }
            }
        })
    }
}
impl Drop for ComposeInput {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keysym(name: &str) -> xkb::Keysym {
        xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS)
    }

    #[test]
    fn physical_dead_keys_and_compose_sequences_emit_only_completed_text() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let mut input = ComposeInput {
            locale: "en_US.UTF-8".into(),
            pending: Vec::new(),
        };
        assert_eq!(
            input.press(&context, keysym("dead_acute"), String::new()),
            ""
        );
        assert_eq!(input.press(&context, keysym("a"), "a".into()), "á");
        assert_eq!(
            input.press(&context, keysym("Multi_key"), String::new()),
            ""
        );
        assert_eq!(input.press(&context, keysym("apostrophe"), "'".into()), "");
        assert_eq!(input.press(&context, keysym("e"), "e".into()), "é");
        assert_eq!(input.press(&context, keysym("x"), "x".into()), "x");
    }

    #[test]
    fn resetting_pending_composition_restores_ordinary_input() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let mut input = ComposeInput::default();
        assert_eq!(
            input.press(&context, keysym("dead_acute"), String::new()),
            ""
        );
        input.reset();
        assert_eq!(input.press(&context, keysym("a"), "a".into()), "a");
        assert!(input.pending.is_empty());
    }
}

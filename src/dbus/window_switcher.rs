// SPDX-License-Identifier: GPL-3.0-only

use super::DBusState;
use calloop::timer::TimeoutFuture;
use cosmic_settings_config::shortcuts::{Binding, Modifiers, action::System};
use futures_util::{
    StreamExt,
    future::{Either, select},
};
use smithay::input::keyboard::ModifiersState;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use zbus::{fdo::DBusProxy, names::OwnedUniqueName};

// Limit the number of key events waiting on a stalled launcher.
const MAX_PENDING_EVENTS: usize = 256;
const LAUNCHER: &str = "com.system76.CosmicLauncher";

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
enum Action {
    AltTab,
    ShiftAltTab,
    SwitcherCommit,
    SwitcherCancel,
}

impl Action {
    fn ends_gesture(self) -> bool {
        matches!(self, Self::SwitcherCommit | Self::SwitcherCancel)
    }
}

#[derive(Debug, Default)]
pub(super) struct WindowSwitcher {
    active: Option<(String, Modifiers)>,
    pending: VecDeque<(String, Action)>,
    sending: bool,
    // Send the whole gesture to the launcher instance that received its first step.
    recipient: Option<OwnedUniqueName>,
    sent_seat: Option<String>,
}

impl WindowSwitcher {
    fn finish(&mut self, seat: &str, mods: &ModifiersState, cancel: bool) -> Option<Action> {
        let (owner, held) = self.active.as_ref()?;
        let released =
            (held.alt && !mods.alt) || (held.logo && !mods.logo) || (held.ctrl && !mods.ctrl);
        if owner != seat || (!cancel && !released) {
            return None;
        }
        self.active = None;
        Some(if cancel {
            Action::SwitcherCancel
        } else {
            Action::SwitcherCommit
        })
    }
}

impl DBusState {
    pub(crate) fn window_switcher_step(
        &self,
        seat: &str,
        binding: &Binding,
        system: &System,
        command: &str,
    ) -> bool {
        let action = match (system, command) {
            (System::WindowSwitcher, "cosmic-launcher alt-tab") => Action::AltTab,
            (System::WindowSwitcherPrevious, "cosmic-launcher shift-alt-tab") => {
                Action::ShiftAltTab
            }
            _ => return false,
        };
        let mods = &binding.modifiers;
        if binding.key.is_none()
            || !(mods.alt || mods.logo || mods.ctrl)
            || self.0.session_conn.is_err()
        {
            return false;
        }

        let mut state = self.0.switcher.borrow_mut();
        if state.active.as_ref().is_none_or(|(owner, _)| owner != seat) {
            if let Some((owner, _)) = state.active.replace((seat.to_owned(), mods.clone())) {
                state.pending.push_back((owner, Action::SwitcherCancel));
            }
        }
        drop(state);
        self.enqueue_switcher(seat.to_owned(), action);
        true
    }

    pub(crate) fn window_switcher_input(&self, seat: &str, mods: &ModifiersState, cancel: bool) {
        let action = self.0.switcher.borrow_mut().finish(seat, mods, cancel);
        if let Some(action) = action {
            self.enqueue_switcher(seat.to_owned(), action);
        }
    }

    pub(crate) fn cancel_window_switcher(&self) {
        let mut state = self.0.switcher.borrow_mut();
        state.active = None;
        state.pending.clear();
        let sent_seat = state.sent_seat.take();
        drop(state);
        if let Some(seat) = sent_seat {
            self.enqueue_switcher(seat, Action::SwitcherCancel);
        }
    }

    fn enqueue_switcher(&self, seat: String, action: Action) {
        let mut state = self.0.switcher.borrow_mut();
        if state.pending.len() >= MAX_PENDING_EVENTS {
            drop(state);
            self.cancel_window_switcher();
            tracing::warn!("Cancelled window switcher after its input queue filled");
            return;
        }
        state.pending.push_back((seat, action));
        if state.sending {
            return;
        }
        state.sending = true;
        let dbus = self.clone();
        self.spawn(async move {
            loop {
                let event = dbus.0.switcher.borrow_mut().pending.pop_front();
                let Some((seat, action)) = event else { break };
                if let Err(err) = dbus.send_switcher(&seat, action).await {
                    dbus.0.switcher.borrow_mut().recipient = None;
                    tracing::warn!(?err, "Failed to send window switcher action");
                }
                if action.ends_gesture() {
                    let mut state = dbus.0.switcher.borrow_mut();
                    state.sent_seat = None;
                    state.recipient = None;
                }
            }
            dbus.0.switcher.borrow_mut().sending = false;
        });
    }

    async fn send_switcher(&self, seat: &str, action: Action) -> anyhow::Result<()> {
        let conn = self.session_conn().await?;
        if !action.ends_gesture() && self.0.switcher.borrow().sent_seat.is_none() {
            self.0.switcher.borrow_mut().sent_seat = Some(seat.to_owned());
            let owner = self.launcher_owner(conn).await?;
            let mut state = self.0.switcher.borrow_mut();
            if state.sent_seat.is_none() {
                // The session locked or input overflowed while the launcher was starting.
                return Ok(());
            }
            state.recipient = Some(owner);
        }
        let receiver = self.0.switcher.borrow().recipient.clone();
        if let Some(receiver) = receiver {
            let platform = HashMap::<&str, zbus::zvariant::Variant<'_>>::new();
            conn.call_method(
                Some(receiver.as_str()),
                "/com/system76/CosmicLauncher",
                Some("org.freedesktop.DbusActivation"),
                "ActivateAction",
                &(
                    serde_json::to_string(&action)?,
                    vec!["ordered-switch-v1", seat],
                    platform,
                ),
            )
            .await?;
        }
        Ok(())
    }

    async fn launcher_owner(&self, conn: &zbus::Connection) -> anyhow::Result<OwnedUniqueName> {
        let bus = DBusProxy::new(conn).await?;
        let name = zbus::names::BusName::try_from(LAUNCHER)?;
        match bus.get_name_owner(name.clone()).await {
            Ok(owner) => return Ok(owner),
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => (),
            Err(err) => return Err(err.into()),
        }
        // Subscribe before checking again so we catch a launcher that starts during setup.
        let mut owners = bus
            .receive_name_owner_changed_with_args(&[(0, LAUNCHER)])
            .await?;
        match bus.get_name_owner(name).await {
            Ok(owner) => return Ok(owner),
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => (),
            Err(err) => return Err(err.into()),
        }
        self.0.evlh.insert_idle(|state| {
            if state.common.shell.read().session_lock.is_none() {
                state.spawn_command("cosmic-launcher".to_owned());
            }
        });
        let acquired = async {
            while let Some(change) = owners.next().await {
                if let Some(owner) = change.args()?.new_owner.as_ref() {
                    return Ok(OwnedUniqueName::from(owner.to_owned()));
                }
            }
            anyhow::bail!("session bus disconnected before launcher startup");
        };
        let timeout = TimeoutFuture::from_duration(&self.0.evlh, Duration::from_secs(5));
        match select(std::pin::pin!(acquired), timeout).await {
            Either::Left((owner, _)) => owner,
            Either::Right(_) => anyhow::bail!("launcher did not acquire its bus name"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alt_shift_gesture() -> WindowSwitcher {
        WindowSwitcher {
            active: Some(("seat-0".into(), Modifiers::new().alt().shift())),
            ..Default::default()
        }
    }

    #[test]
    fn releasing_shift_preserves_the_gesture_until_alt_release() {
        let mut switcher = alt_shift_gesture();
        let alt_only = ModifiersState {
            alt: true,
            ..Default::default()
        };
        assert!(switcher.finish("seat-0", &alt_only, false).is_none());
        let released = ModifiersState::default();
        let commit = switcher.finish("seat-0", &released, false).unwrap();
        assert_eq!(commit, Action::SwitcherCommit);
        assert!(switcher.finish("seat-0", &released, false).is_none());
    }

    #[test]
    fn another_seat_cannot_release_or_cancel_the_gesture() {
        let mut switcher = alt_shift_gesture();
        let released = ModifiersState::default();
        assert!(switcher.finish("seat-1", &released, false).is_none());
        assert!(switcher.finish("seat-1", &released, true).is_none());
        let cancel = switcher.finish("seat-0", &released, true).unwrap();
        assert_eq!(cancel, Action::SwitcherCancel);
        assert!(switcher.finish("seat-0", &released, false).is_none());
    }
}

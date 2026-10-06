//! Secure application activation through `xdg-activation-v1`.
//!
//! Possessing any random token must not be enough to steal keyboard focus. Client-created tokens
//! therefore require a recent serial from Anvil's only seat and must belong to the client that was
//! focused for that input. Launcher-created tokens are trusted separately because their origin is
//! the compositor itself, not an unprivileged Wayland client.

use std::time::Duration;

use crate::{Anvil, state::ACTIVATION_TOKEN_LIFETIME};
use smithay::{
    delegate_xdg_activation,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

impl XdgActivationHandler for Anvil {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.activation_state
    }

    fn token_created(&mut self, _: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        // Clients may abandon a token without activating anything. Opportunistic pruning keeps
        // that untrusted protocol state bounded without a timer or another event-loop source.
        self.activation_state
            .retain_tokens(|_, token| token.timestamp.elapsed() <= ACTIVATION_TOKEN_LIFETIME);
        let recent_serial = data.serial.as_ref().is_some_and(|(serial, seat)| {
            self.seat.owns(seat)
                && self.last_user_input.is_some_and(|(known, when)| {
                    known == *serial && when.elapsed() <= ACTIVATION_TOKEN_LIFETIME
                })
        });
        let focused_client = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
            .and_then(|surface| surface.client())
            .map(|client| client.id());
        let focused_requester = data
            .client_id
            .as_ref()
            .is_some_and(|requester| focused_client.as_ref() == Some(requester));
        let matching_surface = data.surface.as_ref().is_none_or(|surface| {
            data.client_id.as_ref().is_some_and(|requester| {
                surface
                    .client()
                    .is_some_and(|client| client.id() == *requester)
            })
        });

        activation_context_valid(
            recent_serial,
            focused_requester,
            matching_surface,
            data.timestamp.elapsed(),
        )
    }

    fn request_activation(
        &mut self,
        token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        // Tokens are one-shot capabilities. Removing before changing focus also prevents a client
        // callback triggered by the focus transition from racing to reuse the same authority.
        self.activation_state.remove_token(&token);
        if self.session_locked() || token_data.timestamp.elapsed() > ACTIVATION_TOKEN_LIFETIME {
            return;
        }
        self.activate_surface(&surface);
    }
}
delegate_xdg_activation!(Anvil);

fn activation_context_valid(
    recent_serial: bool,
    focused_requester: bool,
    matching_surface: bool,
    age: Duration,
) -> bool {
    recent_serial && focused_requester && matching_surface && age <= ACTIVATION_TOKEN_LIFETIME
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_requires_complete_recent_context() {
        assert!(activation_context_valid(
            true,
            true,
            true,
            Duration::from_secs(1)
        ));
        assert!(!activation_context_valid(
            false,
            true,
            true,
            Duration::from_secs(1)
        ));
        assert!(!activation_context_valid(
            true,
            false,
            true,
            Duration::from_secs(1)
        ));
        assert!(!activation_context_valid(
            true,
            true,
            false,
            Duration::from_secs(1)
        ));
        assert!(!activation_context_valid(
            true,
            true,
            true,
            Duration::from_secs(11)
        ));
    }
}

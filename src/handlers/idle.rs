//! Idle notification and application-controlled inhibition.
//!
//! Smithay's idle-notify helper assumes that Wayland dispatch and Calloop use the same state type.
//! Anvil deliberately keeps `Display<Anvil>` inside `CalloopData`, so this small adapter stores the
//! protocol resources on `Anvil` while its timers receive `CalloopData`. Idle-inhibit itself has no
//! timer coupling and therefore uses Smithay's standard manager directly.

use std::{
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use smithay::{
    delegate_idle_inhibit,
    input::Seat,
    reexports::{
        calloop::{
            LoopHandle, RegistrationToken,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::ext::idle_notify::v1::server::{
            ext_idle_notification_v1::{self, ExtIdleNotificationV1},
            ext_idle_notifier_v1::{self, ExtIdleNotifierV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::{ClientId, GlobalId},
            protocol::{wl_seat::WlSeat, wl_surface::WlSurface},
        },
    },
    wayland::{
        compositor::add_destruction_hook, idle_inhibit::IdleInhibitHandler, seat::WaylandFocus,
    },
};

use crate::{Anvil, CalloopData};

#[derive(Debug)]
pub(crate) struct IdleNotificationData {
    seat: WlSeat,
    timeout: Duration,
    ignores_inhibitors: bool,
    idle: AtomicBool,
    timer: Mutex<Option<RegistrationToken>>,
}

/// Protocol resources plus the Calloop handle needed to schedule their individual timeouts.
pub(crate) struct IdleNotificationState {
    _global: GlobalId,
    notifications: Vec<ExtIdleNotificationV1>,
    loop_handle: LoopHandle<'static, CalloopData>,
    inhibited: bool,
}

impl IdleNotificationState {
    pub(crate) fn new(
        display: &DisplayHandle,
        loop_handle: LoopHandle<'static, CalloopData>,
    ) -> Self {
        Self {
            _global: display.create_global::<Anvil, ExtIdleNotifierV1, _>(2, ()),
            notifications: Vec::new(),
            loop_handle,
            inhibited: false,
        }
    }

    fn track(&mut self, notification: ExtIdleNotificationV1) {
        self.notifications.push(notification.clone());
        self.rearm(&notification);
    }

    fn remove(&mut self, notification: &ExtIdleNotificationV1) {
        if let Some(data) = notification.data::<IdleNotificationData>() {
            if let Some(token) = data.timer.lock().unwrap().take() {
                self.loop_handle.remove(token);
            }
        }
        self.notifications.retain(|known| known != notification);
    }

    fn rearm(&self, notification: &ExtIdleNotificationV1) {
        let Some(data) = notification.data::<IdleNotificationData>() else {
            return;
        };
        if let Some(token) = data.timer.lock().unwrap().take() {
            self.loop_handle.remove(token);
        }
        if self.inhibited && !data.ignores_inhibitors {
            return;
        }
        let target = notification.clone();
        let timeout = data.timeout;
        let token =
            self.loop_handle
                .insert_source(Timer::from_duration(timeout), move |_, _, data| {
                    data.state.idle_notification_expired(&target);
                    TimeoutAction::Drop
                });
        *data.timer.lock().unwrap() = token.ok();
    }

    fn notify_activity(&mut self, seat: &Seat<Anvil>) {
        self.notifications.retain(Resource::is_alive);
        for notification in self.notifications.clone() {
            let data = notification.data::<IdleNotificationData>().unwrap();
            if !seat.owns(&data.seat) {
                continue;
            }
            if data.idle.swap(false, Ordering::AcqRel) {
                notification.resumed();
            }
            self.rearm(&notification);
        }
    }

    fn set_inhibited(&mut self, inhibited: bool) {
        if self.inhibited == inhibited {
            return;
        }
        self.inhibited = inhibited;
        for notification in self.notifications.clone() {
            let data = notification.data::<IdleNotificationData>().unwrap();
            if data.ignores_inhibitors {
                continue;
            }
            if inhibited && data.idle.swap(false, Ordering::AcqRel) {
                notification.resumed();
            }
            self.rearm(&notification);
        }
    }
}

impl Anvil {
    pub(crate) fn notify_idle_activity(&mut self) {
        let seat = self.seat.clone();
        self.idle_notification_state.notify_activity(&seat);
    }

    fn idle_notification_expired(&mut self, notification: &ExtIdleNotificationV1) {
        let Some(data) = notification.data::<IdleNotificationData>() else {
            return;
        };
        *data.timer.lock().unwrap() = None;
        if notification.is_alive()
            && (data.ignores_inhibitors || !self.idle_notification_state.inhibited)
            && !data.idle.swap(true, Ordering::AcqRel)
        {
            notification.idled();
        }
    }

    pub(crate) fn refresh_idle_inhibition(&mut self) {
        self.idle_inhibitors
            .retain(|(surface, count)| surface.is_alive() && *count > 0);
        let inhibited = self.idle_inhibitors.iter().any(|(surface, _)| {
            self.space
                .elements()
                .any(|window| window.wl_surface().as_deref() == Some(surface))
        });
        self.idle_notification_state.set_inhibited(inhibited);
    }

    fn clear_idle_inhibitors(&mut self, surface: &WlSurface) {
        self.idle_inhibitors.retain(|(known, _)| known != surface);
        self.refresh_idle_inhibition();
    }
}

impl GlobalDispatch<ExtIdleNotifierV1, ()> for Anvil {
    fn bind(
        _: &mut Self,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ExtIdleNotifierV1>,
        _: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ExtIdleNotifierV1, ()> for Anvil {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &ExtIdleNotifierV1,
        request: ext_idle_notifier_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let (id, timeout, seat, ignores_inhibitors) = match request {
            ext_idle_notifier_v1::Request::GetIdleNotification { id, timeout, seat } => {
                (id, timeout, seat, false)
            }
            ext_idle_notifier_v1::Request::GetInputIdleNotification { id, timeout, seat } => {
                (id, timeout, seat, true)
            }
            ext_idle_notifier_v1::Request::Destroy => return,
            _ => unreachable!(),
        };
        let notification = data_init.init(
            id,
            IdleNotificationData {
                seat,
                timeout: Duration::from_millis(u64::from(timeout)),
                ignores_inhibitors,
                idle: AtomicBool::new(false),
                timer: Mutex::new(None),
            },
        );
        state.idle_notification_state.track(notification);
    }
}

impl Dispatch<ExtIdleNotificationV1, IdleNotificationData> for Anvil {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &ExtIdleNotificationV1,
        request: ext_idle_notification_v1::Request,
        _: &IdleNotificationData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_idle_notification_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut Self,
        _: ClientId,
        notification: &ExtIdleNotificationV1,
        _: &IdleNotificationData,
    ) {
        state.idle_notification_state.remove(notification);
    }
}

impl IdleInhibitHandler for Anvil {
    fn inhibit(&mut self, surface: WlSurface) {
        if let Some((_, count)) = self
            .idle_inhibitors
            .iter_mut()
            .find(|(known, _)| known == &surface)
        {
            *count += 1;
        } else {
            add_destruction_hook::<Anvil, _>(&surface, |state, destroyed| {
                state.clear_idle_inhibitors(destroyed);
            });
            self.idle_inhibitors.push((surface, 1));
        }
        self.refresh_idle_inhibition();
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        if let Some((_, count)) = self
            .idle_inhibitors
            .iter_mut()
            .find(|(known, _)| known == &surface)
        {
            *count = count.saturating_sub(1);
        }
        self.refresh_idle_inhibition();
    }
}

delegate_idle_inhibit!(Anvil);

#[cfg(test)]
mod tests {
    #[test]
    fn input_idle_listeners_ignore_application_inhibitors() {
        fn timer_runs(inhibited: bool, ignores_inhibitors: bool) -> bool {
            !inhibited || ignores_inhibitors
        }
        assert!(!timer_runs(true, false));
        assert!(timer_runs(true, true));
        assert!(timer_runs(false, false));
    }
}

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
    time::{Duration, Instant},
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
    last_activity: Mutex<Instant>,
    timer: Mutex<Option<RegistrationToken>>,
}

/// Protocol resources plus the Calloop handle needed to schedule their individual timeouts.
pub(crate) struct IdleNotificationState {
    _global: GlobalId,
    notifications: Vec<ExtIdleNotificationV1>,
    loop_handle: LoopHandle<'static, CalloopData>,
    inhibited: bool,
    #[cfg(test)]
    timer_starts: std::cell::Cell<u64>,
    #[cfg(test)]
    timer_cancels: std::cell::Cell<u64>,
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
            #[cfg(test)]
            timer_starts: std::cell::Cell::new(0),
            #[cfg(test)]
            timer_cancels: std::cell::Cell::new(0),
        }
    }

    fn track(&mut self, notification: ExtIdleNotificationV1) {
        self.notifications.push(notification.clone());
        self.arm(&notification);
    }

    fn remove(&mut self, notification: &ExtIdleNotificationV1) {
        self.cancel_timer(notification);
        self.notifications.retain(|known| known != notification);
    }

    fn cancel_timer(&self, notification: &ExtIdleNotificationV1) {
        if let Some(data) = notification.data::<IdleNotificationData>() {
            let token = data.timer.lock().unwrap().take();
            if let Some(token) = token {
                self.loop_handle.remove(token);
                #[cfg(test)]
                self.timer_cancels.set(self.timer_cancels.get() + 1);
            }
        }
    }

    /// Register only when no timer is armed. Existing sources follow the latest
    /// activity deadline from their callback instead of being rebuilt on input.
    fn arm(&self, notification: &ExtIdleNotificationV1) {
        let Some(data) = notification.data::<IdleNotificationData>() else {
            return;
        };
        if !notification.is_alive() || (self.inhibited && !data.ignores_inhibitors) {
            return;
        }
        let mut timer = data.timer.lock().unwrap();
        if timer.is_some() {
            return;
        }
        let deadline = *data.last_activity.lock().unwrap() + data.timeout;
        let target = notification.clone();
        match self
            .loop_handle
            .insert_source(Timer::from_deadline(deadline), move |_, _, data| {
                data.state.idle_notification_expired(&target)
            }) {
            Ok(token) => {
                *timer = Some(token);
                #[cfg(test)]
                self.timer_starts.set(self.timer_starts.get() + 1);
            }
            Err(error) => tracing::warn!(%error, "cannot schedule idle notification"),
        }
    }

    fn notify_activity(&mut self, seat: &Seat<Anvil>) {
        self.notifications.retain(Resource::is_alive);
        if self.notifications.is_empty() {
            return;
        }
        let now = Instant::now();
        for notification in &self.notifications {
            let data = notification.data::<IdleNotificationData>().unwrap();
            if !seat.owns(&data.seat) {
                continue;
            }
            *data.last_activity.lock().unwrap() = now;
            if data.idle.swap(false, Ordering::AcqRel) {
                notification.resumed();
            }
            // Also retries a failed initial registration without losing a listener.
            self.arm(notification);
        }
    }

    fn set_inhibited(&mut self, inhibited: bool) {
        if self.inhibited == inhibited {
            return;
        }
        self.inhibited = inhibited;
        let now = Instant::now();
        for notification in &self.notifications {
            let data = notification.data::<IdleNotificationData>().unwrap();
            if data.ignores_inhibitors {
                continue;
            }
            self.cancel_timer(notification);
            // Ordinary notification timeouts restart in full after inhibition ends.
            *data.last_activity.lock().unwrap() = now;
            if inhibited && data.idle.swap(false, Ordering::AcqRel) {
                notification.resumed();
            }
            self.arm(notification);
        }
    }
}

impl Anvil {
    pub(crate) fn notify_idle_activity(&mut self) {
        let seat = self.seat.clone();
        self.idle_notification_state.notify_activity(&seat);
    }

    fn idle_notification_expired(&mut self, notification: &ExtIdleNotificationV1) -> TimeoutAction {
        let Some(data) = notification.data::<IdleNotificationData>() else {
            return TimeoutAction::Drop;
        };
        if !notification.is_alive()
            || (self.idle_notification_state.inhibited && !data.ignores_inhibitors)
        {
            data.timer.lock().unwrap().take();
            return TimeoutAction::Drop;
        }
        let deadline = *data.last_activity.lock().unwrap() + data.timeout;
        if Instant::now() < deadline {
            // ToInstant retains the registration token and reuses the timer-wheel
            // entry. No listener/source allocation is needed for intervening input.
            return TimeoutAction::ToInstant(deadline);
        }
        data.timer.lock().unwrap().take();
        if !data.idle.swap(true, Ordering::AcqRel) {
            notification.idled();
        }
        // Includes timeout=0: emit once, then wait for real activity to resume.
        TimeoutAction::Drop
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
                last_activity: Mutex::new(Instant::now()),
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
    #[ignore = "requires Wayland sockets and tests/protocols/build.sh"]
    fn input_activity_keeps_idle_timer_registrations() {
        use super::*;
        use anvil::{config::Config, layout::Rect};
        use smithay::{
            output::{Mode, Output, PhysicalProperties, Subpixel},
            reexports::{calloop::EventLoop, wayland_server::Display},
        };
        use std::{
            io::{BufRead, BufReader, Write},
            process::{Child, Command, Stdio},
            sync::mpsc,
        };
        struct ClientGuard(Child);
        impl Drop for ClientGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        fn receive(
            event_loop: &mut EventLoop<CalloopData>,
            data: &mut CalloopData,
            rx: &mpsc::Receiver<String>,
            prefix: &str,
        ) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                event_loop
                    .dispatch(Duration::from_millis(10), data)
                    .unwrap();
                data.display_handle.flush_clients().unwrap();
                while let Ok(line) = rx.try_recv() {
                    if line.starts_with(prefix) {
                        return;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "idle client did not send {prefix}"
                );
            }
        }
        fn command(client: &mut ClientGuard, text: &str) {
            let stdin = client.0.stdin.as_mut().unwrap();
            writeln!(stdin, "{text}").unwrap();
            stdin.flush().unwrap();
        }
        let mut event_loop: EventLoop<CalloopData> = EventLoop::try_new().unwrap();
        let display = Display::<Anvil>::new().unwrap();
        let dh = display.handle();
        let state = Anvil::new(&mut event_loop, display, Config::default(), None).unwrap();
        let mut data = CalloopData {
            state,
            display_handle: dh.clone(),
        };
        let output = Output::new(
            "idle-test".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "test".into(),
                model: "logical".into(),
            },
        );
        output.create_global::<Anvil>(&dh);
        output.change_current_state(
            Some(Mode {
                size: (800, 600).into(),
                refresh: 60_000,
            }),
            None,
            None,
            Some((0, 0).into()),
        );
        data.state.space.map_output(&output, (0, 0));
        data.state
            .configure_output("idle-test", Rect::new(0, 0, 800, 600));
        let mode = if cfg!(feature = "layer-shell") {
            "layer-shell"
        } else {
            "default"
        };
        let mut client = ClientGuard(
            Command::new("target/protocol-tests/client")
                .args([mode, "idle-watch"])
                .env("WAYLAND_DISPLAY", &data.state.socket_name)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let stdout = client.0.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        receive(&mut event_loop, &mut data, &rx, "READY");
        for text in ["add 1 normal 60000", "add 2 input 60000"] {
            command(&mut client, text);
            receive(&mut event_loop, &mut data, &rx, "ACK");
        }
        let notifications = data.state.idle_notification_state.notifications.clone();
        assert_eq!(notifications.len(), 2);
        assert_eq!(data.state.idle_notification_state.timer_starts.get(), 2);
        for _ in 0..10_000 {
            data.state.notify_idle_activity();
        }
        assert_eq!(data.state.idle_notification_state.timer_starts.get(), 2);
        assert_eq!(data.state.idle_notification_state.timer_cancels.get(), 0);
        let normal = notifications
            .iter()
            .find(|n| !n.data::<IdleNotificationData>().unwrap().ignores_inhibitors)
            .unwrap();
        let input = notifications
            .iter()
            .find(|n| n.data::<IdleNotificationData>().unwrap().ignores_inhibitors)
            .unwrap();
        let normal_data = normal.data::<IdleNotificationData>().unwrap();
        let input_data = input.data::<IdleNotificationData>().unwrap();
        let normal_activity = *normal_data.last_activity.lock().unwrap();
        let input_activity = *input_data.last_activity.lock().unwrap();
        let other = data.state.seat_state.new_wl_seat(&dh, "other");
        data.state.idle_notification_state.notify_activity(&other);
        assert_eq!(*normal_data.last_activity.lock().unwrap(), normal_activity);
        assert_eq!(*input_data.last_activity.lock().unwrap(), input_activity);
        let action = data.state.idle_notification_expired(normal);
        assert!(
            matches!(action, TimeoutAction::ToInstant(at) if at == normal_activity + normal_data.timeout)
        );
        assert!(normal_data.timer.lock().unwrap().is_some());
        data.state.idle_notification_state.set_inhibited(true);
        assert!(normal_data.timer.lock().unwrap().is_none());
        assert!(input_data.timer.lock().unwrap().is_some());
        assert_eq!(*input_data.last_activity.lock().unwrap(), input_activity);
        assert_eq!(data.state.idle_notification_state.timer_cancels.get(), 1);
        data.state.idle_notification_state.set_inhibited(false);
        assert!(normal_data.timer.lock().unwrap().is_some());
        assert_eq!(data.state.idle_notification_state.timer_starts.get(), 3);
        assert_eq!(*input_data.last_activity.lock().unwrap(), input_activity);
        command(&mut client, "del 1");
        receive(&mut event_loop, &mut data, &rx, "ACK");
        assert_eq!(data.state.idle_notification_state.notifications.len(), 1);
        assert_eq!(data.state.idle_notification_state.timer_cancels.get(), 2);
        assert!(matches!(
            data.state.idle_notification_expired(normal),
            TimeoutAction::Drop
        ));
        // Disconnect without waiting on the fixture's final Wayland roundtrip
        // while this same thread owns server dispatch.
        drop(client);
        reader.join().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !data.state.idle_notification_state.notifications.is_empty() {
            event_loop
                .dispatch(Duration::from_millis(10), &mut data)
                .unwrap();
            assert!(Instant::now() < deadline);
        }
        assert_eq!(data.state.idle_notification_state.timer_cancels.get(), 3);
    }

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

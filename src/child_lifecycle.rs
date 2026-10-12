//! Kernel-backed child exit notifications, so a four-process desktop does not
//! call wait4(WNOHANG) four times at every 16 ms compositor scheduler tick.
//! pidfds are a per-child solution: they neither mask SIGCHLD for other
//! threads/toolkits nor consume exits belonging to other process owners.

use std::process::Child;

use rustix::process::{Pid, PidfdFlags, pidfd_open};
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction, generic::Generic};

use crate::CalloopData;

/// Returns false when pidfds cannot be used. The caller retains the Child and
/// enables a 1-second reap fallback. No orphan/zombie can be forgotten.
pub(crate) fn watch_child(child: &Child, handle: &LoopHandle<'static, CalloopData>) -> bool {
    let pid = child.id();
    let Some(raw_pid) = Pid::from_raw(pid as i32) else {
        return false;
    };
    let fd = match pidfd_open(raw_pid, PidfdFlags::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            tracing::debug!(pid, %error, "pidfd unavailable; using slow reap fallback");
            return false;
        }
    };
    match handle.insert_source(
        Generic::new(fd, Interest::READ, Mode::Level),
        move |_, _, data| {
            data.state.child_exited(pid);
            // PIDFD is no longer needed after child is reaped. Remove source to
            // prevent a permanently readable descriptor from waking Calloop.
            Ok(PostAction::Remove)
        },
    ) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(pid, %error, "unable to watch child pidfd; using fallback");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use rustix::process::{Pid, PidfdFlags, pidfd_open};
    use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic};
    use std::{
        process::Command,
        time::{Duration, Instant},
    };

    #[test]
    fn pidfd_readiness_reaps_child_without_polling() {
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 0.05")
            .spawn()
            .unwrap();
        let fd = pidfd_open(Pid::from_child(&child), PidfdFlags::empty()).unwrap();
        let mut loop_ = EventLoop::<()>::try_new().unwrap();
        let reaped = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = reaped.clone();
        loop_
            .handle()
            .insert_source(
                Generic::new(fd, Interest::READ, Mode::Level),
                move |_, _, _| {
                    observed.set(true);
                    Ok(PostAction::Remove)
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !reaped.get() && Instant::now() < deadline {
            loop_.dispatch(Duration::from_millis(100), &mut ()).unwrap();
        }
        assert!(reaped.get(), "pidfd must signal child exit");
        assert!(
            child.try_wait().unwrap().is_some(),
            "exited child must be reaped"
        );
    }
}

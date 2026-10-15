//! Output mode/scale configuration and the bounded Wayland event pump.
use super::input::Input;
use super::state::{ConfigurationResult, State};
use super::{Command, Commands};
use ironrdp_server::DesktopSize;
use std::{
    os::fd::{AsFd, AsRawFd},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use wayland_client::{Connection, protocol::wl_output};

pub(super) fn resize_output(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    name: &str,
    target: DesktopSize,
    scale: f64,
    alive: &AtomicBool,
) -> anyhow::Result<()> {
    if s.output_policy_matches(name, target, scale)
        && s.applied_scale == Some(scale)
        && s.applied_size == Some(target)
    {
        return Ok(());
    }
    let started = Instant::now();
    while s.output_serial.is_none() || s.heads.iter().all(|head| head.1 != name) {
        anyhow::ensure!(
            alive.load(Ordering::Acquire),
            "desktop stopped during resize"
        );
        anyhow::ensure!(
            s.output_manager.is_some(),
            "missing output management protocol"
        );
        pump(c, q, s)?;
        anyhow::ensure!(
            started.elapsed() < Duration::from_secs(3),
            "output management timed out"
        );
    }
    let h = q.handle();
    loop {
        let manager = s
            .output_manager
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("output manager closed"))?;
        let config = manager.create_configuration(s.output_serial.unwrap(), &h, ());
        for (head, head_name, enabled, _) in &s.heads {
            if head_name == name {
                let conf = config.enable_head(head, &h, ());
                conf.set_custom_mode(target.width.into(), target.height.into(), 60_000);
                conf.set_scale(scale);
                conf.set_transform(wl_output::Transform::Normal);
                conf.set_position(0, 0);
            } else if *enabled {
                // Niri retains the existing properties of other enabled heads.
                config.enable_head(head, &h, ());
            } else {
                config.disable_head(head);
            }
        }
        s.configuration_result = None;
        config.apply();
        let result = (|| -> anyhow::Result<bool> {
            loop {
                anyhow::ensure!(
                    alive.load(Ordering::Acquire),
                    "desktop stopped during resize"
                );
                pump(c, q, s)?;
                anyhow::ensure!(
                    started.elapsed() < Duration::from_secs(3),
                    "output resize not applied"
                );
                match s.configuration_result {
                    Some(ConfigurationResult::Failed) => anyhow::bail!("output resize rejected"),
                    Some(ConfigurationResult::Cancelled) => return Ok(false),
                    Some(ConfigurationResult::Succeeded)
                        if s.output_policy_matches(name, target, scale) =>
                    {
                        return Ok(true);
                    }
                    _ => {}
                }
            }
        })();
        config.destroy();
        if result? {
            s.applied_scale = Some(scale);
            s.applied_size = Some(target);
            return Ok(());
        }
        // Geometry notifications can invalidate the serial between creating
        // and applying a configuration. Sync the updated metadata and retry
        // within the same deadline, using the compositor's latest serial.
        s.synced = false;
        c.display().sync(&h, ());
        while !s.synced {
            anyhow::ensure!(
                alive.load(Ordering::Acquire),
                "desktop stopped during resize"
            );
            pump(c, q, s)?;
            anyhow::ensure!(
                started.elapsed() < Duration::from_secs(3),
                "output resize retry timed out"
            );
        }
    }
}

pub(super) fn pump(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
) -> anyhow::Result<()> {
    pump_with_timeout(c, q, s, 50)
}

fn pump_with_timeout(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
) -> anyhow::Result<()> {
    pump_wake(c, q, s, timeout_ms, None)
}

fn complete_wayland_io<T>(
    result: Result<T, wayland_client::backend::WaylandError>,
) -> anyhow::Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            // Another coordinated event queue can consume socket readiness.
            // The next pump retries reads/flushes without losing pending events.
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Dispatch one batch of compositor events, waiting up to `timeout_ms` for it
/// (or for a command wake), then publish the cursor once for the whole batch.
pub(super) fn pump_wake(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
    commands: Option<&Commands>,
) -> anyhow::Result<()> {
    dispatch_batch(c, q, s, timeout_ms, commands)?;
    // A leave+enter pair, or a shape and its position, never reach RDP split
    // across cursor snapshots.
    s.service_cursor(&q.handle());
    Ok(())
}

fn dispatch_batch(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    timeout_ms: i32,
    commands: Option<&Commands>,
) -> anyhow::Result<()> {
    let dispatched = q.dispatch_pending(s)?;
    // A full Wayland socket needs POLLOUT; never block while holding a read
    // guard or mistake command-only readiness for readable Wayland bytes.
    let want_write = match c.flush() {
        Ok(()) => false,
        Err(wayland_client::backend::WaylandError::Io(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            ) =>
        {
            true
        }
        Err(error) => return Err(error.into()),
    };
    // Pending callbacks can already satisfy negotiation/readback. Recheck the
    // caller's condition before entering another potentially long poll; the
    // Wayland socket may now legitimately be quiet after those events.
    if dispatched > 0 {
        s.validate_output()?;
        return Ok(());
    }
    if let Some(guard) = q.prepare_read() {
        poll_prepared_wayland(c, q, s, guard, timeout_ms, commands, want_write)?;
    }
    q.dispatch_pending(s)?;
    s.validate_output()?;
    Ok(())
}

fn poll_prepared_wayland(
    c: &Connection,
    q: &mut wayland_client::EventQueue<State>,
    s: &mut State,
    guard: wayland_client::backend::ReadEventsGuard,
    timeout_ms: i32,
    commands: Option<&Commands>,
    want_write: bool,
) -> anyhow::Result<()> {
    // A different coordinated queue can read between dispatch_pending and
    // prepare_read, enqueueing our callbacks while draining socket readiness.
    // Once this guard is acquired, the backend prevents another socket read;
    // checking again here closes that window before any potentially long poll.
    if q.dispatch_pending(s)? > 0 {
        drop(guard);
        return Ok(());
    }
    let mut fds = [
        libc::pollfd {
            fd: c.as_fd().as_raw_fd(),
            events: libc::POLLIN | if want_write { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: commands.map_or(-1, |c| c.wake.as_raw_fd()),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: both entries are initialized and valid for this call; -1 is ignored.
    let n = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
    if n < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    if n > 0 && fds[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        complete_wayland_io(guard.read())?;
    } else {
        // Cancels prepared read on command wakes, deadlines, EINTR and POLLOUT.
        drop(guard);
    }
    if fds[1].revents & libc::POLLIN != 0 {
        commands.unwrap().clear_wake();
    }
    if want_write && fds[0].revents & libc::POLLOUT != 0 {
        complete_wayland_io(c.flush())?;
    }
    Ok(())
}

/// Resize is a geometry barrier. Cancel the pending capture before consuming
/// it, so later coordinates are delivered only after its output mode commits.
pub(super) fn service_input(
    commands: &Commands,
    input: &mut Input,
    connection: &Connection,
    clipboard: &mut Option<crate::clipboard::ClipboardAttachment>,
    state: &mut State,
) -> bool {
    // A config reload can change scale without a client resize. Keep queued
    // coordinates behind the same geometry barrier as explicit layout changes.
    if state.remote_geometry_changed() {
        return true;
    }
    for _ in 0..256 {
        if matches!(
            commands.queue.lock().unwrap().front(),
            Some(Command::Resize(..) | Command::Stop)
        ) {
            return true;
        }
        match commands.pop() {
            Some(Command::Resize(..)) => unreachable!("single command consumer"),
            Some(Command::Status) => state.refresh_requested = true,
            Some(Command::Cursor(max)) => {
                state.set_cursor_max(max);
                return true;
            }
            Some(Command::Key(event)) => input.key_event(event),
            Some(Command::Mouse(event)) => {
                input.mouse_event(event);
                state.note_injected(input.point);
            }
            Some(Command::Release) => input.release(),
            Some(Command::Clipboard(next)) => {
                if next.attach_connection(connection.clone()) {
                    *clipboard = Some(next);
                }
            }
            Some(Command::Stop) => return true,
            None => break,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::input::{PhysicalKeymap, WheelAccumulator};
    use super::*;
    use crate::keyboard::UnicodeInput;
    use ironrdp_server::KeyboardEvent;
    use std::{
        collections::BTreeSet,
        io::{Read, Write},
        os::unix::net::UnixStream,
    };
    use wayland_client::{Proxy, protocol::wl_seat};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    use xkbcommon::xkb;
    #[test]
    fn pending_wayland_completion_is_dispatched_without_an_extra_timeout() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut queue = connection.new_event_queue();
        let callback = connection.display().sync(&queue.handle(), ());
        connection.flush().unwrap();
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        queue.prepare_read().unwrap().read().unwrap();
        let mut state = State::default();
        let started = Instant::now();
        pump_wake(&connection, &mut queue, &mut state, 1000, None).unwrap();
        assert!(state.synced);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a completed callback must not consume the next capture deadline"
        );
    }
    #[test]
    fn coordinated_empty_wayland_read_and_full_flush_are_retryable() {
        let (socket, _compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let queue = connection.new_event_queue::<State>();
        // Reading after another queue drained readiness is a normal condition.
        complete_wayland_io(queue.prepare_read().unwrap().read()).unwrap();
        complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
            std::io::Error::from(std::io::ErrorKind::WouldBlock),
        )))
        .unwrap();
        assert!(
            complete_wayland_io::<()>(Err(wayland_client::backend::WaylandError::Io(
                std::io::Error::from(std::io::ErrorKind::BrokenPipe),
            )))
            .is_err()
        );
        let mut state = State::default();
        pump_wake(
            &connection,
            &mut connection.new_event_queue(),
            &mut state,
            0,
            None,
        )
        .unwrap();
    }
    #[test]
    fn foreign_queue_read_between_dispatch_and_prepare_does_not_sleep_on_drained_socket() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut capture = connection.new_event_queue::<State>();
        let clipboard = connection.clone().new_event_queue::<State>();
        let callback = connection.display().sync(&capture.handle(), ());
        connection.flush().unwrap();
        let mut state = State::default();
        assert_eq!(capture.dispatch_pending(&mut state).unwrap(), 0);
        let mut reply = Vec::new();
        reply.extend_from_slice(&callback.id().protocol_id().to_ne_bytes());
        reply.extend_from_slice(&(12u32 << 16).to_ne_bytes());
        reply.extend_from_slice(&1u32.to_ne_bytes());
        compositor.write_all(&reply).unwrap();
        // Deterministically model the clipboard reader winning immediately
        // after capture's first dispatch and before its prepared-read guard.
        clipboard.prepare_read().unwrap().read().unwrap();
        let mut fd = libc::pollfd {
            fd: connection.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd is initialized and refers to the live connection.
        assert_eq!(
            unsafe { libc::poll(&mut fd, 1, 0) },
            0,
            "foreign reader drained socket readiness"
        );
        let guard = capture.prepare_read().unwrap();
        let started = Instant::now();
        poll_prepared_wayland(
            &connection,
            &mut capture,
            &mut state,
            guard,
            1000,
            None,
            false,
        )
        .unwrap();
        // pump_wake dispatches again after its prepared-read polling stage.
        capture.dispatch_pending(&mut state).unwrap();
        assert!(
            state.synced,
            "completion queued by the foreign reader must dispatch"
        );
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "queued completion must not wait for quiet socket readiness"
        );
        // The early return canceled the guard; later prepared reads still work.
        drop(capture.prepare_read().unwrap());
    }
    #[test]
    fn input_wakes_pending_capture_without_waiting_for_wayland_readiness() {
        let (socket, mut compositor) = UnixStream::pair().unwrap();
        compositor
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let connection = Connection::from_socket(socket).unwrap();
        let mut events = connection.new_event_queue();
        let queue = Commands::new().unwrap();
        let producer = queue.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer
                .try_send(Command::Key(KeyboardEvent::Pressed {
                    code: 30,
                    extended: false,
                }))
                .unwrap();
        });
        let handle = events.handle();
        let registry = connection.display().get_registry(&handle, ());
        let seat: wl_seat::WlSeat = registry.bind(1, 7, &handle, ());
        let keyboard_manager: ZwpVirtualKeyboardManagerV1 = registry.bind(2, 1, &handle, ());
        let pointer_manager: ZwlrVirtualPointerManagerV1 = registry.bind(3, 2, &handle, ());
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .unwrap();
        let mut input = Input {
            keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            text_keyboard: keyboard_manager.create_virtual_keyboard(&seat, &handle, ()),
            physical_keymap: PhysicalKeymap::new(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
                .unwrap(),
            unicode: UnicodeInput::default(),
            pointer: pointer_manager.create_virtual_pointer(Some(&seat), &handle, ()),
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            point: (0, 0),
            size: (640, 480),
            target: DesktopSize {
                width: 640,
                height: 480,
            },
            scale: 1.0,
            wheel: WheelAccumulator::default(),
            started: Instant::now(),
        };
        let started = Instant::now();
        let mut state = State::default();
        // No compositor negotiation/ready message is sent. The only readable
        // descriptor is the command eventfd, which must cancel prepare_read.
        pump_wake(&connection, &mut events, &mut state, 1000, Some(&queue)).unwrap();
        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(!state.ready);
        assert!(!service_input(
            &queue,
            &mut input,
            &connection,
            &mut None,
            &mut state
        ));
        connection.flush().unwrap();
        assert!(input.keys.contains(&30));
        let mut wire = [0u8; 4096];
        let size = compositor.read(&mut wire).unwrap();
        let mut offset = 0;
        let mut delivered = false;
        while offset + 8 <= size {
            let object = u32::from_ne_bytes(wire[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(wire[offset + 4..offset + 8].try_into().unwrap());
            let length = (header >> 16) as usize;
            if object == input.keyboard.id().protocol_id() && (header & 0xffff) == 1 {
                assert_eq!(
                    u32::from_ne_bytes(wire[offset + 12..offset + 16].try_into().unwrap()),
                    30
                );
                delivered = true;
            }
            offset += length;
        }
        assert!(
            delivered,
            "virtual key must reach the compositor socket before capture ready"
        );
        assert!(!state.ready);
        input.scale = 1.5;
        input.point = (320, 240);
        input.relative(15, -30);
        input.pointer.frame();
        connection.flush().unwrap();
        let size = compositor.read(&mut wire).unwrap();
        let mut offset = 0;
        let mut relative_delivered = false;
        while offset + 8 <= size {
            let object = u32::from_ne_bytes(wire[offset..offset + 4].try_into().unwrap());
            let header = u32::from_ne_bytes(wire[offset + 4..offset + 8].try_into().unwrap());
            if object == input.pointer.id().protocol_id() && header & 0xffff == 0 {
                assert_eq!(
                    i32::from_ne_bytes(wire[offset + 12..offset + 16].try_into().unwrap()),
                    10 * 256
                );
                assert_eq!(
                    i32::from_ne_bytes(wire[offset + 16..offset + 20].try_into().unwrap()),
                    -20 * 256
                );
                relative_delivered = true;
            }
            offset += (header >> 16) as usize;
        }
        assert!(
            relative_delivered,
            "physical deltas must reach Wayland as scaled relative motion"
        );
        assert_eq!(input.point, (335, 210));
        drop(input);
        drop(seat);
        drop(keyboard_manager);
        drop(pointer_manager);
        drop(registry);
        drop(handle);
        // A later prepared read remains valid after cancellation and shutdown.
        queue.try_send(Command::Stop).unwrap();
        pump_wake(&connection, &mut events, &mut state, 1000, Some(&queue)).unwrap();
        assert!(matches!(queue.pop(), Some(Command::Stop)));
        worker.join().unwrap();
        drop(events);
        drop(connection);
        let mut bytes = [0u8; 16];
        assert_eq!(compositor.read(&mut bytes).unwrap(), 0);
    }
}

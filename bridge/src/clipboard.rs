//! Text-only CLIPRDR on the authenticated compositor's existing connection.
//! No process environment, local display socket, bus or clipboard text is logged.
use ironrdp_cliprdr::{
    backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory},
    pdu::*,
};
use ironrdp_core::{IntoOwned, impl_as_any};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use std::{
    collections::HashMap,
    io::{Read, Write},
    os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle, delegate_noop,
    protocol::{wl_callback, wl_registry, wl_seat},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
const MIME: &str = "text/plain;charset=utf-8";
const OWN_MIME: &str = "application/x-niri-rdp-private-text";

#[derive(Default)]
struct Shared {
    sender: Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>,
    ready: bool,
    authenticated: bool,
    generation: u64,
    wake: Option<Arc<OwnedFd>>,
    remote: Option<Option<String>>,
    local: Option<String>,
    dirty: bool,
    advertised: bool,
    response_pending: bool,
}
impl Shared {
    fn send(&self, message: ClipboardMessage) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ServerEvent::Clipboard(message));
        }
    }
    fn advertise(&mut self) {
        if self.ready && self.authenticated && self.dirty && !self.advertised {
            self.dirty = false;
            self.advertised = true;
            let formats = if self.local.is_some() {
                vec![ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
            } else {
                Vec::new()
            };
            self.send(ClipboardMessage::SendInitiateCopy(formats));
        }
    }
    fn wake(&self) {
        if let Some(fd) = &self.wake {
            let value = 1u64;
            // SAFETY: eventfd and the eight-byte source are valid. A full fd
            // already represents a pending notification.
            unsafe { libc::write(fd.as_raw_fd(), (&value as *const u64).cast(), 8) };
        }
    }
}
#[derive(Clone, Default)]
pub struct Clipboard {
    shared: Arc<Mutex<Shared>>,
    worker: Arc<Mutex<Option<Worker>>>,
}
struct Worker {
    thread: std::thread::JoinHandle<()>,
    done: std::sync::mpsc::Receiver<()>,
}
/// Reserved before a capture command is queued. Revocation invalidates both
/// a queued attachment and cleanup belonging to the previous desktop.
pub(crate) struct ClipboardAttachment {
    clipboard: Clipboard,
    generation: u64,
}
impl ClipboardAttachment {
    pub(crate) fn attach_connection(&self, connection: Connection) -> bool {
        self.clipboard
            .attach_generation(connection, self.generation)
    }
    pub(crate) fn clear(&self) {
        self.clipboard.reset(Some(self.generation));
    }
}
impl std::fmt::Debug for Clipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateTextClipboard")
    }
}
impl Clipboard {
    pub(crate) fn attachment(&self) -> ClipboardAttachment {
        ClipboardAttachment {
            clipboard: self.clone(),
            generation: self.reset(None).expect("unconditional clipboard reset"),
        }
    }
    /// Called only with the authenticated desktop's reserved ticket. Connection
    /// cloning shares Wayland's coordinated backend, never raw-stream readers.
    fn attach_generation(&self, connection: Connection, generation: u64) -> bool {
        // SAFETY: eventfd returns a new owned descriptor or a negative error.
        let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if raw < 0 {
            eprintln!("private text clipboard wake unavailable");
            return false;
        }
        // SAFETY: raw was checked and has one owner.
        let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        // Publish authentication and the owned worker atomically with respect
        // to reset. A command popped before disconnect cannot adopt a newer
        // generation when it eventually runs.
        let mut state = self.shared.lock().unwrap();
        if state.generation != generation {
            return false;
        }
        let mut slot = self.worker.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        state.authenticated = true;
        state.wake = Some(wake.clone());
        let shared = self.shared.clone();
        let (done, completion) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            if worker(connection, shared.clone(), wake, generation).is_err() {
                eprintln!("private text clipboard unavailable");
            }
            let mut shared = shared.lock().unwrap();
            if shared.generation == generation {
                shared.authenticated = false;
                shared.local = None;
                shared.remote = None;
                shared.wake = None;
            }
            drop(shared);
            let _ = done.send(());
        });
        *slot = Some(Worker {
            thread: worker,
            done: completion,
        });
        true
    }
    pub fn clear(&self) {
        self.reset(None);
    }
    fn reset(&self, expected_generation: Option<u64>) -> Option<u64> {
        let mut shared = self.shared.lock().unwrap();
        if expected_generation.is_some_and(|generation| shared.generation != generation) {
            return None;
        }
        shared.generation = shared.generation.wrapping_add(1);
        let generation = shared.generation;
        shared.authenticated = false;
        shared.local = None;
        shared.remote = None;
        shared.dirty = false;
        shared.advertised = false;
        shared.response_pending = false;
        shared.wake();
        shared.wake = None;
        // Detach the old worker while holding the generation lock, and release
        // both locks before waiting. Concurrent reset cannot take a newer
        // worker, and attachment cannot store one behind a completed clear.
        let worker = self.worker.lock().unwrap().take();
        drop(shared);
        if let Some(worker) = worker {
            // Stop wakes prepare-read/poll immediately. Keep teardown finite if
            // another Wayland queue is still finishing its bounded read guard.
            if worker.done.recv_timeout(Duration::from_secs(2)).is_ok() {
                let _ = worker.thread.join();
            } else {
                eprintln!("private clipboard teardown exceeded its deadline");
            }
        }
        Some(generation)
    }
}
impl ServerEventSender for Clipboard {
    fn set_sender(&mut self, sender: tokio::sync::mpsc::UnboundedSender<ServerEvent>) {
        self.shared.lock().unwrap().sender = Some(sender);
    }
}
impl CliprdrServerFactory for Clipboard {
    fn on_format_data_response_drained(&self) {
        self.shared.lock().unwrap().response_pending = false;
    }
}
impl CliprdrBackendFactory for Clipboard {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        self.shared.lock().unwrap().ready = false;
        Box::new(Backend {
            clipboard: self.clone(),
            request: None,
            revision: 0,
            latest_copy: false,
            remote_disabled: false,
        })
    }
}
struct Backend {
    clipboard: Clipboard,
    request: Option<(u64, Instant)>,
    revision: u64,
    latest_copy: bool,
    remote_disabled: bool,
}
impl_as_any!(Backend);
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateTextBackend")
    }
}
impl Backend {
    fn request_text(&mut self) {
        if self.request.is_some_and(|(_, at)| at.elapsed() > TIMEOUT) {
            // Responses have no request IDs. Retrying after a timeout could
            // publish an old response as a newer selection; disable this
            // direction until reconnect and keep the desktop usable.
            self.request = None;
            self.remote_disabled = true;
            eprintln!("remote clipboard transfer timed out; reconnect to restore remote copy");
        }
        if self.remote_disabled {
            return;
        }
        if self.request.is_none() && self.latest_copy {
            let shared = self.clipboard.shared.lock().unwrap();
            if shared.authenticated {
                self.request = Some((self.revision, Instant::now()));
                shared.send(ClipboardMessage::SendInitiatePaste(
                    ClipboardFormatId::CF_UNICODETEXT,
                ));
            }
        }
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.clipboard.clear();
    }
}
impl CliprdrBackend for Backend {
    fn temporary_directory(&self) -> &str {
        ""
    }
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }
    fn on_ready(&mut self) {
        let mut s = self.clipboard.shared.lock().unwrap();
        s.ready = true;
        s.advertise();
    }
    fn on_request_format_list(&mut self) {
        let mut s = self.clipboard.shared.lock().unwrap();
        s.dirty = true;
        s.advertise();
    }
    fn on_format_list_response(&mut self, _: bool) {
        let mut s = self.clipboard.shared.lock().unwrap();
        s.advertised = false;
        s.advertise();
    }
    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}
    fn on_remote_copy(&mut self, formats: &[ClipboardFormat]) {
        self.revision = self.revision.wrapping_add(1);
        self.latest_copy = formats
            .iter()
            .any(|f| f.id() == ClipboardFormatId::CF_UNICODETEXT);
        if !self.latest_copy {
            let mut s = self.clipboard.shared.lock().unwrap();
            if s.authenticated {
                s.remote = Some(None);
                s.wake();
            }
        }
        self.request_text();
    }
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let mut s = self.clipboard.shared.lock().unwrap();
        if s.response_pending {
            return;
        }
        s.response_pending = true;
        let response = if s.authenticated && request.format == ClipboardFormatId::CF_UNICODETEXT {
            s.local.as_ref().map(|text| {
                FormatDataResponse::new_unicode_string(
                    &text.replace("\r\n", "\n").replace('\n', "\r\n"),
                )
            })
        } else {
            None
        }
        .unwrap_or_else(FormatDataResponse::new_error);
        s.send(ClipboardMessage::SendFormatData(response.into_owned()));
    }
    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        if self.remote_disabled {
            return;
        }
        let Some((revision, started)) = self.request.take() else {
            return;
        };
        if started.elapsed() > TIMEOUT {
            self.remote_disabled = true;
            eprintln!("remote clipboard transfer timed out; reconnect to restore remote copy");
            return;
        }
        if revision == self.revision
            && started.elapsed() <= TIMEOUT
            && !response.is_error()
            && let Some(text) = decode_unicode(response.data())
        {
            let mut s = self.clipboard.shared.lock().unwrap();
            if s.authenticated {
                s.remote = Some(Some(text));
                s.wake();
            }
        }
        if revision != self.revision {
            self.request_text();
        }
    }
    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}
    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}
    fn on_lock(&mut self, _: LockDataId) {}
    fn on_unlock(&mut self, _: LockDataId) {}
}
fn decode_unicode(bytes: &[u8]) -> Option<String> {
    // LF -> CRLF plus UTF-16 can use four wire bytes per UTF-8 input byte.
    if bytes.len() > MAX_TEXT_BYTES * 4 + 2 || bytes.len() < 2 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut units: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    if units.pop()? != 0 || units.contains(&0) {
        return None;
    }
    let text = String::from_utf16(&units).ok()?.replace("\r\n", "\n");
    (text.len() <= MAX_TEXT_BYTES).then_some(text)
}
fn text_mime(types: &[String]) -> Option<&str> {
    [
        MIME,
        "text/plain;charset=UTF-8",
        "UTF8_STRING",
        "text/plain",
    ]
    .into_iter()
    .find(|wanted| types.iter().any(|offered| offered == wanted))
}
struct Receive {
    fd: std::fs::File,
    data: Vec<u8>,
    deadline: Instant,
    serial: u64,
}
struct SendText {
    fd: std::fs::File,
    text: Arc<[u8]>,
    offset: usize,
    deadline: Instant,
}
struct WaylandState {
    shared: Arc<Mutex<Shared>>,
    generation: u64,
    manager: Option<ZwlrDataControlManagerV1>,
    seat: Option<wl_seat::WlSeat>,
    device: Option<ZwlrDataControlDeviceV1>,
    offers: HashMap<wayland_client::backend::ObjectId, (ZwlrDataControlOfferV1, Vec<String>)>,
    source: Option<ZwlrDataControlSourceV1>,
    source_text: Arc<[u8]>,
    receive: Option<Receive>,
    sends: Vec<SendText>,
    serial: u64,
    synced: bool,
    finished: bool,
}
impl WaylandState {
    fn pending_remote(&self) -> Option<Option<String>> {
        // CLIPRDR may deliver the first copy while the private registry sync
        // is still running. Keep it queued until set_selection can succeed.
        self.device.as_ref()?;
        let mut shared = self.shared.lock().unwrap();
        if shared.generation != self.generation || !shared.authenticated {
            return None;
        }
        shared.remote.take()
    }
    fn local_text(&self, text: Option<String>) {
        let mut s = self.shared.lock().unwrap();
        if s.generation != self.generation || !s.authenticated {
            return;
        }
        s.local = text;
        s.dirty = true;
        s.advertise();
    }
    fn remote_text(&mut self, text: Option<String>, qh: &QueueHandle<Self>) {
        self.receive = None;
        // Replace the previous native owner's cached text immediately. Do not
        // advertise it back to the peer that supplied this new selection.
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.generation != self.generation || !shared.authenticated {
                return;
            }
            shared.local = text.clone();
            shared.dirty = false;
        }
        if let Some(source) = self.source.take() {
            source.destroy();
        }
        let Some(manager) = &self.manager else {
            return;
        };
        let Some(device) = &self.device else {
            return;
        };
        if let Some(text) = text {
            self.source_text = text.into_bytes().into();
            let source = manager.create_data_source(qh, ());
            for mime in [MIME, "text/plain", "UTF8_STRING", OWN_MIME] {
                source.offer(mime.into());
            }
            device.set_selection(Some(&source));
            self.source = Some(source);
        } else {
            device.set_selection(None);
            self.source_text = Arc::from([]);
        }
    }
}
impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        s: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwlr_data_control_manager_v1" if s.manager.is_none() => {
                    s.manager = Some(registry.bind(name, version.min(2), qh, ()))
                }
                "wl_seat" if s.seat.is_none() => {
                    s.seat = Some(registry.bind(name, version.min(7), qh, ()))
                }
                _ => {}
            }
        }
    }
}
impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn event(
        s: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        s.synced = true;
    }
}
impl Dispatch<ZwlrDataControlDeviceV1, ()> for WaylandState {
    fn event(
        s: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                if s.offers.len() < 8 {
                    s.offers.insert(id.id(), (id, Vec::new()));
                } else {
                    id.destroy();
                }
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                s.serial = s.serial.wrapping_add(1);
                s.receive = None;
                let selected = id.and_then(|id| s.offers.remove(&id.id()));
                for (_, (offer, _)) in s.offers.drain() {
                    offer.destroy();
                }
                if let Some((offer, types)) = selected {
                    if !types.iter().any(|t| t == OWN_MIME) {
                        // An ownership change invalidates the previous cache
                        // even when pipe creation or the new transfer fails.
                        {
                            let mut shared = s.shared.lock().unwrap();
                            if shared.generation == s.generation {
                                shared.local = None;
                            }
                        }
                        if let Some(mime) = text_mime(&types) {
                            if let Ok((reader, writer)) = std::io::pipe() {
                                // SAFETY: fcntl operates on our fresh pipe descriptors.
                                if unsafe {
                                    libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK)
                                } < 0
                                {
                                    offer.destroy();
                                    s.local_text(None);
                                    return;
                                }
                                offer.receive(mime.into(), writer.as_fd());
                                s.receive = Some(Receive {
                                    fd: OwnedFd::from(reader).into(),
                                    data: Vec::new(),
                                    deadline: Instant::now() + TIMEOUT,
                                    serial: s.serial,
                                });
                            } else {
                                s.local_text(None);
                            }
                        } else {
                            s.local_text(None);
                        }
                    }
                    offer.destroy();
                } else {
                    s.local_text(None);
                }
            }
            zwlr_data_control_device_v1::Event::Finished => s.finished = true,
            zwlr_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                if let Some((offer, _)) = s.offers.remove(&offer.id()) {
                    offer.destroy();
                }
            }
            _ => {}
        }
    }
    wayland_client::event_created_child!(WaylandState, ZwlrDataControlDeviceV1, [0 => (ZwlrDataControlOfferV1, ())]);
}
impl Dispatch<ZwlrDataControlOfferV1, ()> for WaylandState {
    fn event(
        s: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some((_, types)) = s.offers.get_mut(&offer.id())
            && types.len() < 32
            && mime_type.len() < 256
        {
            types.push(mime_type);
        }
    }
}
impl Dispatch<ZwlrDataControlSourceV1, ()> for WaylandState {
    fn event(
        s: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                if s.source.as_ref() == Some(source)
                    && text_mime(std::slice::from_ref(&mime_type)).is_some()
                    && s.sends.len() < 4
                {
                    // SAFETY: we own fd and bounded writes need nonblocking mode.
                    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                        return;
                    }
                    s.sends.push(SendText {
                        fd: fd.into(),
                        text: s.source_text.clone(),
                        offset: 0,
                        deadline: Instant::now() + TIMEOUT,
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if s.source.as_ref() == Some(source) {
                    s.source.take();
                    s.source_text = Arc::from([]);
                }
                source.destroy();
            }
            _ => {}
        }
    }
}
delegate_noop!(WaylandState: ignore wl_seat::WlSeat);
delegate_noop!(WaylandState: ignore ZwlrDataControlManagerV1);

fn worker(
    connection: Connection,
    shared: Arc<Mutex<Shared>>,
    wake: Arc<OwnedFd>,
    generation: u64,
) -> anyhow::Result<()> {
    let mut queue = connection.new_event_queue::<WaylandState>();
    let qh = queue.handle();
    let mut s = WaylandState {
        shared,
        generation,
        manager: None,
        seat: None,
        device: None,
        offers: HashMap::new(),
        source: None,
        source_text: Arc::from([]),
        receive: None,
        sends: Vec::with_capacity(4),
        serial: 0,
        synced: false,
        finished: false,
    };
    connection.display().get_registry(&qh, ());
    connection.display().sync(&qh, ());
    let startup = Instant::now() + TIMEOUT;
    loop {
        queue.dispatch_pending(&mut s)?;
        if s.finished {
            break;
        }
        {
            let shared = s.shared.lock().unwrap();
            if shared.generation != generation {
                break;
            }
        }
        if s.device.is_none() && s.synced {
            let manager = s
                .manager
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing private data control"))?;
            let seat = s
                .seat
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing private seat"))?;
            s.device = Some(manager.get_data_device(seat, &qh, ()));
        }
        if let Some(text) = s.pending_remote() {
            s.remote_text(text, &qh);
        }
        if s.device.is_none() && Instant::now() >= startup {
            anyhow::bail!("clipboard startup timeout");
        }
        if let Some(mut transfer) = s.receive.take() {
            let mut buffer = [0u8; 8192];
            let mut done = false;
            let mut failed = Instant::now() >= transfer.deadline;
            while !failed && !done {
                match transfer.fd.read(&mut buffer) {
                    Ok(0) => done = true,
                    Ok(n) if transfer.data.len() + n <= MAX_TEXT_BYTES => {
                        transfer.data.extend_from_slice(&buffer[..n])
                    }
                    Ok(_) => failed = true,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => failed = true,
                }
            }
            if done && !failed && transfer.serial == s.serial {
                let text = String::from_utf8(transfer.data)
                    .ok()
                    .filter(|text| !text.contains('\0'));
                s.local_text(text);
            } else if !failed {
                s.receive = Some(transfer);
            } else {
                s.local_text(None);
            }
        }
        s.sends.retain_mut(|transfer| {
            if Instant::now() >= transfer.deadline {
                return false;
            }
            match transfer.fd.write(&transfer.text[transfer.offset..]) {
                Ok(0) => false,
                Ok(n) => {
                    transfer.offset += n;
                    transfer.offset < transfer.text.len()
                }
                Err(e) => {
                    e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted
                }
            }
        });
        connection.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        // The capture queue may have read our events between dispatch_pending
        // and prepare_read. Its read is complete once this guard is held; drain
        // those events before polling a socket that can now remain quiet.
        if queue.dispatch_pending(&mut s)? > 0 {
            drop(guard);
            continue;
        }
        let mut fds = vec![
            libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let mut deadline = if s.device.is_none() {
            Some(startup)
        } else {
            None
        };
        if let Some(transfer) = &s.receive {
            fds.push(libc::pollfd {
                fd: transfer.fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
            deadline = Some(deadline.map_or(transfer.deadline, |last| last.min(transfer.deadline)));
        }
        for transfer in &s.sends {
            fds.push(libc::pollfd {
                fd: transfer.fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            });
            deadline = Some(deadline.map_or(transfer.deadline, |last| last.min(transfer.deadline)));
        }
        let timeout = deadline.map_or(-1, |d| {
            d.saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32
        });
        // SAFETY: fds describes valid owned descriptors throughout this call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout) };
        if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            anyhow::bail!("clipboard poll failed");
        }
        if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
            drop(guard);
            break;
        }
        if fds[0].revents & libc::POLLIN != 0 {
            guard.read()?;
        } else {
            drop(guard);
        }
        if fds[1].revents != 0 {
            let mut value = 0u64;
            // SAFETY: nonblocking eventfd and writable eight-byte value.
            unsafe {
                libc::read(wake.as_raw_fd(), (&mut value as *mut u64).cast(), 8);
            }
        }
    }
    // Drop pending transfer fds and relinquish only this connection's source.
    if let Some(source) = s.source {
        source.destroy();
    }
    if let Some(device) = s.device {
        device.destroy();
    }
    let _ = connection.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interrupted_attachment_cannot_revive_or_clear_a_replacement_session() {
        let clipboard = Clipboard::default();
        // Hold teardown open exactly where an old capture command used to
        // adopt a replacement session's generation after clear returned.
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let (done, completion) = std::sync::mpsc::sync_channel(1);
        *clipboard.worker.lock().unwrap() = Some(Worker {
            thread: std::thread::spawn(move || {
                wait.recv().unwrap();
                let _ = done.send(());
            }),
            done: completion,
        });
        let reserved = clipboard.clone();
        let (ticket_sender, ticket_receiver) = std::sync::mpsc::sync_channel(1);
        let reserver = std::thread::spawn(move || {
            ticket_sender.send(reserved.attachment()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while clipboard.shared.lock().unwrap().generation == 0 {
            assert!(Instant::now() < deadline, "attachment did not begin reset");
            std::thread::yield_now();
        }
        clipboard.clear();
        let current = clipboard.attachment();
        {
            let mut shared = clipboard.shared.lock().unwrap();
            shared.authenticated = true;
            shared.local = Some("replacement selection".into());
        }
        release.send(()).unwrap();
        let stale = ticket_receiver
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        reserver.join().unwrap();
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!stale.attach_connection(Connection::from_socket(socket).unwrap()));
        assert!(clipboard.worker.lock().unwrap().is_none());
        // A retired capture worker's normal cleanup must also be harmless.
        stale.clear();
        {
            let shared = clipboard.shared.lock().unwrap();
            assert_eq!(shared.generation, current.generation);
            assert!(shared.authenticated);
            assert_eq!(shared.local.as_deref(), Some("replacement selection"));
        }
        current.clear();
        assert!(!clipboard.shared.lock().unwrap().authenticated);
    }

    #[test]
    fn first_remote_copy_stays_pending_until_private_device_is_ready() {
        let clipboard = Clipboard::default();
        let generation = clipboard.attachment().generation;
        {
            let mut shared = clipboard.shared.lock().unwrap();
            shared.authenticated = true;
            shared.remote = Some(Some("first copy 日本😀".into()));
        }
        let state = WaylandState {
            shared: clipboard.shared.clone(),
            generation,
            manager: None,
            seat: None,
            device: None,
            offers: HashMap::new(),
            source: None,
            source_text: Arc::from([]),
            receive: None,
            sends: Vec::new(),
            serial: 0,
            synced: false,
            finished: false,
        };
        assert!(state.pending_remote().is_none());
        assert_eq!(
            clipboard.shared.lock().unwrap().remote,
            Some(Some("first copy 日本😀".into()))
        );
        clipboard.clear();
        assert!(state.pending_remote().is_none());
        assert!(clipboard.shared.lock().unwrap().remote.is_none());
    }

    #[test]
    fn unicode_text_roundtrip_rejects_malformed_and_oversize_data() {
        for text in ["", "a\r\nb", "日本😀"] {
            assert_eq!(
                decode_unicode(FormatDataResponse::new_unicode_string(text).data()).unwrap(),
                text.replace("\r\n", "\n")
            );
        }
        for bytes in [&[0][..], &[0, 216, 0, 0], &[97, 0], &[0, 0, 97, 0, 0, 0]] {
            assert!(decode_unicode(bytes).is_none());
        }
        assert!(decode_unicode(&vec![0; MAX_TEXT_BYTES * 2 + 4]).is_none());
    }
    #[test]
    fn no_clipboard_access_before_authentication_and_cleanup_erases_content() {
        let mut clipboard = Clipboard::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        let mut backend = clipboard.build_cliprdr_backend();
        backend.on_ready();
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        assert!(events.try_recv().is_err());
        {
            let mut s = clipboard.shared.lock().unwrap();
            s.authenticated = true;
            s.local = Some("private".into());
            s.dirty = true;
            s.advertise();
        }
        assert!(matches!(
            events.try_recv(),
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiateCopy(
                _
            )))
        ));
        clipboard.clear();
        let s = clipboard.shared.lock().unwrap();
        assert!(s.local.is_none() && s.remote.is_none() && !s.authenticated);
    }
    #[test]
    fn format_advertisements_coalesce_until_acknowledged() {
        let mut clipboard = Clipboard::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        let mut backend = clipboard.build_cliprdr_backend();
        {
            let mut s = clipboard.shared.lock().unwrap();
            s.authenticated = true;
            s.local = Some(String::new());
        }
        backend.on_ready();
        for _ in 0..10_000 {
            backend.on_request_format_list();
        }
        assert!(events.try_recv().is_ok());
        assert!(events.try_recv().is_err());
        backend.on_format_list_response(true);
        assert!(events.try_recv().is_ok());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn repeated_data_requests_keep_one_bounded_response_until_transport_drain() {
        let mut clipboard = Clipboard::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        let mut backend = clipboard.build_cliprdr_backend();
        {
            let mut s = clipboard.shared.lock().unwrap();
            s.authenticated = true;
            s.local = Some("x".repeat(MAX_TEXT_BYTES));
        }
        for _ in 0..10_000 {
            backend.on_format_data_request(FormatDataRequest {
                format: ClipboardFormatId::CF_UNICODETEXT,
            });
        }
        assert!(matches!(
            events.try_recv(),
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(_)))
        ));
        assert!(events.try_recv().is_err());
        clipboard.on_format_data_response_drained();
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        });
        assert!(events.try_recv().is_ok());
    }

    #[test]
    fn remote_timeout_disables_ambiguous_responses_until_reconnect() {
        let clipboard = Clipboard::default();
        clipboard.shared.lock().unwrap().authenticated = true;
        let mut backend = Backend {
            clipboard: clipboard.clone(),
            request: Some((1, Instant::now() - TIMEOUT - Duration::from_millis(1))),
            revision: 1,
            latest_copy: true,
            remote_disabled: false,
        };
        backend.on_remote_copy(&[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]);
        assert!(backend.remote_disabled && backend.request.is_none());
        backend.on_format_data_response(FormatDataResponse::new_unicode_string("old selection"));
        assert!(clipboard.shared.lock().unwrap().remote.is_none());
    }

    #[test]
    fn maximum_newline_payload_roundtrips_with_bounded_utf16_expansion() {
        let text = "\n".repeat(MAX_TEXT_BYTES);
        let response = FormatDataResponse::new_unicode_string(&text.replace('\n', "\r\n"));
        assert_eq!(response.data().len(), MAX_TEXT_BYTES * 4 + 2);
        assert_eq!(decode_unicode(response.data()).unwrap(), text);
    }

    #[test]
    fn retired_wayland_worker_cannot_restore_prior_session_content() {
        let clipboard = Clipboard::default();
        {
            clipboard.shared.lock().unwrap().authenticated = true;
        }
        let state = WaylandState {
            shared: clipboard.shared.clone(),
            generation: 0,
            manager: None,
            seat: None,
            device: None,
            offers: HashMap::new(),
            source: None,
            source_text: Arc::from([]),
            receive: None,
            sends: Vec::new(),
            serial: 0,
            synced: false,
            finished: false,
        };
        clipboard.clear();
        {
            clipboard.shared.lock().unwrap().authenticated = true;
        }
        state.local_text(Some("prior session".into()));
        assert!(clipboard.shared.lock().unwrap().local.is_none());
    }
}

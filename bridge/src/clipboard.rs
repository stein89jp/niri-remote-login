//! CLIPRDR text, HTML and images on the authenticated compositor's existing
//! connection. No process environment, local display socket, bus or
//! clipboard content is logged.
mod html;
mod image;

use crate::performance::{Counter, connection as stats};
use ironrdp_cliprdr::{
    backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory},
    pdu::*,
};
use ironrdp_core::{IntoOwned, impl_as_any};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use std::{
    collections::{HashMap, VecDeque},
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
/// CLIPRDR responses carry no request ID, so at most one paste request is
/// outstanding and every response belongs to it. A slow transfer therefore
/// stays attributable however long it takes (four MiB of UTF-16 needs about
/// 32 s at 1 Mbit/s). A request is presumed lost only when a newer copy needs
/// the channel after this cap; the peer announces no size to scale it by.
const REMOTE_REQUEST_CAP: Duration = Duration::from_secs(60);
/// Format Data Requests are answered in order. Text goes out only while no
/// response is queued; overlapping requests get small error responses. A peer
/// that keeps requesting without reading cannot grow the queue past this.
const MAX_QUEUED_RESPONSES: usize = 16;
const MIME: &str = "text/plain;charset=utf-8";
const HTML_MIME: &str = "text/html";
const PNG_MIME: &str = "image/png";
const OWN_MIME: &str = "application/x-niri-rdp-private-text";
const PNG_NAME: ClipboardFormatName = ClipboardFormatName::new_static("PNG");
/// Registered IDs this server announces; the client maps them by name.
const LOCAL_HTML_ID: ClipboardFormatId = ClipboardFormatId(0xc0a1);
const LOCAL_PNG_ID: ClipboardFormatId = ClipboardFormatId(0xc0a2);

/// The clipboard representations this bridge exchanges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Html,
    Png,
    Dib,
}

/// One remote (Windows) copy as fetched, published to Wayland by the worker.
#[derive(Clone, Default, PartialEq)]
struct Remote {
    text: Option<String>,
    /// UTF-8 `text/html`, already extracted from CF_HTML.
    html: Option<String>,
    /// PNG passed through, or a DIB the worker converts off the RDP task.
    image: Option<(Kind, Arc<[u8]>)>,
}
impl Remote {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.image.is_none()
    }
}
impl From<&str> for Remote {
    fn from(text: &str) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}
/// Content is never formatted: only which representations are present.
impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Remote")
            .field("text", &self.text.as_ref().map(String::len))
            .field("html", &self.html.as_ref().map(String::len))
            .field("image", &self.image.as_ref().map(|(k, b)| (k, b.len())))
            .finish()
    }
}

/// The native (Wayland) selection in every wire form the peer may request.
#[derive(Clone, Default, PartialEq)]
struct Local {
    text: Option<String>,
    /// Complete CF_HTML bytes.
    html: Option<Arc<[u8]>>,
    png: Option<Arc<[u8]>>,
    dib: Option<Arc<[u8]>>,
}
impl From<&str> for Local {
    fn from(text: &str) -> Self {
        Self {
            text: Some(text.into()),
            ..Self::default()
        }
    }
}
impl std::fmt::Debug for Local {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let length = |b: &Option<Arc<[u8]>>| b.as_ref().map(|b| b.len());
        f.debug_struct("Local")
            .field("text", &self.text.as_ref().map(String::len))
            .field("html", &length(&self.html))
            .field("png", &length(&self.png))
            .field("dib", &length(&self.dib))
            .finish()
    }
}
impl Local {
    fn is_empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.png.is_none() && self.dib.is_none()
    }
    /// The Format List announcing exactly the representations present.
    fn formats(&self) -> Vec<ClipboardFormat> {
        let mut formats = Vec::new();
        if self.text.is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if self.html.is_some() {
            formats.push(ClipboardFormat::new(LOCAL_HTML_ID).with_name(ClipboardFormatName::HTML));
        }
        if self.dib.is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_DIB));
        }
        if self.png.is_some() {
            formats.push(ClipboardFormat::new(LOCAL_PNG_ID).with_name(PNG_NAME));
        }
        formats
    }
    fn response(&self, format: ClipboardFormatId) -> Option<FormatDataResponse<'static>> {
        let bytes =
            |b: &Option<Arc<[u8]>>| b.as_ref().map(|b| FormatDataResponse::new_data(b.to_vec()));
        match format {
            ClipboardFormatId::CF_UNICODETEXT => self.text.as_ref().map(|text| {
                FormatDataResponse::new_unicode_string(
                    &text.replace("\r\n", "\n").replace('\n', "\r\n"),
                )
                .into_owned()
            }),
            LOCAL_HTML_ID => bytes(&self.html),
            LOCAL_PNG_ID => bytes(&self.png),
            ClipboardFormatId::CF_DIB => bytes(&self.dib),
            _ => None,
        }
    }
}

/// Which remote formats to fetch, in order. Text and HTML are small and
/// always wanted. An image is fetched only when the copy has no text, so
/// copying cells or rich text (which Office also renders as a bitmap) does
/// not move megabytes nobody asked for. PNG is preferred because it is
/// compact and keeps alpha; formats are matched by name only when the ID is
/// a registered one.
fn remote_plan(formats: &[ClipboardFormat]) -> VecDeque<(Kind, ClipboardFormatId)> {
    let named = |name: &ClipboardFormatName| {
        formats
            .iter()
            .find(|f| f.id().is_registered() && f.name().is_some_and(|n| n.value() == name.value()))
            .map(ClipboardFormat::id)
    };
    let has = |id| formats.iter().any(|f| f.id() == id);
    let mut plan = VecDeque::new();
    let text = has(ClipboardFormatId::CF_UNICODETEXT);
    if text {
        plan.push_back((Kind::Text, ClipboardFormatId::CF_UNICODETEXT));
    }
    if let Some(id) = named(&ClipboardFormatName::HTML) {
        plan.push_back((Kind::Html, id));
    }
    if !text {
        if let Some(id) = named(&PNG_NAME) {
            plan.push_back((Kind::Png, id));
        } else if has(ClipboardFormatId::CF_DIBV5) {
            plan.push_back((Kind::Dib, ClipboardFormatId::CF_DIBV5));
        } else if has(ClipboardFormatId::CF_DIB) {
            plan.push_back((Kind::Dib, ClipboardFormatId::CF_DIB));
        }
    }
    plan
}

#[derive(Default)]
struct Shared {
    sender: Option<tokio::sync::mpsc::UnboundedSender<ServerEvent>>,
    ready: bool,
    authenticated: bool,
    generation: u64,
    wake: Option<Arc<OwnedFd>>,
    remote: Option<Remote>,
    local: Option<Local>,
    dirty: bool,
    advertised: bool,
    /// Format Data Responses queued for the transport and not yet drained.
    queued_responses: usize,
}
impl Shared {
    fn send(&self, message: ClipboardMessage) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|sender| sender.send(ServerEvent::Clipboard(message)).is_ok())
    }
    fn advertise(&mut self) {
        if self.ready && self.authenticated && self.dirty && !self.advertised {
            self.dirty = false;
            self.advertised = true;
            let formats = self.local.as_ref().map(Local::formats).unwrap_or_default();
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
        shared.queued_responses = 0;
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
        let mut shared = self.shared.lock().unwrap();
        shared.queued_responses = shared.queued_responses.saturating_sub(1);
    }
}
impl CliprdrBackendFactory for Clipboard {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        self.shared.lock().unwrap().ready = false;
        Box::new(Backend::new(self.clone()))
    }
}
/// The one outstanding paste request, the remote copy it asked for, and
/// which representation it fetches.
#[derive(Clone, Copy, Debug)]
struct RemoteRequest {
    revision: u64,
    started: Instant,
    kind: Kind,
}
struct Backend {
    clipboard: Clipboard,
    request: Option<RemoteRequest>,
    revision: u64,
    /// Formats of the newest remote copy not yet requested, in order.
    pending: VecDeque<(Kind, ClipboardFormatId)>,
    /// What the newest remote copy has delivered so far.
    fetched: Remote,
}
impl_as_any!(Backend);
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PrivateTextBackend")
    }
}
impl Backend {
    fn new(clipboard: Clipboard) -> Self {
        Self {
            clipboard,
            request: None,
            revision: 0,
            pending: VecDeque::new(),
            fetched: Remote::default(),
        }
    }
    fn request_next(&mut self, now: Instant) {
        if let Some(request) = self.request {
            if now.saturating_duration_since(request.started) < REMOTE_REQUEST_CAP {
                // Wait: its response, however late, is used if it still
                // answers the newest copy, else discarded and the newest asked.
                return;
            }
            // Never two outstanding requests: give up on an unanswered one
            // only when a newer copy needs the channel.
            self.request = None;
            stats().add(Counter::ClipboardTimeouts, 1);
            eprintln!("remote clipboard request went unanswered; requesting the newest copy");
        }
        let Some(&(kind, format)) = self.pending.front() else {
            return;
        };
        let shared = self.clipboard.shared.lock().unwrap();
        if shared.authenticated && shared.send(ClipboardMessage::SendInitiatePaste(format)) {
            self.pending.pop_front();
            self.request = Some(RemoteRequest {
                revision: self.revision,
                started: now,
                kind,
            });
        }
    }
    fn remote_copy_at(&mut self, formats: &[ClipboardFormat], now: Instant) {
        self.revision = self.revision.wrapping_add(1);
        // A copy with nothing usable (files, or an emptied Windows
        // clipboard) leaves the native selection as it was.
        self.pending = remote_plan(formats);
        self.fetched = Remote::default();
        self.request_next(now);
    }
    fn format_data_response_at(&mut self, response: FormatDataResponse<'_>, now: Instant) {
        // Only one request is ever outstanding, so this answers it.
        let Some(request) = self.request.take() else {
            return;
        };
        if request.revision != self.revision {
            // Superseded while in flight: discard it and ask for the newest.
            self.request_next(now);
            return;
        }
        if !response.is_error() {
            let data = response.data();
            match request.kind {
                Kind::Text => self.fetched.text = decode_unicode(data),
                Kind::Html => self.fetched.html = html::from_cf_html(data),
                Kind::Png if image::png_dimensions(data).is_some() => {
                    self.fetched.image = Some((Kind::Png, data.into()))
                }
                Kind::Dib if data.len() <= image::MAX_DIB_BYTES => {
                    self.fetched.image = Some((Kind::Dib, data.into()))
                }
                Kind::Png | Kind::Dib => {}
            }
            let accepted = match request.kind {
                Kind::Text => self.fetched.text.is_some(),
                Kind::Html => self.fetched.html.is_some(),
                Kind::Png | Kind::Dib => self.fetched.image.is_some(),
            };
            if accepted {
                stats().add(Counter::ClipboardFromClient, 1);
            } else if data.len() > request.kind.wire_limit() {
                stats().add(Counter::ClipboardOversize, 1);
            }
        }
        if !self.pending.is_empty() {
            self.request_next(now);
            return;
        }
        // Publish once per copy, so native clipboard managers see one change.
        let fetched = std::mem::take(&mut self.fetched);
        if !fetched.is_empty() {
            let mut s = self.clipboard.shared.lock().unwrap();
            if s.authenticated {
                s.remote = Some(fetched);
                s.wake();
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
        self.remote_copy_at(formats, Instant::now());
    }
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let mut s = self.clipboard.shared.lock().unwrap();
        // MS-RDPECLIP expects an answer to every request. Only an idle
        // transport receives data, which keeps one bounded copy in flight.
        if s.queued_responses >= MAX_QUEUED_RESPONSES {
            return;
        }
        let response = if s.queued_responses == 0 && s.authenticated {
            s.local
                .as_ref()
                .and_then(|local| local.response(request.format))
        } else {
            None
        }
        .unwrap_or_else(|| FormatDataResponse::new_error().into_owned());
        let transfer = !response.is_error();
        if s.send(ClipboardMessage::SendFormatData(response)) {
            s.queued_responses += 1;
            if transfer {
                stats().add(Counter::ClipboardToClient, 1);
            }
        }
    }
    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        self.format_data_response_at(response, Instant::now());
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
const TEXT_MIMES: [&str; 4] = [
    MIME,
    "text/plain;charset=UTF-8",
    "UTF8_STRING",
    "text/plain",
];
fn text_mime(types: &[String]) -> Option<&'static str> {
    TEXT_MIMES
        .into_iter()
        .find(|wanted| types.iter().any(|offered| offered == wanted))
}
/// Which native types to read for the peer, mirroring remote_plan: an
/// image only when the selection has no text.
fn local_plan(types: &[String]) -> Vec<(Kind, &'static str)> {
    let offered = |mime: &str| types.iter().any(|t| t == mime);
    let mut plan = Vec::new();
    let text = text_mime(types);
    if let Some(mime) = text {
        plan.push((Kind::Text, mime));
    }
    if offered(HTML_MIME) {
        plan.push((Kind::Html, HTML_MIME));
    }
    if text.is_none() && offered(PNG_MIME) {
        plan.push((Kind::Png, PNG_MIME));
    }
    plan
}
impl Kind {
    fn limit(self) -> usize {
        match self {
            Kind::Text => MAX_TEXT_BYTES,
            Kind::Html => html::MAX_HTML_BYTES,
            Kind::Png => image::MAX_IMAGE_BYTES,
            Kind::Dib => image::MAX_DIB_BYTES,
        }
    }
    /// Largest peer response that can still decode within [`Kind::limit`].
    fn wire_limit(self) -> usize {
        match self {
            // LF -> CRLF plus UTF-16 and the terminator, as in `decode_unicode`.
            Kind::Text => MAX_TEXT_BYTES * 4 + 2,
            Kind::Html => html::MAX_HTML_BYTES + html::MAX_HEADER_BYTES,
            Kind::Png => image::MAX_IMAGE_BYTES,
            Kind::Dib => image::MAX_DIB_BYTES,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Kind::Text => "text",
            Kind::Html => "HTML",
            Kind::Png => "PNG",
            Kind::Dib => "DIB",
        }
    }
}
struct Receive {
    kind: Kind,
    fd: std::fs::File,
    data: Vec<u8>,
    deadline: Instant,
    /// Some(true) once complete, Some(false) once failed or over its bound.
    finished: Option<bool>,
}
impl Receive {
    fn pump(&mut self) {
        if Instant::now() >= self.deadline {
            self.finished = Some(false);
            stats().add(Counter::ClipboardTimeouts, 1);
            return;
        }
        let mut buffer = [0u8; 65536];
        loop {
            match self.fd.read(&mut buffer) {
                Ok(0) => {
                    self.finished = Some(true);
                    return;
                }
                Ok(n) if self.data.len() + n <= self.kind.limit() => {
                    self.data.extend_from_slice(&buffer[..n])
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(_) => {
                    // Size only: the content itself is never logged.
                    eprintln!(
                        "local clipboard {} exceeded {} bytes and was not offered to the client",
                        self.kind.name(),
                        self.kind.limit()
                    );
                    stats().add(Counter::ClipboardOversize, 1);
                    self.finished = Some(false);
                    return;
                }
                Err(_) => {
                    self.finished = Some(false);
                    return;
                }
            }
        }
    }
}
/// Converts finished native reads into every wire form. A failed or
/// unusable representation is simply absent.
fn local_from_reads(receives: Vec<Receive>) -> Option<Local> {
    let mut local = Local::default();
    for receive in receives {
        if receive.finished != Some(true) {
            continue;
        }
        match receive.kind {
            Kind::Text => {
                local.text = String::from_utf8(receive.data)
                    .ok()
                    .filter(|text| !text.contains('\0'))
            }
            Kind::Html => {
                local.html = html::decode(&receive.data).map(|html| html::to_cf_html(&html).into())
            }
            Kind::Png => {
                if let Some(dib) = image::png_to_dib(&receive.data) {
                    local.dib = Some(dib.into());
                    local.png = Some(receive.data.into());
                }
            }
            Kind::Dib => {}
        }
    }
    (!local.is_empty()).then_some(local)
}
/// What this bridge's own data source serves, by MIME type.
#[derive(Default)]
struct SourceData {
    text: Option<Arc<[u8]>>,
    html: Option<Arc<[u8]>>,
    png: Option<Arc<[u8]>>,
}
impl SourceData {
    fn get(&self, mime: &str) -> Option<Arc<[u8]>> {
        if TEXT_MIMES.contains(&mime) {
            self.text.clone()
        } else if mime == HTML_MIME {
            self.html.clone()
        } else if mime == PNG_MIME {
            self.png.clone()
        } else {
            None
        }
    }
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
    source_data: SourceData,
    receives: Vec<Receive>,
    sends: Vec<SendText>,
    synced: bool,
    finished: bool,
}
impl WaylandState {
    fn new(shared: Arc<Mutex<Shared>>, generation: u64) -> Self {
        Self {
            shared,
            generation,
            manager: None,
            seat: None,
            device: None,
            offers: HashMap::new(),
            source: None,
            source_data: SourceData::default(),
            receives: Vec::new(),
            sends: Vec::with_capacity(4),
            synced: false,
            finished: false,
        }
    }
    fn pending_remote(&self) -> Option<Remote> {
        // CLIPRDR may deliver the first copy while the private registry sync
        // is still running. Keep it queued until set_selection can succeed.
        self.device.as_ref()?;
        let mut shared = self.shared.lock().unwrap();
        if shared.generation != self.generation || !shared.authenticated {
            return None;
        }
        shared.remote.take()
    }
    fn local_content(&self, local: Option<Local>) {
        let mut s = self.shared.lock().unwrap();
        if s.generation != self.generation || !s.authenticated {
            return;
        }
        s.local = local;
        s.dirty = true;
        s.advertise();
    }
    fn remote_content(&mut self, remote: Remote, qh: &QueueHandle<Self>) {
        // Image conversion runs here, never on the RDP task.
        let png: Option<Arc<[u8]>> = match &remote.image {
            Some((Kind::Dib, dib)) => image::dib_to_png(dib).map(Arc::from),
            Some((_, png)) => Some(png.clone()),
            None => None,
        };
        let data = SourceData {
            text: remote.text.as_deref().map(|t| Arc::from(t.as_bytes())),
            html: remote.html.as_deref().map(|h| Arc::from(h.as_bytes())),
            png: png.clone(),
        };
        if data.text.is_none() && data.html.is_none() && data.png.is_none() {
            // Nothing usable: the native selection stays as it was.
            return;
        }
        self.receives.clear();
        // Replace the previous native owner's cached content immediately. Do
        // not advertise it back to the peer that supplied this new selection.
        {
            let mut shared = self.shared.lock().unwrap();
            if shared.generation != self.generation || !shared.authenticated {
                return;
            }
            shared.local = Some(Local {
                html: remote.html.as_deref().map(|h| html::to_cf_html(h).into()),
                dib: match &remote.image {
                    Some((Kind::Dib, dib)) => Some(dib.clone()),
                    _ => None,
                },
                text: remote.text,
                png,
            });
            shared.dirty = false;
        }
        if let Some(source) = self.source.take() {
            source.destroy();
        }
        let (Some(manager), Some(device)) = (&self.manager, &self.device) else {
            return;
        };
        let source = manager.create_data_source(qh, ());
        if data.text.is_some() {
            for mime in [MIME, "text/plain", "UTF8_STRING"] {
                source.offer(mime.into());
            }
        }
        if data.html.is_some() {
            source.offer(HTML_MIME.into());
        }
        if data.png.is_some() {
            source.offer(PNG_MIME.into());
        }
        source.offer(OWN_MIME.into());
        device.set_selection(Some(&source));
        self.source = Some(source);
        self.source_data = data;
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
                s.receives.clear();
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
                        // Every wanted type is requested now, from this one
                        // offer, and read concurrently.
                        for (kind, mime) in local_plan(&types) {
                            let Ok((reader, writer)) = std::io::pipe() else {
                                continue;
                            };
                            // SAFETY: fcntl operates on our fresh pipe descriptor.
                            if unsafe {
                                libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK)
                            } < 0
                            {
                                continue;
                            }
                            offer.receive(mime.into(), writer.as_fd());
                            s.receives.push(Receive {
                                kind,
                                fd: OwnedFd::from(reader).into(),
                                data: Vec::new(),
                                deadline: Instant::now() + TIMEOUT,
                                finished: None,
                            });
                        }
                        if s.receives.is_empty() {
                            s.local_content(None);
                        }
                    }
                    offer.destroy();
                } else {
                    s.local_content(None);
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
                    && s.sends.len() < 4
                    && let Some(data) = s.source_data.get(&mime_type)
                {
                    // SAFETY: we own fd and bounded writes need nonblocking mode.
                    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                        return;
                    }
                    s.sends.push(SendText {
                        fd: fd.into(),
                        text: data,
                        offset: 0,
                        deadline: Instant::now() + TIMEOUT,
                    });
                }
            }
            zwlr_data_control_source_v1::Event::Cancelled => {
                if s.source.as_ref() == Some(source) {
                    s.source.take();
                    s.source_data = SourceData::default();
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
    let mut s = WaylandState::new(shared, generation);
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
        if let Some(remote) = s.pending_remote() {
            s.remote_content(remote, &qh);
        }
        if s.device.is_none() && Instant::now() >= startup {
            anyhow::bail!("clipboard startup timeout");
        }
        if !s.receives.is_empty() {
            for transfer in s.receives.iter_mut().filter(|r| r.finished.is_none()) {
                transfer.pump();
            }
            if s.receives.iter().all(|r| r.finished.is_some()) {
                let local = local_from_reads(std::mem::take(&mut s.receives));
                s.local_content(local);
            }
        }
        s.sends.retain_mut(|transfer| {
            if Instant::now() >= transfer.deadline {
                stats().add(Counter::ClipboardTimeouts, 1);
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
        for transfer in s.receives.iter().filter(|r| r.finished.is_none()) {
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
            assert_eq!(shared.local, Some("replacement selection".into()));
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
            shared.remote = Some("first copy 日本😀".into());
        }
        let state = WaylandState::new(clipboard.shared.clone(), generation);
        assert!(state.pending_remote().is_none());
        assert_eq!(
            clipboard.shared.lock().unwrap().remote,
            Some("first copy 日本😀".into())
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
            s.local = Some("".into());
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

    /// Some(true) for a text response, Some(false) for an error response.
    fn next_format_data(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) -> Option<bool> {
        match events.try_recv() {
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response))) => {
                Some(!response.is_error())
            }
            Ok(_) => panic!("unexpected clipboard event"),
            Err(_) => None,
        }
    }

    fn text_request() -> FormatDataRequest {
        FormatDataRequest {
            format: ClipboardFormatId::CF_UNICODETEXT,
        }
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
            s.local = Some("x".repeat(MAX_TEXT_BYTES).as_str().into());
        }
        for _ in 0..10_000 {
            backend.on_format_data_request(text_request());
        }
        // One text copy, then small error answers up to the queue bound.
        assert_eq!(next_format_data(&mut events), Some(true));
        for _ in 1..MAX_QUEUED_RESPONSES {
            assert_eq!(next_format_data(&mut events), Some(false));
        }
        assert_eq!(next_format_data(&mut events), None);
        for _ in 0..MAX_QUEUED_RESPONSES {
            clipboard.on_format_data_response_drained();
        }
        backend.on_format_data_request(text_request());
        assert_eq!(next_format_data(&mut events), Some(true));
    }

    #[test]
    fn overlapping_data_request_receives_an_error_response() {
        let mut clipboard = Clipboard::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        let mut backend = clipboard.build_cliprdr_backend();
        {
            let mut s = clipboard.shared.lock().unwrap();
            s.authenticated = true;
            s.local = Some("local".into());
        }
        backend.on_format_data_request(text_request());
        assert_eq!(next_format_data(&mut events), Some(true));
        backend.on_format_data_request(text_request());
        assert_eq!(next_format_data(&mut events), Some(false));
        // Drains arrive in order; text resumes only once both have left.
        clipboard.on_format_data_response_drained();
        backend.on_format_data_request(text_request());
        assert_eq!(next_format_data(&mut events), Some(false));
        clipboard.on_format_data_response_drained();
        clipboard.on_format_data_response_drained();
        backend.on_format_data_request(text_request());
        assert_eq!(next_format_data(&mut events), Some(true));
        clipboard.on_format_data_response_drained();
        // An undeliverable answer is never counted as queued.
        drop(events);
        backend.on_format_data_request(text_request());
        assert_eq!(clipboard.shared.lock().unwrap().queued_responses, 0);
    }

    fn remote_backend() -> (
        Clipboard,
        Backend,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let mut clipboard = Clipboard::default();
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        clipboard.shared.lock().unwrap().authenticated = true;
        let backend = Backend::new(clipboard.clone());
        (clipboard, backend, events)
    }

    fn paste_requests(events: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>) -> usize {
        let mut count = 0;
        while let Ok(event) = events.try_recv() {
            assert!(matches!(
                event,
                ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(
                    ClipboardFormatId::CF_UNICODETEXT
                ))
            ));
            count += 1;
        }
        count
    }

    fn take_remote(clipboard: &Clipboard) -> Option<Remote> {
        clipboard.shared.lock().unwrap().remote.take()
    }

    const TEXT: &[ClipboardFormat] = &[ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)];

    #[test]
    fn slow_remote_response_is_published_and_remote_copy_stays_enabled() {
        let (clipboard, mut backend, mut events) = remote_backend();
        let start = Instant::now();
        backend.remote_copy_at(TEXT, start);
        assert_eq!(paste_requests(&mut events), 1);
        // A multi-megabyte paste over a slow link, far beyond the old 5 s.
        let late = start + Duration::from_secs(45);
        backend.format_data_response_at(FormatDataResponse::new_unicode_string("slow"), late);
        assert_eq!(take_remote(&clipboard), Some("slow".into()));
        backend.remote_copy_at(TEXT, late + Duration::from_secs(1));
        assert_eq!(paste_requests(&mut events), 1);
        backend.format_data_response_at(
            FormatDataResponse::new_unicode_string("next"),
            late + Duration::from_secs(2),
        );
        assert_eq!(take_remote(&clipboard), Some("next".into()));
    }

    #[test]
    fn superseded_late_response_is_discarded_and_newest_copy_requested_once() {
        let (clipboard, mut backend, mut events) = remote_backend();
        let start = Instant::now();
        backend.remote_copy_at(TEXT, start);
        assert_eq!(paste_requests(&mut events), 1);
        // Newer copies inside the cap never open a second request.
        for second in 1..50 {
            backend.remote_copy_at(TEXT, start + Duration::from_secs(second));
        }
        assert_eq!(paste_requests(&mut events), 0);
        let late = start + Duration::from_secs(55);
        backend.format_data_response_at(FormatDataResponse::new_unicode_string("old"), late);
        assert_eq!(take_remote(&clipboard), None);
        assert_eq!(paste_requests(&mut events), 1);
        backend.format_data_response_at(
            FormatDataResponse::new_unicode_string("newest"),
            late + Duration::from_secs(1),
        );
        assert_eq!(take_remote(&clipboard), Some("newest".into()));
        // An unsolicited response is ignored.
        backend.format_data_response_at(
            FormatDataResponse::new_unicode_string("unsolicited"),
            late + Duration::from_secs(2),
        );
        assert_eq!(take_remote(&clipboard), None);
        assert_eq!(paste_requests(&mut events), 0);
    }

    #[test]
    fn unanswered_request_is_replaced_on_next_copy_after_cap() {
        let (clipboard, mut backend, mut events) = remote_backend();
        let start = Instant::now();
        backend.remote_copy_at(TEXT, start);
        assert_eq!(paste_requests(&mut events), 1);
        backend.remote_copy_at(TEXT, start + REMOTE_REQUEST_CAP - Duration::from_millis(1));
        assert_eq!(paste_requests(&mut events), 0);
        let after = start + REMOTE_REQUEST_CAP;
        backend.remote_copy_at(TEXT, after);
        assert_eq!(paste_requests(&mut events), 1);
        backend.format_data_response_at(
            FormatDataResponse::new_unicode_string("restored"),
            after + Duration::from_secs(1),
        );
        assert_eq!(take_remote(&clipboard), Some("restored".into()));
        // A non-text copy after the cap also releases the lost request, and
        // leaves the native selection alone.
        backend.remote_copy_at(TEXT, after + Duration::from_secs(2));
        assert_eq!(paste_requests(&mut events), 1);
        backend.remote_copy_at(&[], after + Duration::from_secs(2) + REMOTE_REQUEST_CAP);
        assert!(backend.request.is_none());
        assert_eq!(take_remote(&clipboard), None);
        assert_eq!(paste_requests(&mut events), 0);
    }

    /// The summary counters are process-wide and other tests run in
    /// parallel, so these assert growth by at least the expected amount.
    fn counted(counter: Counter, action: impl FnOnce()) -> u64 {
        let before = stats().get(counter);
        action();
        stats().get(counter) - before
    }

    #[test]
    fn summary_counts_transfers_each_way_timeouts_and_oversize_drops() {
        let (clipboard, mut backend, mut events) = remote_backend();
        let start = Instant::now();
        let from_client = counted(Counter::ClipboardFromClient, || {
            backend.remote_copy_at(TEXT, start);
            backend.format_data_response_at(FormatDataResponse::new_unicode_string("a"), start);
        });
        assert!(from_client >= 1);
        assert_eq!(take_remote(&clipboard), Some("a".into()));
        paste_requests(&mut events);
        // An oversized peer response is dropped and counted, never published.
        let oversize = counted(Counter::ClipboardOversize, || {
            backend.remote_copy_at(TEXT, start);
            let mut data = [b'a', 0].repeat(MAX_TEXT_BYTES * 2 + 2);
            data.extend([0, 0]);
            backend.format_data_response_at(FormatDataResponse::new_data(data), start);
        });
        assert!(oversize >= 1);
        assert_eq!(take_remote(&clipboard), None);
        paste_requests(&mut events);
        let timeouts = counted(Counter::ClipboardTimeouts, || {
            backend.remote_copy_at(TEXT, start);
            backend.remote_copy_at(TEXT, start + REMOTE_REQUEST_CAP);
        });
        assert!(timeouts >= 1);
        let to_client = counted(Counter::ClipboardToClient, || {
            clipboard.shared.lock().unwrap().local = Some("b".into());
            backend.on_format_data_request(text_request());
        });
        assert!(to_client >= 1);
    }

    #[test]
    fn local_text_over_the_bound_is_dropped_counted_and_not_offered() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        // SAFETY: fcntl operates on our fresh pipe descriptor.
        assert!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
        let mut receive = Receive {
            kind: Kind::Text,
            fd: OwnedFd::from(reader).into(),
            data: Vec::new(),
            deadline: Instant::now() + TIMEOUT,
            finished: None,
        };
        let payload = vec![b'x'; MAX_TEXT_BYTES + 1];
        let oversize = counted(Counter::ClipboardOversize, || {
            let mut written = 0;
            while receive.finished.is_none() {
                if written < payload.len() {
                    // A pipe holds only part; pump drains it between writes.
                    let chunk = &payload[written..(written + 65536).min(payload.len())];
                    writer.write_all(chunk).unwrap();
                    written += chunk.len();
                }
                receive.pump();
            }
        });
        assert!(oversize >= 1);
        assert_eq!(receive.finished, Some(false));
        assert!(receive.data.len() <= MAX_TEXT_BYTES);
        assert_eq!(local_from_reads(vec![receive]), None);
        // Exactly the bound is still accepted.
        let (reader, mut writer) = std::io::pipe().unwrap();
        let mut receive = Receive {
            kind: Kind::Text,
            fd: OwnedFd::from(reader).into(),
            data: Vec::new(),
            deadline: Instant::now() + TIMEOUT,
            finished: None,
        };
        let payload = vec![b'x'; MAX_TEXT_BYTES];
        std::thread::scope(|scope| {
            scope.spawn(move || writer.write_all(&payload).unwrap());
            while receive.finished.is_none() {
                receive.pump();
            }
        });
        assert_eq!(receive.finished, Some(true));
        assert!(local_from_reads(vec![receive]).is_some());
    }

    #[test]
    fn expired_local_read_counts_a_timeout() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let mut receive = Receive {
            kind: Kind::Html,
            fd: OwnedFd::from(reader).into(),
            data: Vec::new(),
            deadline: Instant::now(),
            finished: None,
        };
        assert!(counted(Counter::ClipboardTimeouts, || receive.pump()) >= 1);
        assert_eq!(receive.finished, Some(false));
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
        let state = WaylandState::new(clipboard.shared.clone(), 0);
        clipboard.clear();
        {
            clipboard.shared.lock().unwrap().authenticated = true;
        }
        state.local_content(Some("prior session".into()));
        assert!(clipboard.shared.lock().unwrap().local.is_none());
    }

    /// Encodes a Format List as a peer would send it and decodes it as the
    /// server channel does, with long format names negotiated.
    fn wire_formats(formats: &[ClipboardFormat]) -> Vec<ClipboardFormat> {
        let pdu = ClipboardPdu::FormatList(FormatList::new_unicode(formats, true).unwrap());
        let bytes = ironrdp_core::encode_vec(&pdu).unwrap();
        let ClipboardPdu::FormatList(list) =
            ironrdp_core::decode::<ClipboardPdu<'_>>(&bytes).unwrap()
        else {
            panic!("wrong clipboard PDU");
        };
        list.get_formats(true).unwrap()
    }

    fn named(id: u32, name: &'static str) -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId(id)).with_name(ClipboardFormatName::new_static(name))
    }

    fn plan(formats: &[ClipboardFormat]) -> Vec<(Kind, ClipboardFormatId)> {
        remote_plan(&wire_formats(formats)).into_iter().collect()
    }

    #[test]
    fn remote_format_lists_select_text_html_and_one_image() {
        let text = ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT);
        let dib = ClipboardFormat::new(ClipboardFormatId::CF_DIB);
        let dibv5 = ClipboardFormat::new(ClipboardFormatId::CF_DIBV5);
        // A browser text selection: text and HTML, IDs as the client chose.
        assert_eq!(
            plan(&[
                named(0xc0f3, "HTML Format"),
                text.clone(),
                named(0xc0f9, "Rich Text Format")
            ]),
            [
                (Kind::Text, ClipboardFormatId::CF_UNICODETEXT),
                (Kind::Html, ClipboardFormatId(0xc0f3))
            ]
        );
        // Office cells also render a bitmap: not fetched beside text.
        assert_eq!(
            plan(&[text.clone(), dib.clone(), named(0xc10a, "PNG")]),
            [(Kind::Text, ClipboardFormatId::CF_UNICODETEXT)]
        );
        // A browser image copy: HTML plus PNG, PNG preferred to DIBs.
        assert_eq!(
            plan(&[
                dib.clone(),
                dibv5.clone(),
                named(0xc10a, "PNG"),
                named(0xc0f3, "HTML Format")
            ]),
            [
                (Kind::Html, ClipboardFormatId(0xc0f3)),
                (Kind::Png, ClipboardFormatId(0xc10a))
            ]
        );
        // Print Screen: DIBV5 preferred to DIB.
        assert_eq!(
            plan(&[
                ClipboardFormat::new(ClipboardFormatId::CF_BITMAP),
                dib.clone(),
                dibv5
            ]),
            [(Kind::Dib, ClipboardFormatId::CF_DIBV5)]
        );
        assert_eq!(plan(&[dib]), [(Kind::Dib, ClipboardFormatId::CF_DIB)]);
        // Files and empty lists have nothing usable; a standard ID claiming
        // a registered name is not trusted.
        assert!(
            plan(&[
                named(0xc0a0, "FileGroupDescriptorW"),
                named(0xc0a5, "Preferred DropEffect")
            ])
            .is_empty()
        );
        assert!(plan(&[]).is_empty());
        assert!(remote_plan(&[named(2, "PNG")]).is_empty());
    }

    #[test]
    fn advertised_local_formats_survive_the_wire_and_answer_requests() {
        let local = Local {
            text: Some("a\nb".into()),
            html: Some(html::to_cf_html("<b>x</b>").into()),
            png: Some(Arc::from(&b"png"[..])),
            dib: Some(Arc::from(&b"dib"[..])),
        };
        let formats = wire_formats(&local.formats());
        assert_eq!(formats, local.formats());
        let names: Vec<_> = formats
            .iter()
            .map(|f| (f.id(), f.name().map(|n| n.value().to_owned())))
            .collect();
        assert_eq!(
            names,
            [
                (ClipboardFormatId::CF_UNICODETEXT, None),
                (LOCAL_HTML_ID, Some("HTML Format".into())),
                (ClipboardFormatId::CF_DIB, None),
                (LOCAL_PNG_ID, Some("PNG".into())),
            ]
        );
        let data = |id| local.response(id).map(|r| r.data().to_vec());
        assert_eq!(
            data(ClipboardFormatId::CF_UNICODETEXT),
            Some(
                FormatDataResponse::new_unicode_string("a\r\nb")
                    .data()
                    .to_vec()
            )
        );
        assert_eq!(data(LOCAL_HTML_ID).as_deref(), local.html.as_deref());
        assert_eq!(data(LOCAL_PNG_ID).as_deref(), Some(&b"png"[..]));
        assert_eq!(
            data(ClipboardFormatId::CF_DIB).as_deref(),
            Some(&b"dib"[..])
        );
        assert_eq!(data(ClipboardFormatId::CF_DIBV5), None);
        assert_eq!(data(ClipboardFormatId(0xc0ff)), None);
        // Only present representations are announced.
        assert_eq!(
            Local::from("t").formats(),
            [ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)]
        );
        assert!(Local::default().formats().is_empty());
    }

    #[test]
    fn image_and_html_requests_are_answered_and_bounded_like_text() {
        let mut clipboard = Clipboard::default();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        clipboard.set_sender(sender);
        let mut backend = clipboard.build_cliprdr_backend();
        {
            let mut s = clipboard.shared.lock().unwrap();
            s.authenticated = true;
            s.local = Some(Local {
                png: Some(Arc::from(&b"png"[..])),
                dib: Some(Arc::from(&b"dib"[..])),
                ..Local::default()
            });
        }
        let mut answer = |format| {
            backend.on_format_data_request(FormatDataRequest { format });
            let event = events.try_recv().unwrap();
            clipboard.on_format_data_response_drained();
            match event {
                ServerEvent::Clipboard(ClipboardMessage::SendFormatData(response)) => {
                    (!response.is_error()).then(|| response.data().to_vec())
                }
                _ => panic!("unexpected clipboard event"),
            }
        };
        assert_eq!(
            answer(ClipboardFormatId::CF_DIB).as_deref(),
            Some(&b"dib"[..])
        );
        assert_eq!(answer(LOCAL_PNG_ID).as_deref(), Some(&b"png"[..]));
        // Absent representations get error responses.
        assert_eq!(answer(ClipboardFormatId::CF_UNICODETEXT), None);
        assert_eq!(answer(LOCAL_HTML_ID), None);
        // While a response is queued, images also wait behind an error.
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        backend.on_format_data_request(FormatDataRequest {
            format: ClipboardFormatId::CF_DIB,
        });
        assert_eq!(next_format_data(&mut events), Some(true));
        assert_eq!(next_format_data(&mut events), Some(false));
    }

    fn next_paste(
        events: &mut tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) -> Option<ClipboardFormatId> {
        match events.try_recv() {
            Ok(ServerEvent::Clipboard(ClipboardMessage::SendInitiatePaste(format))) => Some(format),
            Ok(_) => panic!("unexpected clipboard event"),
            Err(_) => None,
        }
    }

    #[test]
    fn remote_formats_are_fetched_one_at_a_time_and_published_once() {
        let (clipboard, mut backend, mut events) = remote_backend();
        let now = Instant::now();
        let html_id = ClipboardFormatId(0xc0f3);
        let copy = [
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
            named(0xc0f3, "HTML Format"),
        ];
        backend.remote_copy_at(&copy, now);
        assert_eq!(
            next_paste(&mut events),
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );
        assert_eq!(next_paste(&mut events), None);
        backend.format_data_response_at(FormatDataResponse::new_unicode_string("text"), now);
        // Nothing is published until the last representation arrives.
        assert_eq!(take_remote(&clipboard), None);
        assert_eq!(next_paste(&mut events), Some(html_id));
        let cf_html = html::to_cf_html("<i>rich</i>");
        backend.format_data_response_at(FormatDataResponse::new_data(cf_html), now);
        let remote = take_remote(&clipboard).unwrap();
        assert_eq!(remote.text.as_deref(), Some("text"));
        assert!(
            remote
                .html
                .unwrap()
                .contains("<!--StartFragment--><i>rich</i><!--EndFragment-->")
        );
        assert_eq!(next_paste(&mut events), None);

        // A failed representation is skipped; the rest is still published.
        backend.remote_copy_at(&copy, now);
        assert_eq!(
            next_paste(&mut events),
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );
        backend.format_data_response_at(FormatDataResponse::new_unicode_string("only"), now);
        assert_eq!(next_paste(&mut events), Some(html_id));
        backend.format_data_response_at(FormatDataResponse::new_error(), now);
        assert_eq!(take_remote(&clipboard), Some("only".into()));

        // A newer copy mid-chain discards the older one's remaining formats.
        backend.remote_copy_at(&copy, now);
        assert_eq!(
            next_paste(&mut events),
            Some(ClipboardFormatId::CF_UNICODETEXT)
        );
        let screenshot = [ClipboardFormat::new(ClipboardFormatId::CF_DIB)];
        backend.remote_copy_at(&screenshot, now);
        assert_eq!(next_paste(&mut events), None);
        backend.format_data_response_at(FormatDataResponse::new_unicode_string("stale"), now);
        assert_eq!(take_remote(&clipboard), None);
        assert_eq!(next_paste(&mut events), Some(ClipboardFormatId::CF_DIB));
        let dib = image::encode_dib(&image::Rgba {
            width: 1,
            height: 1,
            pixels: vec![1, 2, 3, 255],
        });
        backend.format_data_response_at(FormatDataResponse::new_data(dib.clone()), now);
        let remote = take_remote(&clipboard).unwrap();
        assert_eq!(remote.image, Some((Kind::Dib, Arc::from(dib))));
        assert!(remote.text.is_none() && remote.html.is_none());

        // Unusable or oversize images and all-failed copies publish nothing.
        for (format, data) in [
            (named(0xc10a, "PNG"), b"not a png".to_vec()),
            (
                ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                vec![0; image::MAX_DIB_BYTES + 1],
            ),
        ] {
            backend.remote_copy_at(&[format], now);
            assert!(next_paste(&mut events).is_some());
            backend.format_data_response_at(FormatDataResponse::new_data(data), now);
            assert_eq!(take_remote(&clipboard), None);
        }
        backend.remote_copy_at(&copy, now);
        backend.format_data_response_at(FormatDataResponse::new_error(), now);
        backend.format_data_response_at(FormatDataResponse::new_error(), now);
        assert_eq!(take_remote(&clipboard), None);
    }

    fn finished(kind: Kind, data: &[u8], ok: bool) -> Receive {
        let (reader, _writer) = std::io::pipe().unwrap();
        Receive {
            kind,
            fd: OwnedFd::from(reader).into(),
            data: data.to_vec(),
            deadline: Instant::now(),
            finished: Some(ok),
        }
    }

    #[test]
    fn native_selection_reads_become_every_wire_form() {
        let types = |list: &[&str]| list.iter().map(|t| t.to_string()).collect::<Vec<_>>();
        assert_eq!(
            local_plan(&types(&[
                "text/html",
                "text/plain",
                "image/png",
                "UTF8_STRING"
            ])),
            [(Kind::Text, "UTF8_STRING"), (Kind::Html, HTML_MIME)]
        );
        assert_eq!(
            local_plan(&types(&["image/png", "text/html"])),
            [(Kind::Html, HTML_MIME), (Kind::Png, PNG_MIME)]
        );
        assert!(local_plan(&types(&["image/jpeg", "text/uri-list"])).is_empty());

        let png = image::encode_png(&image::Rgba {
            width: 2,
            height: 1,
            pixels: vec![9, 8, 7, 255, 1, 2, 3, 128],
        });
        let local = local_from_reads(vec![
            finished(Kind::Html, "<p>é</p>".as_bytes(), true),
            finished(Kind::Png, &png, true),
        ])
        .unwrap();
        assert!(local.text.is_none());
        let cf_html = local.html.as_deref().unwrap();
        assert!(
            html::from_cf_html(cf_html)
                .unwrap()
                .contains("<!--StartFragment--><p>é</p><!--EndFragment-->")
        );
        assert_eq!(local.png.as_deref(), Some(&png[..]));
        assert_eq!(local.dib.as_deref(), image::png_to_dib(&png).as_deref());
        // Invalid parts are absent; nothing usable means no local content.
        let local = local_from_reads(vec![
            finished(Kind::Text, b"ok", true),
            finished(Kind::Png, b"\x89PNG broken", true),
            finished(Kind::Html, b"<p>late</p>", false),
        ])
        .unwrap();
        assert_eq!(local, Local::from("ok"));
        assert!(
            local_from_reads(vec![
                finished(Kind::Text, b"a\0b", true),
                finished(Kind::Png, &png, false)
            ])
            .is_none()
        );
        assert!(local_from_reads(Vec::new()).is_none());
    }

    #[test]
    fn oversize_native_reads_fail_at_their_own_bound() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        // SAFETY: fcntl operates on our fresh pipe descriptor.
        assert!(unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } >= 0);
        let mut receive = Receive {
            kind: Kind::Html,
            fd: OwnedFd::from(reader).into(),
            data: vec![b'a'; html::MAX_HTML_BYTES - 1],
            deadline: Instant::now() + TIMEOUT,
            finished: None,
        };
        writer.write_all(b"b").unwrap();
        receive.pump();
        assert_eq!(receive.finished, None);
        writer.write_all(b"c").unwrap();
        receive.pump();
        assert_eq!(receive.finished, Some(false));
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let mut receive = Receive {
            kind: Kind::Png,
            fd: OwnedFd::from(reader).into(),
            data: Vec::new(),
            deadline: Instant::now() + TIMEOUT,
            finished: None,
        };
        receive.pump();
        assert_eq!(receive.finished, Some(true));
        assert!(
            Kind::Png.limit() == image::MAX_IMAGE_BYTES && Kind::Text.limit() == MAX_TEXT_BYTES
        );
    }

    #[test]
    fn remote_images_convert_off_the_rdp_task_and_unusable_ones_keep_the_selection() {
        let clipboard = Clipboard::default();
        let generation = clipboard.attachment().generation;
        clipboard.shared.lock().unwrap().authenticated = true;
        let mut queue = {
            let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            Connection::from_socket(socket)
                .unwrap()
                .new_event_queue::<WaylandState>()
        };
        let qh = queue.handle();
        let mut state = WaylandState::new(clipboard.shared.clone(), generation);
        let pixels = image::Rgba {
            width: 1,
            height: 2,
            pixels: vec![1, 2, 3, 255, 4, 5, 6, 255],
        };
        let dib: Arc<[u8]> = image::encode_dib(&pixels).into();
        state.remote_content(
            Remote {
                text: Some("caption".into()),
                html: None,
                image: Some((Kind::Dib, dib.clone())),
            },
            &qh,
        );
        {
            let shared = clipboard.shared.lock().unwrap();
            let local = shared.local.as_ref().unwrap();
            assert_eq!(local.text.as_deref(), Some("caption"));
            assert_eq!(local.dib, Some(dib));
            assert_eq!(
                image::decode_png(local.png.as_deref().unwrap()).unwrap(),
                pixels
            );
            assert!(!shared.dirty);
        }
        // A DIB that cannot be decoded changes nothing.
        clipboard.shared.lock().unwrap().local = Some("kept".into());
        state.remote_content(
            Remote {
                image: Some((Kind::Dib, Arc::from(&b"bad"[..]))),
                ..Remote::default()
            },
            &qh,
        );
        assert_eq!(clipboard.shared.lock().unwrap().local, Some("kept".into()));
        let _ = queue.dispatch_pending(&mut state);
    }
}

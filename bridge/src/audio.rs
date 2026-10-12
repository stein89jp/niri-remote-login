//! Session audio through the TLS-protected RDP sound channel.
use ironrdp_rdpsnd::{
    pdu::{AudioFormat, WaveFormat},
    server::{NegotiatedFormat, RdpsndError},
};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use std::{
    collections::VecDeque,
    ffi::{CStr, c_char, c_void},
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::mpsc::UnboundedSender;

pub const BLOCK_BYTES: usize = 48000 * 2 * 2 / 50; // 20 ms, stereo S16LE
const ADPCM_SAMPLES: usize = 1012;
const ADPCM_BLOCK_BYTES: usize = 1024;
// IronRDP keeps at most four waves per event batch. Stay below that limit
// so every queued wave receives a block number and can be acknowledged.
const MAX_PENDING: usize = 4;
const MAX_CAPTURE_BLOCKS: usize = 3; // At most 64 ms of capture batching/jitter
const PLAYBACK_BUDGET: Duration = Duration::from_millis(120);

unsafe extern "C" {
    fn niri_audio_new(error: *mut c_char, length: usize) -> *mut c_void;
    fn niri_audio_free(encoder: *mut c_void);
    fn niri_audio_extra(encoder: *mut c_void, data: *mut u8, capacity: usize) -> i32;
    fn niri_audio_encode(
        encoder: *mut c_void,
        pcm: *const u8,
        length: usize,
        data: *mut *const u8,
        error: *mut c_char,
        error_length: usize,
    ) -> i32;
}

struct Adpcm {
    ptr: *mut c_void,
    format: AudioFormat,
}
// The FFmpeg context has one owner, moved to the capture worker after negotiation.
unsafe impl Send for Adpcm {}
impl Drop for Adpcm {
    fn drop(&mut self) {
        unsafe { niri_audio_free(self.ptr) };
    }
}
impl Adpcm {
    fn new() -> anyhow::Result<Self> {
        let mut error = [0 as c_char; 256];
        let ptr = unsafe { niri_audio_new(error.as_mut_ptr(), error.len()) };
        anyhow::ensure!(
            !ptr.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        let mut encoder = Self {
            ptr,
            format: AudioFormat {
                format: WaveFormat::ADPCM,
                n_channels: 2,
                n_samples_per_sec: 48000,
                n_avg_bytes_per_sec: (48000 * ADPCM_BLOCK_BYTES / ADPCM_SAMPLES) as u32,
                n_block_align: ADPCM_BLOCK_BYTES as u16,
                bits_per_sample: 4,
                data: None,
            },
        };
        let mut extra = vec![0; 32];
        anyhow::ensure!(
            unsafe { niri_audio_extra(ptr, extra.as_mut_ptr(), extra.len()) } == 32,
            "invalid MS ADPCM format data"
        );
        encoder.format.data = Some(extra);
        Ok(encoder)
    }
    fn encode(&mut self, pcm: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut error = [0 as c_char; 256];
        let mut data = std::ptr::null();
        let length = unsafe {
            niri_audio_encode(
                self.ptr,
                pcm.as_ptr(),
                pcm.len(),
                &mut data,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        anyhow::ensure!(
            length == ADPCM_BLOCK_BYTES as i32 && !data.is_null(),
            "{}",
            unsafe { CStr::from_ptr(error.as_ptr()) }.to_string_lossy()
        );
        Ok(unsafe { std::slice::from_raw_parts(data, length as usize) }.to_vec())
    }
}

fn pcm_format() -> AudioFormat {
    AudioFormat {
        format: WaveFormat::PCM,
        n_channels: 2,
        n_samples_per_sec: 48000,
        n_avg_bytes_per_sec: 192000,
        n_block_align: 4,
        bits_per_sample: 16,
        data: None,
    }
}

fn monotonic_millis() -> std::io::Result<u32> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000) as u32)
}

#[derive(Debug)]
struct SentWave {
    number: u8,
    timestamp: u16,
    acknowledged: bool,
    feedback_ms: u16,
    playback_confirmed: bool,
}

/// Arrival confirms release transport credit, but do not mean the sound played.
/// mstsc and FreeRDP also report the playback queue in a later confirm. Remember
/// those waves after arrival so a growing client buffer can drain before we
/// send current audio again. Clients sending one confirm continue to work.
#[derive(Default, Debug)]
struct FlowControl {
    pending: usize,
    recent: VecDeque<SentWave>,
    hold_until: Option<Instant>,
    last_warning: Option<Instant>,
    latest_sent: Option<Instant>,
    paired_confirms: bool,
}
impl FlowControl {
    fn can_send(&self, now: Instant) -> bool {
        self.pending < MAX_PENDING
            && self.hold_until.is_none_or(|until| now >= until)
            // Block IDs wrap after about five seconds. Once the client has
            // demonstrated separate receipt/playback confirms, do not reuse
            // an ID still awaiting playback. A late render ACK otherwise
            // looks like a confirmation of the newer wave with that ID.
            && !(self.paired_confirms
                && self.recent.len() == 256
                && self.recent.front().is_some_and(|wave| !wave.playback_confirmed))
    }
    fn sent(&mut self, number: u8, timestamp: u16, now: Instant) {
        if self.recent.len() == 256 {
            // After 256 consecutively numbered waves, FIFO dispatch and the
            // four-credit cap prove this old wave reached the wire. Retire a
            // lost ACK at number reuse, without expiring still-queued events.
            if let Some(old) = self.recent.pop_front()
                && !old.acknowledged
            {
                self.pending -= 1;
            }
        }
        self.pending += 1;
        self.latest_sent = Some(now);
        self.recent.push_back(SentWave {
            number,
            timestamp,
            acknowledged: false,
            feedback_ms: 0,
            playback_confirmed: false,
        });
    }
    fn confirm(&mut self, number: u8, timestamp: u16, now: Instant) {
        let Some(position) = self.recent.iter().rposition(|wave| wave.number == number) else {
            return;
        };
        let wave = &mut self.recent[position];
        let held_ms = timestamp.wrapping_sub(wave.timestamp);
        if held_ms >= 0x8000 {
            return;
        }
        let already_acknowledged = wave.acknowledged;
        if !already_acknowledged {
            wave.acknowledged = true;
            self.pending -= 1;
        }
        let increased = held_ms > wave.feedback_ms;
        let playback_confirmed = already_acknowledged && (increased || self.paired_confirms);
        if already_acknowledged && increased {
            self.paired_confirms = true;
        }
        if playback_confirmed {
            // Audio is consumed in stream order. Completion of a later wave
            // also retires a missing earlier confirm, so one lost/equal ACK
            // cannot permanently mute a paired client at block-number reuse.
            for old in self.recent.iter_mut().take(position + 1) {
                old.playback_confirmed = true;
                if !old.acknowledged {
                    old.acknowledged = true;
                    self.pending -= 1;
                }
            }
        }
        // Receipt itself can take a few milliseconds: a positive first
        // confirmation does not prove playback finished. Keep accepting
        // increased feedback for this wave, including the later render ACK.
        // Duplicate or reordered smaller reports must not restart recovery.
        if !increased {
            return;
        }
        self.recent[position].feedback_ms = held_ms;
        let held = Duration::from_millis(u64::from(held_ms));
        if held > PLAYBACK_BUDGET {
            // Anchor to the queued tail, not the receipt of a delayed render
            // ACK. A run of old confirmations must not repeatedly restart the
            // pause after the client's old buffer has already drained.
            let until = self.latest_sent.unwrap_or(now) + (held - PLAYBACK_BUDGET);
            self.hold_until = Some(self.hold_until.map_or(until, |old| old.max(until)));
            if self
                .last_warning
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(5))
            {
                eprintln!(
                    "RDP audio client playback delay {held_ms} ms; dropping capture while its buffer drains"
                );
                self.last_warning = Some(now);
            }
        }
    }
}

#[derive(Clone, Default, Debug)]
pub struct Audio(Arc<Mutex<Option<UnixStream>>>);
impl Audio {
    pub fn attach(&self, socket: Option<UnixStream>) {
        *self.0.lock().unwrap() = socket;
    }
    pub fn clear(&self) {
        self.0.lock().unwrap().take();
    }
    pub fn factory(&self) -> Factory {
        Factory {
            audio: self.clone(),
            sender: None,
        }
    }
}
pub struct Factory {
    audio: Audio,
    sender: Option<UnboundedSender<ServerEvent>>,
}
impl ServerEventSender for Factory {
    fn set_sender(&mut self, sender: UnboundedSender<ServerEvent>) {
        self.sender = Some(sender);
    }
}
impl SoundServerFactory for Factory {
    fn build_backend(&self) -> Box<dyn RdpsndServerHandler> {
        let encoder = Adpcm::new()
            .map_err(|error| {
                eprintln!("RDP audio MS ADPCM unavailable; offering PCM: {error}");
                error
            })
            .ok();
        // mstsc advertises this ADPCM layout but real playback can become
        // static. Prefer PCM until compressed playback is validated there;
        // flow control and bounded capture keep either format low-latency.
        let mut formats = vec![pcm_format()];
        if let Some(encoder) = &encoder {
            formats.push(encoder.format.clone());
        }
        Box::new(Backend {
            audio: self.audio.clone(),
            sender: self.sender.clone().expect("sound factory sender"),
            formats,
            encoder,
            stopped: Arc::new(AtomicBool::new(false)),
            pending: Arc::new(Mutex::new(FlowControl::default())),
            worker: None,
        })
    }
}
struct Backend {
    audio: Audio,
    sender: UnboundedSender<ServerEvent>,
    formats: Vec<AudioFormat>,
    encoder: Option<Adpcm>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<FlowControl>>,
    worker: Option<thread::JoinHandle<()>>,
}
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SessionAudio")
    }
}
impl RdpsndServerHandler for Backend {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }
    fn choose_format<'a>(
        &mut self,
        common: &'a [NegotiatedFormat],
    ) -> Option<&'a NegotiatedFormat> {
        common.first()
    }
    fn start(&mut self, format: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
        let mut encoder = if format.format().format == WaveFormat::ADPCM {
            Some(self.encoder.take().ok_or_else(|| {
                Box::new(std::io::Error::other("MS ADPCM encoder unavailable"))
                    as Box<dyn RdpsndError>
            })?)
        } else {
            self.encoder.take();
            None
        };
        let block_bytes = if encoder.is_some() {
            ADPCM_SAMPLES * 4
        } else {
            BLOCK_BYTES
        };
        let block_duration = Duration::from_secs_f64(block_bytes as f64 / 192000.0);
        let start_timestamp =
            monotonic_millis().map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?;
        let clock_start = Instant::now();
        eprintln!(
            "RDP audio {}: 48 kHz stereo, {} kbps, {:.2} ms blocks",
            if encoder.is_some() { "MS ADPCM" } else { "PCM" },
            format.format().n_avg_bytes_per_sec * 8 / 1000,
            block_duration.as_secs_f64() * 1000.0,
        );
        let audio = self.audio.clone();
        let sender = self.sender.clone();
        let stopped = self.stopped.clone();
        let pending = self.pending.clone();
        self.worker = Some(
            thread::Builder::new()
                .name("rdp-audio".into())
                .spawn(move || {
                    // Negotiation may precede submission of credentials on the login form.
                    let mut socket = loop {
                        if stopped.load(Ordering::Acquire) {
                            return;
                        }
                        if let Some(socket) = audio.0.lock().unwrap().take() {
                            break socket;
                        }
                        thread::sleep(Duration::from_millis(20));
                    };
                    if socket.write_all(b"A").is_err() || socket.set_nonblocking(true).is_err() {
                        return;
                    }
                    let start = Instant::now();
                    let mut block = vec![0u8; block_bytes];
                    let mut captured = VecDeque::new();
                    let mut filled = 0;
                    let mut number = 0u8;
                    let mut next_send = start;
                    'capture: while !stopped.load(Ordering::Acquire) {
                        // Keep a small FIFO for normal PipeWire quantum batching
                        // and scheduling jitter. Drop only capture exceeding the
                        // three-block budget, preventing replay after a stall.
                        // Keep partial blocks intact to preserve stereo alignment.
                        let mut drained = false;
                        for _ in 0..64 {
                            if stopped.load(Ordering::Acquire) {
                                break 'capture;
                            }
                            match socket.read(&mut block[filled..]) {
                                Ok(0) => break 'capture,
                                Ok(n) => filled += n,
                                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    drained = true;
                                    break;
                                }
                                Err(_) => break 'capture,
                            }
                            if filled == block_bytes {
                                if captured.len() == MAX_CAPTURE_BLOCKS {
                                    captured.pop_front();
                                }
                                // Wave2 timestamps describe capture, and use
                                // the boot-relative millisecond clock required
                                // by MS-RDPEA, even when paced after a quantum.
                                let timestamp = start_timestamp
                                    .wrapping_add(clock_start.elapsed().as_millis() as u32);
                                captured.push_back((block.clone(), timestamp));
                                filled = 0;
                            }
                        }
                        if !drained {
                            continue;
                        }
                        let now = Instant::now();
                        let mut queue = pending.lock().unwrap();
                        if !queue.can_send(now) {
                            // Always consume capture while waiting for transport
                            // credit or playback recovery. Resume with fresh sound.
                            captured.clear();
                        } else if now >= next_send
                            && let Some((data, timestamp)) = captured.pop_front()
                        {
                            let data = if let Some(encoder) = &mut encoder {
                                match encoder.encode(&data) {
                                    Ok(data) => data,
                                    Err(error) => {
                                        eprintln!("RDP audio encoding stopped: {error}");
                                        break;
                                    }
                                }
                            } else {
                                data
                            };
                            queue.sent(number, timestamp as u16, now);
                            number = number.wrapping_add(1);
                            if sender
                                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                                    data, timestamp,
                                )))
                                .is_err()
                            {
                                break;
                            }
                            next_send = if now.duration_since(next_send) >= block_duration {
                                now + block_duration
                            } else {
                                next_send + block_duration
                            };
                        }
                        drop(queue);
                        thread::sleep(Duration::from_millis(2));
                    }
                })
                .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?,
        );
        Ok(())
    }
    fn wave_confirm(&mut self, block_no: u8, timestamp: u16) {
        self.pending
            .lock()
            .unwrap()
            .confirm(block_no, timestamp, Instant::now());
    }
    fn stop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.audio.clear();
    }
}
impl Drop for Backend {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::{decode, encode_vec};
    use ironrdp_rdpsnd::{pdu::*, server::RdpsndServer};
    use ironrdp_svc::SvcProcessor;
    use std::io::Write;

    fn negotiate(
        audio: &Audio,
        formats: Vec<AudioFormat>,
    ) -> (
        RdpsndServer,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        negotiate_version(audio, formats, Version::V8)
    }

    fn negotiate_version(
        audio: &Audio,
        formats: Vec<AudioFormat>,
        version: Version,
    ) -> (
        RdpsndServer,
        tokio::sync::mpsc::UnboundedReceiver<ServerEvent>,
    ) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let mut server = RdpsndServer::new(factory.build_backend());
        server.start().unwrap();
        for pdu in [
            ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
                version,
                flags: AudioFormatFlags::ALIVE,
                formats,
                volume_left: u16::MAX,
                volume_right: u16::MAX,
                pitch: 0,
                dgram_port: 0,
            }),
            ClientAudioOutputPdu::QualityMode(QualityModePdu {
                quality_mode: QualityMode::High,
            }),
            ClientAudioOutputPdu::TrainingConfirm(TrainingConfirmPdu {
                timestamp: 4231,
                pack_size: 0,
            }),
        ] {
            server.process(&encode_vec(&pdu).unwrap()).unwrap();
        }
        (server, receiver)
    }

    fn pcm() -> AudioFormat {
        pcm_format()
    }

    #[test]
    fn pcm_preferred_over_adpcm_preserves_signed_stereo_on_both_wave_versions() {
        for version in [Version::V6, Version::V8] {
            let audio = Audio::default();
            let adpcm = Adpcm::new().unwrap().format.clone();
            let mut unsupported = pcm();
            unsupported.n_samples_per_sec = 44100;
            let (mut server, mut receiver) =
                negotiate_version(&audio, vec![adpcm, unsupported, pcm()], version);
            assert!(server.is_ready());
            let (mut source, capture) = UnixStream::pair().unwrap();
            source
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            audio.attach(Some(capture));
            source.read_exact(&mut [0]).unwrap();
            // Distinct signed channels and nonzero first samples also check the
            // four-byte prefix split in the older WaveInfo/Wave wire format.
            let expected: Vec<u8> = (0..BLOCK_BYTES / 4)
                .flat_map(|frame| {
                    let left = ((-30000 + frame as i32 * 31) as i16).to_le_bytes();
                    let right = ((20000 - frame as i32 * 37) as i16).to_le_bytes();
                    [left[0], left[1], right[0], right[1]]
                })
                .collect();
            source.write_all(&expected).unwrap();
            let deadline = Instant::now() + Duration::from_secs(2);
            let (data, timestamp) = loop {
                if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                    receiver.try_recv()
                {
                    break (data, timestamp);
                }
                assert!(Instant::now() < deadline, "expected a 20 ms PCM block");
                thread::sleep(Duration::from_millis(1));
            };
            assert_eq!(
                data, expected,
                "PCM must be selected even when ADPCM comes first in the client's list"
            );
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let bytes = messages[0].encode_unframed_pdu().unwrap();
            match decode(&bytes).unwrap() {
                ServerAudioOutputPdu::Wave2(wave) => {
                    assert_eq!(messages.len(), 1);
                    assert_eq!(wave.format_no, 2);
                    assert_eq!(wave.data.as_ref(), expected);
                }
                ServerAudioOutputPdu::Wave(info) => {
                    assert_eq!(messages.len(), 2);
                    assert_eq!(info.format_no, 2);
                    assert_eq!(usize::from(info.audio_length), expected.len());
                    let payload = messages[1].encode_unframed_pdu().unwrap();
                    assert_eq!(payload.len(), expected.len());
                    assert_eq!(&payload[..4], &[0; 4]);
                    let mut reconstructed = info.data_prefix.to_vec();
                    reconstructed.extend_from_slice(&payload[4..]);
                    assert_eq!(reconstructed, expected);
                }
                _ => panic!("expected an audio wave"),
            }
            drop(server);
            assert_eq!(source.read(&mut [0]).unwrap(), 0);
        }
    }

    #[test]
    fn negotiated_adpcm_used_when_pcm_not_accepted() {
        let audio = Audio::default();
        let format = Adpcm::new().unwrap().format.clone();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 44100;
        let (mut server, mut receiver) = negotiate(&audio, vec![unsupported, format.clone()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        source.write_all(&vec![0x35; ADPCM_SAMPLES * 4]).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let (data, timestamp) = loop {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                break (data, timestamp);
            }
            assert!(
                Instant::now() < deadline,
                "capture must produce an ADPCM packet"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(data.len(), ADPCM_BLOCK_BYTES);
        assert!(
            data.len() * 3 < ADPCM_SAMPLES * 4,
            "compact audio must reduce transport traffic"
        );
        assert!(monotonic_millis().unwrap().wrapping_sub(timestamp) < 2000);
        let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
        let ServerAudioOutputPdu::Wave2(wave) =
            decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
        else {
            panic!("expected wave2");
        };
        assert_eq!(
            wave.format_no, 1,
            "server preference must keep the client's index"
        );
        assert_eq!(wave.audio_timestamp, timestamp);
        assert_eq!(wave.data.len(), usize::from(format.n_block_align));
        drop(server);
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "disconnect must release the encoder and capture FD"
        );
        // A client accepting only PCM must also remain usable.
        let (server, receiver) = negotiate(&audio, vec![pcm()]);
        assert!(server.is_ready() && receiver.is_empty());
        drop(server);
    }

    #[test]
    fn adpcm_wire_and_flow_survive_more_than_a_minute_of_both_timestamp_wraps() {
        let audio = Audio::default();
        let mut encoder = Adpcm::new().unwrap();
        let (mut server, _receiver) = negotiate(&audio, vec![encoder.format.clone()]);
        let start = Instant::now();
        let pcm = vec![0x35; ADPCM_SAMPLES * 4];
        let mut flow = FlowControl::default();
        for sequence in 0..3400u32 {
            let elapsed_ms = u64::from(sequence) * ADPCM_SAMPLES as u64 * 1000 / 48000;
            let now = start + Duration::from_millis(elapsed_ms);
            let timestamp = 65000 + elapsed_ms as u32;
            assert!(flow.can_send(now));
            let messages: Vec<_> = server
                .wave(encoder.encode(&pcm).unwrap(), timestamp)
                .unwrap()
                .into();
            let ServerAudioOutputPdu::Wave2(wave) =
                decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
            else {
                panic!("expected wave2");
            };
            assert_eq!(wave.block_no, sequence as u8);
            assert_eq!(wave.timestamp, timestamp as u16);
            assert_eq!(wave.audio_timestamp, timestamp);
            assert_eq!(wave.data.len(), ADPCM_BLOCK_BYTES);
            flow.sent(wave.block_no, wave.timestamp, now);
            flow.confirm(
                wave.block_no,
                wave.timestamp.wrapping_add(3),
                now + Duration::from_millis(3),
            );
            flow.confirm(
                wave.block_no,
                wave.timestamp.wrapping_add(30),
                now + Duration::from_millis(30),
            );
            assert_eq!(flow.pending, 0);
            assert!(
                flow.hold_until.is_none(),
                "healthy playback must not be muted at either wrap"
            );
        }
        assert!(flow.paired_confirms);
        assert_eq!(flow.recent.len(), 256);
        drop(server);
    }

    #[test]
    fn closed_event_receiver_stops_capture_and_releases_the_codec() {
        let audio = Audio::default();
        let format = Adpcm::new().unwrap().format.clone();
        let (server, receiver) = negotiate(&audio, vec![format]);
        drop(receiver);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        source.write_all(&vec![0x35; ADPCM_SAMPLES * 4]).unwrap();
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "failed event send must close the source socket"
        );
        drop(server);
    }

    #[test]
    fn negotiated_pcm_reaches_wire_with_client_format_index_and_bounded_backpressure() {
        let audio = Audio::default();
        let mut unsupported = pcm();
        unsupported.n_samples_per_sec = 44100;
        let (mut server, mut receiver) = negotiate(&audio, vec![unsupported, pcm()]);
        assert!(server.is_ready());
        assert!(
            receiver.try_recv().is_err(),
            "login screen must not produce sound"
        );
        let (mut source, capture) = UnixStream::pair().unwrap();
        audio.attach(Some(capture));
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut ready = [0];
        source.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"A");
        let expected = vec![0x35; BLOCK_BYTES];
        for _ in 0..12 {
            source.write_all(&expected).unwrap();
            thread::sleep(Duration::from_millis(25));
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(receiver.len(), MAX_PENDING);
        for number in 0..MAX_PENDING {
            let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
                receiver.try_recv().unwrap()
            else {
                panic!("expected PCM wave")
            };
            assert_eq!(data, expected);
            let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
            let bytes = messages[0].encode_unframed_pdu().unwrap();
            let ServerAudioOutputPdu::Wave2(wave) = decode(&bytes).unwrap() else {
                panic!("expected wave2")
            };
            assert_eq!(
                wave.format_no, 1,
                "format number must index the client's list"
            );
            assert_eq!(wave.block_no, number as u8);
            assert_eq!(wave.data.as_ref(), expected);
            let confirm = ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                block_no: wave.block_no,
                timestamp: wave.timestamp,
            });
            server.process(&encode_vec(&confirm).unwrap()).unwrap();
            server.process(&encode_vec(&confirm).unwrap()).unwrap();
        }
        source.write_all(&expected).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(receiver.len(), 1, "confirmation must unblock current audio");
        drop(server);
        source
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert_eq!(
            source.read(&mut [0]).unwrap(),
            0,
            "disconnect closes capture FD"
        );
        let (server, receiver) = negotiate(&audio, vec![pcm()]);
        assert!(
            receiver.is_empty(),
            "reconnect must not replay previous audio"
        );
        drop(server);
    }

    #[test]
    fn render_feedback_survives_arrival_ack_and_does_not_extend_recovery() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        for number in 0..4 {
            flow.sent(number, 65500 + u16::from(number), start);
            flow.confirm(number, 65500 + u16::from(number), start);
        }
        assert_eq!(
            flow.pending, 0,
            "arrival ACK releases only transport credit"
        );
        flow.confirm(
            0,
            65500u16.wrapping_add(3000),
            start + Duration::from_secs(3),
        );
        assert!(!flow.can_send(start + Duration::from_secs(2)));
        let deadline = flow.hold_until;
        // Other queued waves finish later. They must not add another three
        // seconds of muting to a buffer that is already finishing playback.
        flow.confirm(
            1,
            65501u16.wrapping_add(3000),
            start + Duration::from_secs(4),
        );
        assert_eq!(flow.hold_until, deadline);
        assert!(flow.can_send(start + Duration::from_secs(4)));
        flow.confirm(
            1,
            65501u16.wrapping_add(3000),
            start + Duration::from_secs(5),
        );
        assert_eq!(flow.hold_until, deadline, "duplicate render ACK is ignored");
        flow.confirm(
            1,
            65501u16.wrapping_add(2000),
            start + Duration::from_secs(5),
        );
        assert_eq!(
            flow.hold_until, deadline,
            "older feedback cannot restart recovery"
        );
    }

    #[test]
    fn nonzero_receipt_confirm_does_not_hide_later_nine_second_playback_delay() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 65000, start);
        flow.confirm(0, 65003, start + Duration::from_millis(3));
        assert_eq!(flow.pending, 0);
        // Another wave is queued while the first is held by the device. Its
        // future playback, rather than ACK arrival time, anchors the pause.
        flow.sent(
            1,
            65000u16.wrapping_add(8000),
            start + Duration::from_secs(8),
        );
        flow.confirm(
            1,
            65000u16.wrapping_add(8003),
            start + Duration::from_secs(8),
        );
        flow.confirm(
            0,
            65000u16.wrapping_add(9000),
            start + Duration::from_secs(9),
        );
        assert!(flow.paired_confirms);
        assert!(
            !flow.can_send(start + Duration::from_secs(9)),
            "actual playback feedback must stop queue growth"
        );
        let deadline = flow.hold_until.unwrap();
        assert_eq!(deadline, start + Duration::from_millis(16880));
        flow.confirm(
            0,
            65000u16.wrapping_add(9000),
            start + Duration::from_secs(10),
        );
        assert_eq!(
            flow.pending, 0,
            "duplicates must not release another wave's credit"
        );
        assert_eq!(flow.hold_until, Some(deadline));
        assert!(flow.can_send(deadline));
    }

    #[test]
    fn paired_clients_do_not_reuse_ids_still_awaiting_playback_feedback() {
        let start = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 1000, start);
        flow.confirm(0, 1001, start);
        flow.confirm(0, 1020, start);
        for sequence in 1..=256u16 {
            let timestamp = 1000 + sequence * 20;
            assert!(flow.can_send(start + Duration::from_millis(u64::from(sequence) * 20)));
            flow.sent(
                sequence as u8,
                timestamp,
                start + Duration::from_millis(u64::from(sequence) * 20),
            );
            flow.confirm(sequence as u8, timestamp + 1, start);
        }
        assert!(
            !flow.can_send(start + Duration::from_secs(6)),
            "block one must not be reused ahead of its playback ACK"
        );
        flow.confirm(1, 1020 + 9000, start + Duration::from_secs(9));
        assert_eq!(flow.recent.front().unwrap().number, 1);
        assert_eq!(
            flow.recent.front().unwrap().feedback_ms,
            9000,
            "late playback feedback must still match the original wave"
        );
        assert!(!flow.can_send(start + Duration::from_secs(9)));
        assert!(flow.can_send(start + Duration::from_secs(14)));
    }

    #[test]
    fn equal_and_missing_playback_confirms_retire_at_the_playback_frontier() {
        let now = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 100, now);
        flow.confirm(0, 100, now);
        flow.confirm(0, 120, now);
        assert!(flow.paired_confirms);
        flow.sent(1, 120, now);
        flow.confirm(1, 120, now);
        flow.confirm(1, 120, now);
        assert!(
            flow.recent[1].playback_confirmed,
            "zero-latency second confirm remains valid"
        );
        flow.sent(2, 140, now);
        // Both confirms for block two go missing, but block three finishes.
        flow.sent(3, 160, now);
        flow.confirm(3, 160, now);
        flow.confirm(3, 190, now);
        assert_eq!(
            flow.pending, 0,
            "later playback proves the earlier queued wave was consumed"
        );
        assert!(flow.recent.iter().all(|wave| wave.playback_confirmed));
        for sequence in 4..=260u16 {
            flow.sent(sequence as u8, 100 + sequence * 20, now);
            flow.confirm(sequence as u8, 100 + sequence * 20, now);
            flow.confirm(sequence as u8, 100 + sequence * 20, now);
            assert!(
                flow.can_send(now),
                "one missing confirm must not mute at number reuse"
            );
        }
    }

    #[test]
    fn stale_confirms_and_lost_confirm_at_number_reuse_preserve_credit() {
        let now = Instant::now();
        let mut flow = FlowControl::default();
        flow.sent(0, 100, now);
        flow.confirm(0, 99, now);
        assert_eq!(flow.pending, 1, "older timestamp cannot free current wave");
        // Lose block zero's ACK, but continue to confirm the other waves.
        for sequence in 1..=300u16 {
            assert!(flow.can_send(now));
            flow.sent(sequence as u8, 100 + sequence * 20, now);
            flow.confirm(sequence as u8, 100 + sequence * 20, now);
        }
        assert_eq!(
            flow.pending, 0,
            "lost ACK retires only at natural number reuse"
        );
        assert_eq!(flow.recent.len(), 256);
    }

    #[test]
    fn capture_bursts_keep_bounded_recent_blocks_and_preserve_fragments() {
        let audio = Audio::default();
        let (server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut burst = vec![0x11; BLOCK_BYTES * 8];
        burst.extend(vec![0x22; BLOCK_BYTES]);
        burst.extend([0x33; 7]);
        source.write_all(&burst).unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            receiver.len(),
            MAX_CAPTURE_BLOCKS,
            "capture burst must stay within the jitter budget"
        );
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected wave");
        };
        assert_eq!(data, vec![0x11; BLOCK_BYTES]);
        receiver.try_recv().unwrap();
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected newest wave");
        };
        assert_eq!(data, vec![0x22; BLOCK_BYTES]);
        source.write_all(&vec![0x33; BLOCK_BYTES - 7]).unwrap();
        thread::sleep(Duration::from_millis(50));
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected wave");
        };
        assert_eq!(
            data,
            vec![0x33; BLOCK_BYTES],
            "partial source reads retain PCM alignment"
        );
        drop(server);
    }

    #[test]
    fn playback_backlog_drops_capture_and_resumes_with_fresh_sound() {
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![pcm()]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        source.write_all(&vec![0x11; BLOCK_BYTES]).unwrap();
        thread::sleep(Duration::from_millis(30));
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp)) =
            receiver.try_recv().unwrap()
        else {
            panic!("expected wave");
        };
        let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
        let ServerAudioOutputPdu::Wave2(wave) =
            decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
        else {
            panic!("expected wave2");
        };
        for held_ms in [3, 400] {
            server
                .process(
                    &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                        block_no: wave.block_no,
                        timestamp: wave.timestamp.wrapping_add(held_ms),
                    }))
                    .unwrap(),
                )
                .unwrap();
        }
        source.write_all(&vec![0x22; BLOCK_BYTES * 20]).unwrap();
        thread::sleep(Duration::from_millis(80));
        assert!(
            receiver.is_empty(),
            "arrival credit cannot bypass playback recovery"
        );
        thread::sleep(Duration::from_millis(250));
        assert!(
            receiver.is_empty(),
            "recovery must not replay discarded samples"
        );
        source.write_all(&vec![0x33; BLOCK_BYTES]).unwrap();
        thread::sleep(Duration::from_millis(30));
        let ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, _)) = receiver.try_recv().unwrap()
        else {
            panic!("expected fresh wave");
        };
        assert_eq!(data, vec![0x33; BLOCK_BYTES]);
        drop(server);
    }

    #[test]
    fn healthy_pipewire_quantum_batching_preserves_every_sample() {
        healthy_capture_batching(pcm(), BLOCK_BYTES, None);
    }

    #[test]
    fn healthy_adpcm_quantum_batching_preserves_every_complete_frame() {
        let encoder = Adpcm::new().unwrap();
        healthy_capture_batching(encoder.format.clone(), ADPCM_SAMPLES * 4, Some(encoder));
    }

    fn healthy_capture_batching(
        format: AudioFormat,
        block_bytes: usize,
        mut encoder: Option<Adpcm>,
    ) {
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![format]);
        let (mut source, capture) = UnixStream::pair().unwrap();
        source
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        audio.attach(Some(capture));
        source.read_exact(&mut [0]).unwrap();
        const QUANTUM_BYTES: usize = 4096;
        let samples: Vec<u8> = (0..QUANTUM_BYTES * 32 / 4)
            .flat_map(|sample| (sample as u32).to_le_bytes())
            .collect();
        let complete = &samples[..samples.len() / block_bytes * block_bytes];
        let expected = if let Some(encoder) = &mut encoder {
            complete
                .chunks(block_bytes)
                .flat_map(|pcm| encoder.encode(pcm).unwrap())
                .collect()
        } else {
            complete.to_vec()
        };
        let producer = thread::spawn(move || {
            let start = Instant::now();
            for (number, chunk) in samples.chunks(QUANTUM_BYTES).enumerate() {
                let deadline = start
                    + Duration::from_secs_f64(number as f64 * QUANTUM_BYTES as f64 / 192000.0);
                thread::sleep(deadline.saturating_duration_since(Instant::now()));
                source.write_all(chunk).unwrap();
            }
            // Keep source open until the consumer verifies all complete blocks.
            source
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut received = Vec::new();
        while received.len() < expected.len() && Instant::now() < deadline {
            if let Ok(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(data, timestamp))) =
                receiver.try_recv()
            {
                received.extend_from_slice(&data);
                let messages: Vec<_> = server.wave(data, timestamp).unwrap().into();
                let ServerAudioOutputPdu::Wave2(wave) =
                    decode(&messages[0].encode_unframed_pdu().unwrap()).unwrap()
                else {
                    panic!("expected wave2");
                };
                server
                    .process(
                        &encode_vec(&ClientAudioOutputPdu::WaveConfirm(WaveConfirmPdu {
                            block_no: wave.block_no,
                            timestamp: wave.timestamp,
                        }))
                        .unwrap(),
                    )
                    .unwrap();
            } else {
                thread::sleep(Duration::from_millis(1));
            }
        }
        let source = producer.join().unwrap();
        assert_eq!(
            received, expected,
            "healthy quantum batching must preserve every PCM sample"
        );
        drop(source);
        drop(server);
    }

    #[test]
    fn incompatible_audio_formats_produce_no_sound() {
        let audio = Audio::default();
        let mut unsupported = pcm();
        unsupported.n_channels = 1;
        let (server, receiver) = negotiate(&audio, vec![unsupported]);
        assert!(!server.is_ready());
        assert!(receiver.is_empty());
    }
}

//! Session PCM playback through the TLS-protected RDP sound channel.
use ironrdp_rdpsnd::{
    pdu::{AudioFormat, WaveFormat},
    server::{NegotiatedFormat, RdpsndError},
};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use std::{
    collections::VecDeque,
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
// IronRDP keeps at most four waves per event batch. Stay below that limit
// so every queued wave receives a block number and can be acknowledged.
const MAX_PENDING: usize = 4;
const MAX_CAPTURE_BLOCKS: usize = 3; // 60 ms of normal capture batching/jitter
const BLOCK_DURATION: Duration = Duration::from_millis(20);
const PLAYBACK_BUDGET: Duration = Duration::from_millis(120);

#[derive(Debug)]
struct SentWave {
    number: u8,
    timestamp: u16,
    acknowledged: bool,
    rendered: bool,
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
}
impl FlowControl {
    fn can_send(&self, now: Instant) -> bool {
        self.pending < MAX_PENDING && self.hold_until.is_none_or(|until| now >= until)
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
            rendered: false,
        });
    }
    fn confirm(&mut self, number: u8, timestamp: u16, now: Instant) {
        let Some(wave) = self
            .recent
            .iter_mut()
            .rev()
            .find(|wave| wave.number == number)
        else {
            return;
        };
        let held_ms = timestamp.wrapping_sub(wave.timestamp);
        if held_ms >= 0x8000 || wave.rendered {
            return;
        }
        if !wave.acknowledged {
            wave.acknowledged = true;
            self.pending -= 1;
        }
        // An exact echo can be the arrival confirm, including duplicates.
        // Nonzero held time carries playback feedback, even for clients with
        // only a single confirmation. Ignore duplicate render feedback.
        if held_ms == 0 {
            return;
        }
        wave.rendered = true;
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
        Box::new(Backend {
            audio: self.audio.clone(),
            sender: self.sender.clone().expect("sound factory sender"),
            formats: vec![AudioFormat {
                format: WaveFormat::PCM,
                n_channels: 2,
                n_samples_per_sec: 48000,
                n_avg_bytes_per_sec: 192000,
                n_block_align: 4,
                bits_per_sample: 16,
                data: None,
            }],
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
    fn start(&mut self, _: &NegotiatedFormat) -> Result<(), Box<dyn RdpsndError>> {
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
                    let mut block = [0u8; BLOCK_BYTES];
                    let mut captured = VecDeque::new();
                    let mut filled = 0;
                    let mut number = 0u8;
                    let mut next_send = start;
                    'capture: while !stopped.load(Ordering::Acquire) {
                        // Keep a small FIFO for normal PipeWire quantum batching
                        // and scheduling jitter. Drop only capture exceeding the
                        // 60 ms budget, preventing a stall from replaying old sound.
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
                            if filled == BLOCK_BYTES {
                                if captured.len() == MAX_CAPTURE_BLOCKS {
                                    captured.pop_front();
                                }
                                captured.push_back(block);
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
                            && let Some(data) = captured.pop_front()
                        {
                            let timestamp = start.elapsed().as_millis() as u32;
                            queue.sent(number, timestamp as u16, now);
                            number = number.wrapping_add(1);
                            if sender
                                .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                                    data.to_vec(),
                                    timestamp,
                                )))
                                .is_err()
                            {
                                break;
                            }
                            next_send = if now.duration_since(next_send) >= BLOCK_DURATION {
                                now + BLOCK_DURATION
                            } else {
                                next_send + BLOCK_DURATION
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
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut factory = audio.factory();
        factory.set_sender(sender);
        let mut server = RdpsndServer::new(factory.build_backend());
        server.start().unwrap();
        for pdu in [
            ClientAudioOutputPdu::AudioFormat(ClientAudioFormatPdu {
                version: Version::V8,
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
            65501u16.wrapping_add(5000),
            start + Duration::from_secs(5),
        );
        assert_eq!(flow.hold_until, deadline, "duplicate render ACK is ignored");
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
        for held_ms in [0, 400] {
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
        let audio = Audio::default();
        let (mut server, mut receiver) = negotiate(&audio, vec![pcm()]);
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
        let expected = samples[..samples.len() / BLOCK_BYTES * BLOCK_BYTES].to_vec();
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

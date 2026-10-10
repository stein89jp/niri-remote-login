//! Session PCM playback through the TLS-protected RDP sound channel.
use ironrdp_rdpsnd::{
    pdu::{AudioFormat, WaveFormat},
    server::{NegotiatedFormat, RdpsndError},
};
use ironrdp_server::{
    RdpsndServerHandler, RdpsndServerMessage, ServerEvent, ServerEventSender, SoundServerFactory,
};
use std::{
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
            pending: Arc::new(Mutex::new(Vec::new())),
            worker: None,
        })
    }
}
struct Backend {
    audio: Audio,
    sender: UnboundedSender<ServerEvent>,
    formats: Vec<AudioFormat>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<Vec<(u8, u16)>>>,
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
                    if socket
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .is_err()
                    {
                        return;
                    }
                    if socket.write_all(b"A").is_err() {
                        return;
                    }
                    let start = Instant::now();
                    let mut block = [0u8; BLOCK_BYTES];
                    let mut filled = 0;
                    let mut number = 0u8;
                    while !stopped.load(Ordering::Acquire) {
                        match socket.read(&mut block[filled..]) {
                            Ok(0) => break,
                            Ok(n) => filled += n,
                            Err(e)
                                if matches!(
                                    e.kind(),
                                    std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                        | std::io::ErrorKind::Interrupted
                                ) =>
                            {
                                continue;
                            }
                            Err(_) => break,
                        }
                        if filled != BLOCK_BYTES {
                            continue;
                        }
                        filled = 0;
                        let mut queue = pending.lock().unwrap();
                        // Keep draining capture when the client stalls: old sound must
                        // never accumulate in the server's unbounded event channel.
                        if queue.len() >= MAX_PENDING {
                            continue;
                        }
                        let timestamp = start.elapsed().as_millis() as u32;
                        queue.push((number, timestamp as u16));
                        number = number.wrapping_add(1);
                        if sender
                            .send(ServerEvent::Rdpsnd(RdpsndServerMessage::Wave(
                                block.to_vec(),
                                timestamp,
                            )))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .map_err(|e| Box::new(e) as Box<dyn RdpsndError>)?,
        );
        Ok(())
    }
    fn wave_confirm(&mut self, block_no: u8, timestamp: u16) {
        // The protocol permits two confirmations for one block. Remove once.
        self.pending.lock().unwrap().retain(|(number, sent)| {
            *number != block_no || timestamp.wrapping_sub(*sent) >= 0x8000
        });
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
        source.write_all(&expected.repeat(12)).unwrap();
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
    fn incompatible_audio_formats_produce_no_sound() {
        let audio = Audio::default();
        let mut unsupported = pcm();
        unsupported.n_channels = 1;
        let (server, receiver) = negotiate(&audio, vec![unsupported]);
        assert!(!server.is_ready());
        assert!(receiver.is_empty());
    }
}

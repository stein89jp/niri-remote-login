//! Microphone redirection on the wire: the bridge's AUDIO_INPUT backend
//! against the pinned MS-RDPEAI client, and the maintained channel recycle.
#[path = "../vendor/ironrdp-server/src/rdpeai_recycle.rs"]
mod rdpeai_recycle;

use ironrdp_core::{decode, encode_vec};
use ironrdp_dvc::pdu::{
    CapabilitiesResponsePdu, CapsVersion, ClosePdu, CreateResponsePdu, CreationStatus, DataPdu,
    DrdynvcClientPdu, DrdynvcDataPdu, DrdynvcServerPdu,
};
use ironrdp_dvc::{DrdynvcServer, DvcMessage, DvcProcessor};
use ironrdp_pdu::rdp::vc::ChannelControlFlags;
use ironrdp_rdpeai::client::{AudioPacketSink, RdpeaiCaptureHandler, RdpeaiClient};
use ironrdp_rdpeai::server::RdpeaiServer;
use ironrdp_rdpsnd::pdu::AudioFormat;
use ironrdp_server::{RdpeaiServerFactory, RdpeaiServerMessage, ServerEvent, ServerEventSender};
use ironrdp_svc::{SvcMessage, SvcProcessor};
use niri_rdp_bridge::microphone::{Microphone, OFFER, PcmFormat, wire};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedReceiver;

const CHANNEL: u32 = 7;

#[derive(Default)]
struct Capture {
    sink: Option<AudioPacketSink>,
    opened: Option<(AudioFormat, AudioFormat, usize)>,
    closed: usize,
}

/// A client microphone that captures only when the server opens it.
struct FakeMicrophone {
    formats: Vec<AudioFormat>,
    capture: Arc<Mutex<Capture>>,
}
impl RdpeaiCaptureHandler for FakeMicrophone {
    fn supported_formats(&self) -> &[AudioFormat] {
        &self.formats
    }
    fn open(
        &mut self,
        capture_format: &AudioFormat,
        encode_format: &AudioFormat,
        packet_size: usize,
        sink: AudioPacketSink,
    ) -> i32 {
        let mut capture = self.capture.lock().unwrap();
        capture.opened = Some((capture_format.clone(), encode_format.clone(), packet_size));
        capture.sink = Some(sink);
        0
    }
    fn set_format(&mut self, _: &AudioFormat, _: usize) -> bool {
        true
    }
    fn close(&mut self) {
        let mut capture = self.capture.lock().unwrap();
        capture.sink = None;
        capture.closed += 1;
    }
}

fn bytes(message: &DvcMessage) -> Vec<u8> {
    encode_vec(message.as_ref()).unwrap()
}

/// Deliver messages back and forth until both sides are quiet.
fn pump(server: &mut RdpeaiServer, client: &mut RdpeaiClient, mut to_client: Vec<DvcMessage>) {
    while !to_client.is_empty() {
        let mut to_server = Vec::new();
        for message in to_client.drain(..) {
            to_server.extend(client.process(CHANNEL, &bytes(&message)).unwrap());
        }
        for message in to_server {
            to_client.extend(server.process(CHANNEL, &bytes(&message)).unwrap());
        }
    }
}

/// Reassemble the client's uplinked DRDYNVC data into RDPEAI payloads.
fn payloads(messages: Vec<SvcMessage>) -> Vec<Vec<u8>> {
    let mut complete = Vec::new();
    let mut partial: Option<(usize, Vec<u8>)> = None;
    for message in messages {
        let pdu = decode::<DrdynvcClientPdu>(&message.encode_unframed_pdu().unwrap()).unwrap();
        let DrdynvcClientPdu::Data(data) = pdu else {
            panic!("unexpected uplink PDU {pdu:?}");
        };
        assert_eq!(data.channel_id(), CHANNEL);
        match data {
            DrdynvcDataPdu::DataFirst(first) => {
                partial = Some((first.length() as usize, first.data().to_vec()));
            }
            DrdynvcDataPdu::Data(data) => match partial.as_mut() {
                Some((length, buffer)) => {
                    buffer.extend_from_slice(data.data());
                    if buffer.len() == *length {
                        complete.push(partial.take().unwrap().1);
                    }
                }
                None => complete.push(data.data().to_vec()),
            },
            _ => panic!("unexpected data PDU"),
        }
    }
    assert!(partial.is_none());
    complete
}

fn next_event(events: &mut UnboundedReceiver<ServerEvent>) -> ServerEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(event) = events.try_recv() {
            return event;
        }
        assert!(Instant::now() < deadline, "no server event");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn read_record(helper: &mut UnixStream) -> (wire::Header, Vec<u8>) {
    let mut header = [0; wire::HEADER_BYTES];
    helper.read_exact(&mut header).unwrap();
    let header = wire::Header::decode(&header).unwrap();
    let mut payload = vec![0; header.length as usize];
    helper.read_exact(&mut payload).unwrap();
    (header, payload)
}

fn sine(frames: usize, offset: usize) -> Vec<u8> {
    (offset..offset + frames)
        .flat_map(|n| {
            let phase = n as f64 * 440.0 * std::f64::consts::TAU / 48_000.0;
            ((phase.sin() * 12_000.0) as i16).to_le_bytes()
        })
        .collect()
}

#[test]
fn negotiation_open_and_data_reach_the_helper_and_close_stops_client_capture() {
    let microphone = Microphone::default();
    let mut factory = microphone.factory();
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    factory.set_sender(sender);
    let mut server = RdpeaiServer::new(factory.build_backend());

    // A client offering 44.1 kHz stereo first, then 48 kHz mono.
    let capture = Arc::new(Mutex::new(Capture::default()));
    let uplink = Arc::new(Mutex::new(Vec::new()));
    let uplinked = uplink.clone();
    let mut client = RdpeaiClient::new(
        Box::new(FakeMicrophone {
            formats: vec![
                PcmFormat::new(44_100, 2).audio_format(),
                PcmFormat::new(48_000, 1).audio_format(),
            ],
            capture: capture.clone(),
        }),
        Box::new(move |_, messages| {
            uplinked.lock().unwrap().extend(messages);
            Ok(())
        }),
    );
    client.start(CHANNEL).unwrap();
    let version = server.start(CHANNEL).unwrap();
    pump(&mut server, &mut client, version);
    assert_eq!(
        server.negotiated_formats(),
        [
            PcmFormat::new(44_100, 2).audio_format(),
            PcmFormat::new(48_000, 1).audio_format()
        ]
    );
    assert!(events.try_recv().is_err(), "no helper yet: no Open");

    // The session helper attaches and reports a recorder.
    let (frontend, mut helper) = UnixStream::pair().unwrap();
    helper
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    microphone.attach(Some(frontend));
    assert_eq!(read_record(&mut helper).0.kind, wire::HELLO);
    let (available, _) = read_record(&mut helper);
    assert_eq!(
        (available.kind, available.rate, available.channels),
        (wire::AVAILABLE, 48_000, 1)
    );
    helper
        .write_all(&wire::HelperMessage::Hello.encode())
        .unwrap();
    helper
        .write_all(&wire::HelperMessage::InUse.encode())
        .unwrap();

    // The server loop applies the backend's Open to this channel.
    let ServerEvent::Rdpeai(RdpeaiServerMessage::Open {
        frames_per_packet,
        initial_format,
        capture_format,
    }) = next_event(&mut events)
    else {
        panic!("expected Open");
    };
    assert_eq!((frames_per_packet, initial_format), (960, 1));
    let open = server
        .open(frames_per_packet, initial_format, capture_format)
        .unwrap();
    pump(&mut server, &mut client, open);
    let (captured, encoded, packet_size) = capture.lock().unwrap().opened.clone().unwrap();
    assert_eq!(captured, PcmFormat::new(48_000, 1).extensible());
    assert_eq!(encoded, PcmFormat::new(48_000, 1).audio_format());
    assert_eq!(packet_size, 1920);
    assert_eq!(
        server.current_format(),
        Some(&PcmFormat::new(48_000, 1).audio_format())
    );

    // A 440 Hz tone in 20 ms packets arrives at the helper intact and in order.
    let mut sent = Vec::new();
    for packet in 0..5 {
        let tone = sine(960, packet * 960);
        sent.extend_from_slice(&tone);
        (capture.lock().unwrap().sink.as_mut().unwrap())(tone);
    }
    for payload in payloads(std::mem::take(&mut *uplink.lock().unwrap())) {
        assert!(server.process(CHANNEL, &payload).unwrap().is_empty());
    }
    let mut received = Vec::new();
    for sequence in 0..5 {
        let (header, payload) = read_record(&mut helper);
        assert_eq!(
            (header.kind, header.sequence, header.rate, header.channels),
            (wire::AUDIO, sequence, 48_000, 1)
        );
        received.extend(payload);
    }
    assert_eq!(received, sent);

    // The last recorder stops: the backend asks for the channel to be
    // recycled. Closing the DVC ends the client's capture.
    helper
        .write_all(&wire::HelperMessage::Idle.encode())
        .unwrap();
    assert!(matches!(
        next_event(&mut events),
        ServerEvent::RdpeaiRecycle
    ));
    server.close(CHANNEL);
    client.close(CHANNEL);
    let capture = capture.lock().unwrap();
    assert!(capture.sink.is_none());
    assert_eq!(capture.closed, 1);
    microphone.clear();
}

fn client_pdu(pdu: DrdynvcClientPdu) -> Vec<u8> {
    encode_vec(&pdu).unwrap()
}

/// One complete DRDYNVC PDU as a single static-channel chunk.
fn chunk(pdu: DrdynvcClientPdu) -> Vec<u8> {
    let body = client_pdu(pdu);
    let flags = ChannelControlFlags::FLAG_FIRST | ChannelControlFlags::FLAG_LAST;
    let mut wire = Vec::with_capacity(8 + body.len());
    wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wire.extend_from_slice(&flags.bits().to_le_bytes());
    wire.extend_from_slice(&body);
    wire
}

fn data(channel: u32, payload: &[u8]) -> DrdynvcClientPdu {
    DrdynvcClientPdu::Data(DrdynvcDataPdu::Data(DataPdu::new(
        channel,
        payload.to_vec(),
    )))
}

fn server_pdu(message: &SvcMessage) -> DrdynvcServerPdu {
    decode(&message.encode_unframed_pdu().unwrap()).unwrap()
}

fn accept(drdynvc: &mut DrdynvcServer, channel: u32) {
    let replies = drdynvc
        .process(&client_pdu(DrdynvcClientPdu::Create(
            CreateResponsePdu::new(channel, CreationStatus::OK),
        )))
        .unwrap();
    // The channel starts with the RDPEAI Version PDU.
    assert_eq!(replies.len(), 1);
    assert!(drdynvc.is_channel_opened(channel));
}

#[test]
fn recycle_closes_then_recreates_and_later_events_use_the_tracked_id() {
    let microphone = Microphone::default();
    let factory = microphone.factory();
    let mut drdynvc =
        DrdynvcServer::new().with_dynamic_channel(RdpeaiServer::new(factory.build_backend()));
    let first = drdynvc.get_channel_id_by_type::<RdpeaiServer>().unwrap();
    // Before the client accepts the channel there is nothing to stop.
    assert!(rdpeai_recycle::recycle(&mut drdynvc, None, || factory.build_backend()).is_none());
    SvcProcessor::start(&mut drdynvc).unwrap();
    let creates = drdynvc
        .process(&client_pdu(DrdynvcClientPdu::Capabilities(
            CapabilitiesResponsePdu::new(CapsVersion::V2),
        )))
        .unwrap();
    assert_eq!(creates.len(), 1);
    accept(&mut drdynvc, first);
    assert_eq!(rdpeai_recycle::current_channel(&drdynvc, None), Some(first));

    let (recycled, created) =
        rdpeai_recycle::recycle(&mut drdynvc, None, || factory.build_backend()).unwrap();
    created.unwrap();
    assert_eq!(recycled.closed, first);
    let second = recycled.channel_id.unwrap();
    assert_ne!(second, first);
    // Close, then Create for a channel with the same name, in one batch.
    assert_eq!(recycled.messages.len(), 2);
    let DrdynvcServerPdu::Close(close) = server_pdu(&recycled.messages[0]) else {
        panic!("expected Close first");
    };
    assert_eq!(close.channel_id(), first);
    let DrdynvcServerPdu::Create(create) = server_pdu(&recycled.messages[1]) else {
        panic!("expected Create second");
    };
    assert_eq!(
        (create.channel_id(), create.channel_name()),
        (second, ironrdp_rdpeai::CHANNEL_NAME)
    );
    // DRDYNVC forgets the closed channel's type: only the tracked id finds
    // the replacement.
    assert_eq!(drdynvc.get_channel_id_by_type::<RdpeaiServer>(), None);
    assert_eq!(rdpeai_recycle::current_channel(&drdynvc, None), None);
    assert_eq!(
        rdpeai_recycle::current_channel(&drdynvc, Some(second)),
        Some(second)
    );
    // A replacement that is still being created is not recycled again.
    assert!(
        rdpeai_recycle::recycle(&mut drdynvc, Some(second), || factory.build_backend()).is_none()
    );
    accept(&mut drdynvc, second);
    assert!(
        drdynvc
            .dvc_by_id_mut::<RdpeaiServer>(second)
            .is_some_and(|mut channel| {
                // Version sent, client Version pending: Open is rejected, not fatal.
                channel
                    .processor_mut()
                    .open(960, 0, OFFER[0].audio_format())
                    .is_err()
            })
    );
}

#[test]
fn late_capture_data_for_a_recycled_channel_is_dropped_until_the_client_closes_it() {
    let microphone = Microphone::default();
    let factory = microphone.factory();
    let mut drdynvc =
        DrdynvcServer::new().with_dynamic_channel(RdpeaiServer::new(factory.build_backend()));
    let first = drdynvc.get_channel_id_by_type::<RdpeaiServer>().unwrap();
    SvcProcessor::start(&mut drdynvc).unwrap();
    drdynvc
        .process(&client_pdu(DrdynvcClientPdu::Capabilities(
            CapabilitiesResponsePdu::new(CapsVersion::V2),
        )))
        .unwrap();
    accept(&mut drdynvc, first);
    let (recycled, _) =
        rdpeai_recycle::recycle(&mut drdynvc, None, || factory.build_backend()).unwrap();
    let second = recycled.channel_id.unwrap();
    let mut closed = rdpeai_recycle::ClosedChannels::default();
    assert!(closed.is_empty());
    closed.insert(recycled.closed);

    // Capture data the client sent before it saw the Close would fail DRDYNVC
    // (and so the connection) if it were processed.
    let late = chunk(data(first, &[0x06, 0, 0]));
    assert!(drdynvc.process(&late[8..]).is_err());
    assert!(closed.drop_late_data(&late));
    // Other channels, fragments and malformed input take the normal path.
    assert!(!closed.drop_late_data(&chunk(data(second, &[0x06, 0, 0]))));
    let mut fragment = late.clone();
    fragment[4..8].copy_from_slice(&ChannelControlFlags::FLAG_FIRST.bits().to_le_bytes());
    assert!(!closed.drop_late_data(&fragment));
    assert!(!closed.drop_late_data(&late[..6]));
    // The client's Close ends the wait and is processed as usual.
    let close = chunk(DrdynvcClientPdu::Close(ClosePdu::new(first)));
    assert!(!closed.drop_late_data(&close));
    assert!(closed.is_empty());
    drdynvc.process(&close[8..]).unwrap();
    assert!(!closed.drop_late_data(&late));

    // The list stays bounded when a client never answers.
    for id in 100..120 {
        closed.insert(id);
    }
    assert!(!closed.drop_late_data(&chunk(data(100, &[0]))));
    assert!(closed.drop_late_data(&chunk(data(119, &[0]))));
}

//! Bound CLIPRDR before the pinned SVC layer allocates its reassembly buffer.
use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatDataResponse};
use ironrdp_core::{decode, encode_vec};
use ironrdp_pdu::rdp::vc::{ChannelControlFlags, ChannelPduHeader};

// The bridge's largest clipboard payload is a packed DIB: 32 MiB of pixels
// plus up to 4 KiB of header, masks and palette, after the eight-byte
// CLIPRDR message header. One MiB of UTF-8 text (four MiB of UTF-16 CRLF)
// fits well inside.
pub(crate) const MAX_CLIPBOARD_PDU_BYTES: usize = 32 * 1024 * 1024 + 4096 + 8;
const MSG_TYPE_FORMAT_DATA_RESPONSE: u16 = 0x0005;

/// What the caller does with one CLIPRDR channel chunk.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChunkVerdict {
    /// Within bounds: hand it to the SVC layer.
    Accept,
    /// Part of an oversize message being skipped: never reaches the SVC layer.
    Discard,
    /// The last chunk of a skipped message. Carries its declared size, and,
    /// for a Format Data Response, a complete failed response to process in
    /// its place, so the request it answered is still answered in order.
    Dropped {
        declared: usize,
        substitute: Option<Vec<u8>>,
    },
}

#[derive(Debug)]
struct Discarding {
    declared: usize,
    received: usize,
    format_data: bool,
}

#[derive(Default, Debug)]
pub(crate) struct ClipboardChunkGuard {
    pending: Option<(usize, usize)>,
    discarding: Option<Discarding>,
}

/// A whole, unfragmented CLIPRDR_FORMAT_DATA_RESPONSE with CB_RESPONSE_FAIL.
fn failed_format_data_response() -> Vec<u8> {
    let pdu = encode_vec(&ClipboardPdu::FormatDataResponse(
        FormatDataResponse::new_error(),
    ))
    .expect("an empty clipboard response always encodes");
    let flags = ChannelControlFlags::FLAG_FIRST | ChannelControlFlags::FLAG_LAST;
    let mut wire = Vec::with_capacity(8 + pdu.len());
    wire.extend_from_slice(&(pdu.len() as u32).to_le_bytes());
    wire.extend_from_slice(&flags.bits().to_le_bytes());
    wire.extend_from_slice(&pdu);
    wire
}

impl ClipboardChunkGuard {
    /// Malformed or compressed chunks are errors that end the connection, as
    /// before. A well-formed message that is merely too large is skipped
    /// without buffering any of it, and the session continues.
    pub(crate) fn accept(&mut self, bytes: &[u8]) -> Result<ChunkVerdict, &'static str> {
        let result = self.validate(bytes);
        if result.is_err() {
            self.pending = None;
            self.discarding = None;
        }
        result
    }

    fn validate(&mut self, bytes: &[u8]) -> Result<ChunkVerdict, &'static str> {
        let header =
            decode::<ChannelPduHeader>(bytes).map_err(|_| "invalid clipboard channel header")?;
        let declared = header.length as usize;
        if declared < 8 {
            return Err("clipboard channel message is shorter than its header");
        }
        // Incoming SVC bulk decompression is not implemented by this server.
        // The pinned dechunker skips length checks for this flag; rejecting it
        // keeps declared and accumulated bytes bounded by the same contract.
        if header
            .flags
            .contains(ChannelControlFlags::PACKET_COMPRESSED)
        {
            return Err("compressed clipboard channel data is unsupported");
        }
        let first = header.flags.contains(ChannelControlFlags::FLAG_FIRST);
        let last = header.flags.contains(ChannelControlFlags::FLAG_LAST);
        let chunk = bytes.len() - 8;
        if let Some(discarding) = &mut self.discarding {
            if first || discarding.declared != declared {
                return Err("clipboard channel fragment does not continue the skipped message");
            }
            discarding.received = discarding
                .received
                .checked_add(chunk)
                .ok_or("clipboard channel size overflow")?;
            if discarding.received > declared {
                return Err("clipboard channel fragments exceed the declared length");
            }
            if !last {
                if chunk == 0 {
                    return Err("empty clipboard channel fragment");
                }
                return Ok(ChunkVerdict::Discard);
            }
            if discarding.received != declared {
                return Err("incomplete clipboard channel message");
            }
            let format_data = discarding.format_data;
            self.discarding = None;
            return Ok(ChunkVerdict::Dropped {
                declared,
                substitute: format_data.then(failed_format_data_response),
            });
        }
        if declared > MAX_CLIPBOARD_PDU_BYTES {
            let legacy = !first && !last && self.pending.is_none() && chunk == declared;
            if !(first || legacy) || self.pending.is_some() {
                return Err("clipboard channel fragment has no matching first fragment");
            }
            if chunk > declared {
                return Err("clipboard channel fragments exceed the declared length");
            }
            let format_data =
                bytes.get(8..10) == Some(&MSG_TYPE_FORMAT_DATA_RESPONSE.to_le_bytes()[..]);
            if (last || legacy) && chunk != declared {
                return Err("incomplete clipboard channel message");
            }
            if last || legacy {
                return Ok(ChunkVerdict::Dropped {
                    declared,
                    substitute: format_data.then(failed_format_data_response),
                });
            }
            if chunk == 0 {
                return Err("empty clipboard channel fragment");
            }
            self.discarding = Some(Discarding {
                declared,
                received: chunk,
                format_data,
            });
            return Ok(ChunkVerdict::Discard);
        }
        let previous = match (first, self.pending) {
            (true, None) => 0,
            (true, Some(_)) => return Err("overlapping clipboard channel messages"),
            (false, Some((length, received))) if length == declared => received,
            // Preserve the pinned layer's legacy unfragmented representation.
            (false, None) if !last && chunk == declared => return Ok(ChunkVerdict::Accept),
            _ => return Err("clipboard channel fragment has no matching first fragment"),
        };
        let received = previous
            .checked_add(chunk)
            .ok_or("clipboard channel size overflow")?;
        if received > declared {
            return Err("clipboard channel fragments exceed the declared length");
        }
        if last {
            if received != declared {
                return Err("incomplete clipboard channel message");
            }
            self.pending = None;
        } else {
            if chunk == 0 {
                return Err("empty clipboard channel fragment");
            }
            self.pending = Some((declared, received));
        }
        Ok(ChunkVerdict::Accept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_core::impl_as_any;
    use ironrdp_pdu::{PduResult, gcc::ChannelName};
    use ironrdp_svc::{StaticVirtualChannel, SvcMessage, SvcProcessor};
    use std::sync::{Arc, Mutex};

    fn fragment(length: usize, flags: u32, data: &[u8]) -> Vec<u8> {
        let mut wire = Vec::with_capacity(8 + data.len());
        wire.extend_from_slice(&(length as u32).to_le_bytes());
        wire.extend_from_slice(&flags.to_le_bytes());
        wire.extend_from_slice(data);
        wire
    }

    #[derive(Debug)]
    struct Sink(Arc<Mutex<Option<Vec<u8>>>>);
    impl_as_any!(Sink);
    impl SvcProcessor for Sink {
        fn channel_name(&self) -> ChannelName {
            ChannelName::from_static(b"cliprdr\0")
        }
        fn process(&mut self, payload: &[u8]) -> PduResult<Vec<SvcMessage>> {
            *self.0.lock().unwrap() = Some(payload.to_vec());
            Ok(Vec::new())
        }
    }

    #[test]
    fn maximum_unicode_newline_response_survives_real_svc_fragmentation() {
        let text = "\r\n".repeat(1024 * 1024);
        let pdu = ClipboardPdu::FormatDataResponse(FormatDataResponse::new_unicode_string(&text));
        let encoded = encode_vec(&pdu).unwrap();
        assert!(encoded.len() <= MAX_CLIPBOARD_PDU_BYTES);
        let received = Arc::new(Mutex::new(None));
        let mut channel = StaticVirtualChannel::new(Sink(received.clone()));
        let mut guard = ClipboardChunkGuard::default();
        let chunks = encoded.chunks(1600);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            let flags = u32::from(index == 0) | (u32::from(index + 1 == count) << 1) | 0x10;
            let wire = fragment(encoded.len(), flags, chunk);
            assert_eq!(guard.accept(&wire), Ok(ChunkVerdict::Accept));
            channel.process(&wire).unwrap();
            if index + 1 != count {
                assert!(received.lock().unwrap().is_none());
            }
        }
        let wire = received.lock().unwrap().take().unwrap();
        assert_eq!(wire, encoded);
        let ClipboardPdu::FormatDataResponse(decoded) = decode::<ClipboardPdu<'_>>(&wire).unwrap()
        else {
            panic!("wrong clipboard PDU");
        };
        assert_eq!(
            decoded.data(),
            FormatDataResponse::new_unicode_string(&text).data()
        );
        assert!(guard.pending.is_none());
    }

    #[test]
    fn undersized_declarations_fail_and_oversize_first_fragments_are_skipped() {
        let mut guard = ClipboardChunkGuard::default();
        for declared in [0, 7] {
            assert!(guard.accept(&fragment(declared, 1, &[0; 8])).is_err());
            assert!(guard.pending.is_none());
        }
        assert!(guard.accept(&[0; 7]).is_err());
        for declared in [MAX_CLIPBOARD_PDU_BYTES + 1, u32::MAX as usize] {
            let mut guard = ClipboardChunkGuard::default();
            assert_eq!(
                guard.accept(&fragment(declared, 1, &[0; 8])),
                Ok(ChunkVerdict::Discard)
            );
            assert!(guard.pending.is_none() && guard.discarding.is_some());
            // A skipped message must still be a well-formed sequence.
            assert!(guard.accept(&fragment(declared, 1, &[0; 8])).is_err());
            assert!(guard.discarding.is_none());
            assert!(guard.accept(&fragment(declared, 2, &[0; 8])).is_err());
        }
    }

    /// Splits one encoded CLIPRDR PDU as an RDP client does and handles each
    /// chunk as the server does: guard first, then the real SVC layer.
    fn deliver(
        guard: &mut ClipboardChunkGuard,
        channel: &mut StaticVirtualChannel,
        encoded: &[u8],
    ) -> Vec<ChunkVerdict> {
        let chunks = encoded.chunks(1600);
        let count = chunks.len();
        let mut verdicts = Vec::with_capacity(count);
        for (index, chunk) in chunks.enumerate() {
            let flags = u32::from(index == 0) | (u32::from(index + 1 == count) << 1) | 0x10;
            let wire = fragment(encoded.len(), flags, chunk);
            let verdict = guard.accept(&wire).unwrap();
            match &verdict {
                ChunkVerdict::Accept => drop(channel.process(&wire).unwrap()),
                ChunkVerdict::Dropped {
                    substitute: Some(substitute),
                    ..
                } => drop(channel.process(substitute).unwrap()),
                ChunkVerdict::Discard | ChunkVerdict::Dropped { .. } => {}
            }
            verdicts.push(verdict);
        }
        verdicts
    }

    fn format_data(wire: &[u8]) -> FormatDataResponse<'_> {
        let ClipboardPdu::FormatDataResponse(decoded) = decode::<ClipboardPdu<'_>>(wire).unwrap()
        else {
            panic!("wrong clipboard PDU");
        };
        decoded
    }

    #[test]
    fn nine_megabyte_dib_response_is_accepted_under_the_image_bound() {
        // A 1920x1080 32-bit CF_DIB is 8,294,440 bytes; round up past it.
        let dib: Vec<u8> = (0..9_000_000u32).map(|i| (i % 251) as u8).collect();
        let encoded = encode_vec(&ClipboardPdu::FormatDataResponse(
            FormatDataResponse::new_data(dib.as_slice()),
        ))
        .unwrap();
        let received = Arc::new(Mutex::new(None));
        let mut channel = StaticVirtualChannel::new(Sink(received.clone()));
        let mut guard = ClipboardChunkGuard::default();
        let verdicts = deliver(&mut guard, &mut channel, &encoded);
        assert!(verdicts.iter().all(|v| *v == ChunkVerdict::Accept));
        let wire = received.lock().unwrap().take().unwrap();
        assert_eq!(format_data(&wire).data(), dib.as_slice());
        assert!(guard.pending.is_none() && guard.discarding.is_none());
    }

    #[test]
    fn oversize_messages_are_skipped_and_the_channel_continues() {
        let received = Arc::new(Mutex::new(None));
        let mut channel = StaticVirtualChannel::new(Sink(received.clone()));
        let mut guard = ClipboardChunkGuard::default();
        // One byte over: a Format Data Response becomes a failed response,
        // so the paste request it answered is still answered in order.
        let oversize = vec![7u8; MAX_CLIPBOARD_PDU_BYTES - 8 + 1];
        let encoded = encode_vec(&ClipboardPdu::FormatDataResponse(
            FormatDataResponse::new_data(oversize.as_slice()),
        ))
        .unwrap();
        assert_eq!(encoded.len(), MAX_CLIPBOARD_PDU_BYTES + 1);
        let verdicts = deliver(&mut guard, &mut channel, &encoded);
        let (last, skipped) = verdicts.split_last().unwrap();
        assert!(skipped.iter().all(|v| *v == ChunkVerdict::Discard));
        assert!(matches!(
            last,
            ChunkVerdict::Dropped { declared, substitute: Some(_) } if *declared == encoded.len()
        ));
        let wire = received.lock().unwrap().take().unwrap();
        let substitute = format_data(&wire);
        assert!(substitute.is_error() && substitute.data().is_empty());
        assert!(guard.pending.is_none() && guard.discarding.is_none());
        // Any other oversize message is skipped without a reply.
        let mut format_list = vec![2u8, 0, 0, 0];
        format_list.extend_from_slice(&((MAX_CLIPBOARD_PDU_BYTES - 7) as u32).to_le_bytes());
        format_list.resize(MAX_CLIPBOARD_PDU_BYTES + 1, 0);
        let verdicts = deliver(&mut guard, &mut channel, &format_list);
        assert!(matches!(
            verdicts.last(),
            Some(ChunkVerdict::Dropped {
                substitute: None,
                ..
            })
        ));
        assert!(received.lock().unwrap().is_none());
        // The channel keeps working afterwards.
        let text = encode_vec(&ClipboardPdu::FormatDataResponse(
            FormatDataResponse::new_unicode_string("after"),
        ))
        .unwrap();
        let verdicts = deliver(&mut guard, &mut channel, &text);
        assert_eq!(verdicts, [ChunkVerdict::Accept]);
        let wire = received.lock().unwrap().take().unwrap();
        assert_eq!(wire, text);
        // A skipped message whose fragments overrun is still a fatal error.
        let mut guard = ClipboardChunkGuard::default();
        let declared = MAX_CLIPBOARD_PDU_BYTES + 1;
        guard.accept(&fragment(declared, 1, &[5, 0])).unwrap();
        guard
            .accept(&fragment(declared, 0, &vec![0; declared - 3]))
            .unwrap();
        assert!(guard.accept(&fragment(declared, 2, &[0; 2])).is_err());
        assert!(guard.discarding.is_none());
    }

    #[test]
    fn actual_fragment_bytes_cannot_exceed_declared_or_absolute_bounds() {
        let mut guard = ClipboardChunkGuard::default();
        guard.accept(&fragment(8, 1, &[0; 7])).unwrap();
        assert!(guard.accept(&fragment(8, 2, &[0; 2])).is_err());
        assert!(guard.pending.is_none());
        assert!(guard.accept(&fragment(8, 3, &[0; 9])).is_err());
        assert!(
            guard
                .accept(&fragment(
                    MAX_CLIPBOARD_PDU_BYTES,
                    3,
                    &vec![0; MAX_CLIPBOARD_PDU_BYTES + 1]
                ))
                .is_err()
        );
    }

    #[test]
    fn malformed_sequences_and_compression_cannot_bypass_bounds() {
        for first in [0, 2] {
            let mut guard = ClipboardChunkGuard::default();
            assert!(guard.accept(&fragment(9, first, &[0; 8])).is_err());
        }
        for next in [
            fragment(9, 1, &[0]),           // overlapping FIRST
            fragment(10, 2, &[0]),          // changed length
            fragment(9, 2, &[]),            // short LAST
            fragment(9, 0, &[]),            // no progress
            fragment(0, 2, &[0]),           // zero-length header
            fragment(9, 0x0020_0002, &[0]), // compressed length-check bypass
        ] {
            let mut guard = ClipboardChunkGuard::default();
            guard.accept(&fragment(9, 1, &[0; 8])).unwrap();
            assert!(guard.accept(&next).is_err());
            assert!(guard.pending.is_none());
        }
        let mut guard = ClipboardChunkGuard::default();
        assert!(guard.accept(&fragment(8, 0x0020_0003, &[0; 8])).is_err());
    }

    #[test]
    fn complete_and_legacy_unfragmented_messages_start_fresh_each_time() {
        let mut guard = ClipboardChunkGuard::default();
        for flags in [3, 0, 3] {
            guard.accept(&fragment(8, flags, &[0; 8])).unwrap();
            assert!(guard.pending.is_none());
        }
        guard.accept(&fragment(8, 1, &[0; 8])).unwrap();
        guard.accept(&fragment(8, 2, &[])).unwrap();
        assert!(guard.pending.is_none());
    }
}

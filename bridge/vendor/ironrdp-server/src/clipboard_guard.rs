//! Bound CLIPRDR before the pinned SVC layer allocates its reassembly buffer.
use ironrdp_core::decode;
use ironrdp_pdu::rdp::vc::{ChannelControlFlags, ChannelPduHeader};

// One MiB of UTF-8 LF text expands to four MiB of UTF-16 CRLF, followed
// by its NUL terminator and the eight-byte CLIPRDR message header.
pub(crate) const MAX_CLIPBOARD_PDU_BYTES: usize = 4 * 1024 * 1024 + 2 + 8;

#[derive(Default, Debug)]
pub(crate) struct ClipboardChunkGuard {
    pending: Option<(usize, usize)>,
}

impl ClipboardChunkGuard {
    pub(crate) fn accept(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        let result = self.validate(bytes);
        if result.is_err() {
            self.pending = None;
        }
        result
    }

    fn validate(&mut self, bytes: &[u8]) -> Result<(), &'static str> {
        let header =
            decode::<ChannelPduHeader>(bytes).map_err(|_| "invalid clipboard channel header")?;
        let declared = header.length as usize;
        if !(8..=MAX_CLIPBOARD_PDU_BYTES).contains(&declared) {
            return Err("clipboard channel message exceeds the text transfer bound");
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
        let previous = match (first, self.pending) {
            (true, None) => 0,
            (true, Some(_)) => return Err("overlapping clipboard channel messages"),
            (false, Some((length, received))) if length == declared => received,
            // Preserve the pinned layer's legacy unfragmented representation.
            (false, None) if !last && chunk == declared => return Ok(()),
            _ => return Err("clipboard channel fragment has no matching first fragment"),
        };
        let received = previous
            .checked_add(chunk)
            .ok_or("clipboard channel size overflow")?;
        if received > declared || received > MAX_CLIPBOARD_PDU_BYTES {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironrdp_cliprdr::pdu::{ClipboardPdu, FormatDataResponse};
    use ironrdp_core::{encode_vec, impl_as_any};
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
        assert_eq!(encoded.len(), MAX_CLIPBOARD_PDU_BYTES);
        let received = Arc::new(Mutex::new(None));
        let mut channel = StaticVirtualChannel::new(Sink(received.clone()));
        let mut guard = ClipboardChunkGuard::default();
        let chunks = encoded.chunks(1600);
        let count = chunks.len();
        for (index, chunk) in chunks.enumerate() {
            let flags = u32::from(index == 0) | (u32::from(index + 1 == count) << 1) | 0x10;
            let wire = fragment(encoded.len(), flags, chunk);
            guard.accept(&wire).unwrap();
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
    fn oversized_declared_messages_fail_before_any_svc_accumulation() {
        let mut guard = ClipboardChunkGuard::default();
        for declared in [MAX_CLIPBOARD_PDU_BYTES + 1, u32::MAX as usize, 0, 7] {
            assert!(guard.accept(&fragment(declared, 1, &[0; 8])).is_err());
            assert!(guard.pending.is_none());
        }
        assert!(guard.accept(&[0; 7]).is_err());
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

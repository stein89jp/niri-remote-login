//! Close and re-create the AUDIO_INPUT dynamic channel (MS-RDPEAI).
//!
//! MS-RDPEAI has no "stop recording" PDU: a server stops the client's capture
//! by closing the channel, as Windows RDS and GNOME Remote Desktop do when the
//! last local recorder goes away. The pinned DRDYNVC server forgets a
//! pre-registered channel's type once it is closed, so the replacement's id is
//! tracked by the caller and preferred over the type lookup.
//!
//! A streaming client keeps sending capture data until it processes the
//! Close, and the pinned DRDYNVC server treats data for a removed channel as a
//! fatal protocol error. [`ClosedChannels`] drops that late data until the
//! client's own Close for the channel arrives, so stopping the microphone can
//! never end the connection.
use std::collections::VecDeque;

use ironrdp_core::decode;
use ironrdp_dvc::DrdynvcServer;
use ironrdp_dvc::pdu::DrdynvcClientPdu;
use ironrdp_pdu::rdp::vc::{ChannelControlFlags, ChannelPduHeader};
use ironrdp_pdu::{PduError, PduResult};
use ironrdp_rdpeai::server::{RdpeaiServer, RdpeaiServerBackend};
use ironrdp_svc::SvcMessage;

/// Closed channels remembered while their Close Response is outstanding. A
/// client that never answers cannot grow the list.
const MAX_CLOSED_CHANNELS: usize = 8;

/// The current AUDIO_INPUT channel: the tracked replacement after a recycle,
/// otherwise the channel pre-registered at connection start.
pub(crate) fn current_channel(drdynvc: &DrdynvcServer, tracked: Option<u32>) -> Option<u32> {
    tracked.or_else(|| drdynvc.get_channel_id_by_type::<RdpeaiServer>())
}

/// DRDYNVC messages that close the open AUDIO_INPUT channel and request a fresh one.
pub(crate) struct Recycled {
    /// Close, then Create (unless creating it failed), in send order.
    pub(crate) messages: Vec<SvcMessage>,
    /// Id of the closed channel, whose late data must be dropped.
    pub(crate) closed: u32,
    /// Id of the replacement channel, which the caller tracks from now on.
    pub(crate) channel_id: Option<u32>,
}

/// Close the open AUDIO_INPUT channel, which runs the old processor's
/// `close` (and so the backend's `on_close`), then register a replacement
/// built from `build`. The replacement renegotiates Version and Formats when
/// the client accepts it, and then waits in `Ready` for the next Open.
///
/// Returns `None` without side effects when no AUDIO_INPUT channel is open.
/// The result is an error only when the replacement could not be encoded;
/// the Close is still returned and must still be sent.
pub(crate) fn recycle(
    drdynvc: &mut DrdynvcServer,
    tracked: Option<u32>,
    build: impl FnOnce() -> Box<dyn RdpeaiServerBackend>,
) -> Option<(Recycled, PduResult<()>)> {
    let closed = current_channel(drdynvc, tracked)?;
    if !drdynvc.is_channel_opened(closed) {
        return None;
    }
    let close = drdynvc.close_channel(closed)?;
    let mut replacement = None;
    let created = drdynvc.create_channel_with(|id| {
        replacement = Some(id);
        Ok::<_, PduError>(RdpeaiServer::new(build()))
    });
    let (messages, result) = match created {
        Ok(create) => (vec![close, create], Ok(())),
        Err(error) => {
            replacement = None;
            (vec![close], Err(error))
        }
    };
    Some((
        Recycled {
            messages,
            closed,
            channel_id: replacement,
        },
        result,
    ))
}

/// AUDIO_INPUT channels closed by [`recycle`] whose Close Response has not arrived.
#[derive(Debug, Default)]
pub(crate) struct ClosedChannels {
    ids: VecDeque<u32>,
}

impl ClosedChannels {
    pub(crate) fn insert(&mut self, channel_id: u32) {
        if self.ids.len() == MAX_CLOSED_CHANNELS {
            self.ids.pop_front();
        }
        self.ids.push_back(channel_id);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Whether one raw DRDYNVC channel chunk is late DVC data for a closed
    /// channel, which the caller drops instead of processing. The client's
    /// Close for such a channel ends the wait and is processed as usual.
    /// Anything that is not a single complete, uncompressed DRDYNVC PDU is
    /// left to the normal path.
    pub(crate) fn drop_late_data(&mut self, chunk: &[u8]) -> bool {
        if self.ids.is_empty() {
            return false;
        }
        let Ok(header) = decode::<ChannelPduHeader>(chunk) else {
            return false;
        };
        let single = ChannelControlFlags::FLAG_FIRST | ChannelControlFlags::FLAG_LAST;
        let Some(body) = chunk.get(8..) else {
            return false;
        };
        if !header.flags.contains(single)
            || header
                .flags
                .contains(ChannelControlFlags::PACKET_COMPRESSED)
            || usize::try_from(header.length).ok() != Some(body.len())
        {
            return false;
        }
        match decode::<DrdynvcClientPdu>(body) {
            Ok(DrdynvcClientPdu::Data(data)) => self.ids.contains(&data.channel_id()),
            Ok(DrdynvcClientPdu::Close(close)) => {
                self.ids.retain(|id| *id != close.channel_id());
                false
            }
            _ => false,
        }
    }
}

use ironrdp_cliprdr::backend::CliprdrBackendFactory;

use crate::ServerEventSender;

pub trait CliprdrServerFactory: CliprdrBackendFactory + ServerEventSender {
    /// A queued format-data response has been sent or discarded. Backends can
    /// bound their outstanding payloads without treating client ACKs as credits.
    fn on_format_data_response_drained(&self) {}
}

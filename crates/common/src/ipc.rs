//! Common types and utilities for IPC communication between the overlay client and server.

use asdf_overlay_event::OverlayEvent;
use bincode::{Decode, Encode};
use tokio::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::request::Request;

/// Creates an IPC address for the given process ID.
///
/// This function is used internally by `asdf-overlay-client` and
/// `asdf-overlay-dll` crates to establish IPC communication.
///
/// Note: an earlier version of this protocol also hashed in the DLL's
/// `HMODULE` to support multiple distinct overlays inside the same target
/// process. That was dropped because kernel anti-cheats (Vanguard) deny the
/// `PROCESS_VM_READ` access needed for the client-side injector to discover
/// the remote `HMODULE`. Only one overlay instance per target process is
/// supported.
pub fn create_ipc_addr(pid: u32) -> String {
    format!("\\\\.\\pipe\\asdf-overlay-{pid}")
}

/// Describes a request sent from the client to the server.
#[derive(Encode, Decode)]
pub struct ClientRequest {
    /// Unique identifier for matching responses.
    pub id: u32,

    /// The actual request data.
    pub req: Request,
}

/// Describes a response sent from server to client.
#[derive(Encode, Decode)]
pub struct ServerResponse {
    /// Unique identifier matching the request.
    pub id: u32,

    /// The raw response data.
    pub data: Vec<u8>,
}

/// Describes a packet sent from server to client.
#[derive(Encode, Decode)]
pub enum ServerToClientPacket {
    /// The packet is a response to a specific request.
    Response(ServerResponse),

    /// The packet is an event notification.
    Event(OverlayEvent),
}

/// Describes a frame header for IPC communication.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// Size of the frame body in bytes.
    pub size: u32,
}

impl Frame {
    /// Reads a frame header from the given async reader.
    pub async fn read(mut r: impl AsyncRead + Unpin) -> io::Result<Self> {
        Ok(Self {
            size: r.read_u32().await?,
        })
    }

    /// Writes the frame header to the given async writer.
    pub async fn write(self, mut w: impl AsyncWrite + Unpin) -> io::Result<()> {
        w.write_u32(self.size).await?;
        Ok(())
    }
}

mod frame;
mod preflight;
mod session;

pub mod wire {
    include!(concat!(env!("OUT_DIR"), "/corvid.protocol.v1.rs"));
}

pub use frame::{
    CONTROL_LIMIT, FRAME_PREAMBLE, Frame, FrameError, FrameReader, FrameRole, FrameWriter,
    RENDER_LIMIT, RenderFrame, decode_envelope,
};
pub use session::{
    COMMAND_TIMEOUT, PROTOCOL_MAJOR, PROTOCOL_MINOR, PeerRole, ProtocolFault, Session,
    accept_hello, check_hello_result, hello,
};

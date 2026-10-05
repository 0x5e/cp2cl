//! `aa_client` — the Android Auto protocol from the **phone's** side.
//!
//! A head unit (HU) is the display, the knobs and the speakers; the phone (MD)
//! is everything else. This module is the phone half, so cp2aa can be plugged
//! into a car and be treated as an Android handset:
//!
//! ```text
//! head unit ──USB(AOAP)──> us ──> the CarPlay receiver's frames
//!          <──video/audio──
//!          ──touch/keys───> us ──> HID reports back to the iPhone
//! ```
//!
//! It deliberately knows nothing about CarPlay. Callers hand it already-encoded
//! frames and it hands back decoded input events: there is no transcoding,
//! resampling or pixel work anywhere in here, which is the whole point of
//! bridging instead of decoding.
//!
//! # Where the code came from
//!
//! `consts.rs`, `frame.rs`, `proto.rs` and `av.rs` are derived from LIVI's
//! `livi-aa` crate (GPL-3.0), which implements the *head-unit* side of this
//! same protocol. The wire format is symmetric, so those four carry over with
//! only their `crate::` paths rewritten to `super::`. Only the pieces that flip
//! direction — TLS (the head unit is the client, we are the server), the
//! session, and USB (gadget rather than host) — have to be written fresh.
//!
//! # Protocol shape
//!
//! * Frames are `[ch][flags][len:u16 BE]`, see `frame`.
//! * The head unit drives the handshake and we answer it, see `session`.
//! * Media travels as `[msgId:u16 BE][timestamp:u64 BE][data]`, see `av`.
//! * Input arrives on `CH_INPUT` as `InputReport`, see `input`.
//! * Control messages are protobuf, hand-encoded in `control`.

// Nothing calls in here yet: the USB gadget and the TLS server are still to be
// written, and until they exist every type below is unused by definition.
#![allow(dead_code)]

pub mod av;
pub mod consts;
pub mod control;
pub mod frame;
pub mod input;
pub mod proto;
pub mod session;
pub mod tls;
// The USB gadget is the only transport a head unit will accept, and only Linux
// has one to offer. TCP (`CP2AA_AA_TCP_PORT`) is the cross-platform stand-in.
#[cfg(target_os = "linux")]
pub mod usb;

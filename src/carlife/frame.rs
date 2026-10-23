//! The two framings between the car and the CarLife messages on top of them.
//!
//! The car and this device are two USB endpoints, not two sockets: there is
//! nothing to connect to and no stream to open. Every lump the host sends is
//! length framed, and the length is read before anything else, so a reader that
//! stops halfway cannot desynchronise the next message. That outer header is
//! eight bytes, big endian:
//!
//! ```text
//! [4B channelId][4B length][ CarLife message ]
//! ```
//!
//! `length` covers the CarLife message — its own header and its body — and not
//! these eight bytes.
//!
//! The CarLife message header is either eight or twelve bytes, also big endian.
//! Which one it is depends on the channel, and so does the cap on how large a
//! body may be:
//!
//! ```text
//! cmd, touch            [2B bodyLen][2B reserved][4B serviceType]      cmd 40K, touch 10K
//! video, media, tts, vr [4B bodyLen][4B timestamp][4B serviceType]     video 600K, others 50K
//! ```
//!
//! The bodies are protobuf, except on the video channel, where the body is what
//! the encoder produced.
//!
//! Every number below is the one the reference implementations use:
//! CarLifeVehicleLib's `CTranRecvPackageProcess.h` (`CMD_HEAD_LEN` and friends,
//! `E_CHANNEL_TYPE`, `EPackageHeadType`) and CarLifePort's `link/bridge.c`, which
//! is the same set read off a working car.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Which of the six multiplexed channels a chunk belongs to. They are all
/// carried on one bulk pipe; this is the only thing that tells them apart.
pub const CMD: u32 = 1;
pub const VIDEO: u32 = 2;
pub const MEDIA: u32 = 3;
pub const TTS: u32 = 4;
pub const VR: u32 = 5;
pub const TOUCH: u32 = 6;

/// The outer header, the same for every channel.
pub const OUTER_HEAD: usize = 8;

/// The size of one read, and of the buffer it fills.
///
/// A read on a FunctionFS bulk endpoint becomes one `usb_request` of that size.
/// The controller fills a request with whole packets and drops the rest when it
/// acknowledges the packet. So an eight-byte read for an outer header loses the
/// body that arrived in the same packet. At this size a read always covers a
/// whole packet, and the host decides where the stream is cut.
pub const READ_CHUNK: usize = 16 * 1024;

/// The largest CarLife message that may follow it. The reference uses 640 KB,
/// which is the video body's 600 KB plus its header and room to spare.
pub const MAX_MESSAGE: u32 = 640 * 1024;

/// The whole message, header and body, on its way somewhere.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub channel: u32,
    pub body: Vec<u8>,
}

/// One decoded message: the header split off, the body left as protobuf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The full 32-bit value, channel prefix included. Which channel it names
    /// is already known from the chunk it came in on, so it is kept whole.
    pub service_type: u32,
    /// Only the media channels' header carries one. The command channel's has a
    /// reserved half-word there instead, which the reference sometimes leaves
    /// unfilled — nothing here reads it.
    pub timestamp_ms: Option<u32>,
    pub body: Vec<u8>,
}

/// How long the CarLife header is on this channel.
pub fn head_len(channel: u32) -> usize {
    match channel {
        CMD | TOUCH => 8,
        _ => 12,
    }
}

/// How large a body may be on this channel, from the reference's caps.
pub fn max_body(channel: u32) -> u32 {
    match channel {
        CMD => 40 * 1024,
        VIDEO => 600 * 1024,
        MEDIA => 100 * 1024,
        TTS | VR => 50 * 1024,
        TOUCH => 10 * 1024,
        _ => 0,
    }
}

/// Whether the car sent this, as opposed to us.
///
/// The low half-word's top bit is the direction. Two of the message numbers
/// break the rule — gear and the car-data subscription acknowledgement — and are
/// sent by the car without it, a leftover in the library rather than a second
/// convention. They are listed by name so the exception stays visible.
pub fn from_car(service_type: u32) -> bool {
    const SENT_BY_CAR_DESPITE_THE_BIT: [u32; 2] = [0x0001_0029, 0x0001_0032];
    SENT_BY_CAR_DESPITE_THE_BIT.contains(&service_type) || service_type & 0x8000 != 0
}

/// The eight bytes that precede a message, addressed to one channel.
pub fn outer_head(channel: u32, length: usize) -> [u8; OUTER_HEAD] {
    let mut head = [0u8; OUTER_HEAD];
    head[0..4].copy_from_slice(&channel.to_be_bytes());
    head[4..8].copy_from_slice(&(length as u32).to_be_bytes());
    head
}

/// Reads one chunk, waiting for all of it.
///
/// `Ok(None)` means the far end went away between chunks, which is the only
/// clean way this ends; a short read inside a chunk is an error, because there
/// is no way to tell a truncated message from a desynchronised pipe.
pub async fn read_chunk<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Chunk>> {
    let mut head = [0u8; OUTER_HEAD];
    match fill(reader, &mut head).await? {
        0 => return Ok(None),
        n if n < OUTER_HEAD => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => {}
    }

    let channel = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
    let length = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let head_len = head_len(channel);
    if !(1..=TOUCH).contains(&channel)
        || length < head_len
        || length as u32 > MAX_MESSAGE
        || (length - head_len) as u32 > max_body(channel)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("channel {channel:#x} with a length of {length}"),
        ));
    }

    let mut body = vec![0u8; length];
    if fill(reader, &mut body).await? < length {
        return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
    }
    Ok(Some(Chunk { channel, body }))
}

/// Writes one message, header first.
///
/// The two writes are on purpose: the header is what lets the other end start
/// looking at the body, and a video frame can be six hundred kilobytes. The
/// writer below this is what has to keep any single transfer under the endpoint's
/// limit.
pub async fn write_chunk<W: AsyncWrite + Unpin>(
    writer: &mut W,
    channel: u32,
    message: &[u8],
) -> io::Result<()> {
    writer.write_all(&outer_head(channel, message.len())).await?;
    writer.write_all(message).await?;
    writer.flush().await
}

/// Builds a message: its channel's header, then the body.
pub fn encode(channel: u32, service_type: u32, timestamp_ms: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(head_len(channel) + body.len());
    match head_len(channel) {
        8 => {
            out.extend_from_slice(&(body.len() as u16).to_be_bytes());
            // The reserved half-word is filled with zero on the way out, which
            // is what the reference does; nothing reads it.
            out.extend_from_slice(&[0, 0]);
        }
        _ => {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend_from_slice(&timestamp_ms.to_be_bytes());
        }
    }
    out.extend_from_slice(&service_type.to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// Splits a message back into its header and body.
///
/// `None` means the header describes a body that is not all there, or a body
/// larger than the channel allows — either way the chunk is not usable.
pub fn decode(channel: u32, message: &[u8]) -> Option<Message> {
    let head_len = head_len(channel);
    if message.len() < head_len {
        return None;
    }
    let (body_len, timestamp_ms) = match head_len {
        8 => (u16::from_be_bytes([message[0], message[1]]) as usize, None),
        _ => (
            u32::from_be_bytes([message[0], message[1], message[2], message[3]]) as usize,
            Some(u32::from_be_bytes([
                message[4], message[5], message[6], message[7],
            ])),
        ),
    };
    if body_len > max_body(channel) as usize || head_len + body_len > message.len() {
        return None;
    }
    let service_type = u32::from_be_bytes([
        message[head_len - 4],
        message[head_len - 3],
        message[head_len - 2],
        message[head_len - 1],
    ]);
    Some(Message {
        service_type,
        timestamp_ms,
        body: message[head_len..head_len + body_len].to_vec(),
    })
}

/// Fills `buf`, returning how much arrived before the far end stopped.
async fn fill<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = reader.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use tokio::io::{BufReader, ReadBuf};

    #[test]
    fn the_command_channel_has_an_eight_byte_header_and_a_sixteen_bit_length() {
        let message = encode(CMD, 0x0001_0002, 0, &[0x08, 0x01]);
        assert_eq!(message.len(), 8 + 2);
        assert_eq!(&message[..2], &[0x00, 0x02]);
        assert_eq!(&message[2..4], &[0x00, 0x00]);
        assert_eq!(&message[4..8], &0x0001_0002u32.to_be_bytes());
    }

    #[test]
    fn the_video_channel_has_a_twelve_byte_header_and_a_thirty_two_bit_length() {
        let message = encode(VIDEO, 0x0002_0001, 1234, &[1, 2, 3]);
        assert_eq!(message.len(), 12 + 3);
        assert_eq!(&message[..4], &3u32.to_be_bytes());
        assert_eq!(&message[4..8], &1234u32.to_be_bytes());
        assert_eq!(&message[8..12], &0x0002_0001u32.to_be_bytes());
    }

    #[test]
    fn a_message_survives_a_round_trip_with_its_timestamp() {
        let message = encode(VIDEO, 0x0002_0002, 99, &[]);
        let decoded = decode(VIDEO, &message).expect("it decodes");
        assert_eq!(decoded.timestamp_ms, Some(99));
        assert_eq!(decoded.service_type, 0x0002_0002);
        assert!(decoded.body.is_empty());

        let message = encode(CMD, 0x0001_0051, 0, &[]);
        let decoded = decode(CMD, &message).expect("it decodes");
        assert_eq!(decoded.timestamp_ms, None);
        assert_eq!(decoded.service_type, 0x0001_0051);
    }

    #[test]
    fn the_timestamp_is_ignored_on_the_command_channel_but_still_parsed() {
        // The reserved half-word is not always filled by the other end; nothing
        // here reads it, so a non-zero value must not upset the body length.
        let mut message = encode(CMD, 0x0001_0004, 0, &[7]);
        message[2] = 0xff;
        let decoded = decode(CMD, &message).expect("it decodes");
        assert_eq!(decoded.body, vec![7]);
    }

    #[test]
    fn the_car_is_the_end_whose_messages_carry_the_top_bit() {
        assert!(from_car(0x0001_8001));
        assert!(!from_car(0x0001_0002));
        // Sent by the car with the bit clear, per the library's own exception.
        assert!(from_car(0x0001_0029));
        assert!(from_car(0x0001_0032));
    }

    #[tokio::test]
    async fn a_chunk_arrives_whole_when_the_pipe_hands_it_over_in_pieces() {
        let (mut car, mut device) = tokio::io::duplex(64);
        let message = encode(CMD, 0x0001_0002, 0, &[8, 1]);
        let head = outer_head(CMD, message.len());

        let writer = tokio::spawn(async move {
            // A byte at a time, the way a bulk transfer does not, but a reader
            // that only ever got one piece must still assemble the whole thing.
            for byte in head.iter().chain(message.iter()) {
                car.write_all(&[*byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            drop(car);
        });

        let chunk = read_chunk(&mut device).await.unwrap().expect("a chunk");
        assert_eq!(chunk.channel, CMD);
        assert_eq!(decode(CMD, &chunk.body).unwrap().service_type, 0x0001_0002);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn two_chunks_in_one_write_are_read_apart() {
        let (mut car, mut device) = tokio::io::duplex(64);
        let mut both = Vec::new();
        for service_type in [0x0001_8001u32, 0x0001_8003] {
            let message = encode(CMD, service_type, 0, &[]);
            both.extend_from_slice(&outer_head(CMD, message.len()));
            both.extend_from_slice(&message);
        }
        car.write_all(&both).await.unwrap();

        for expected in [0x0001_8001u32, 0x0001_8003] {
            let chunk = read_chunk(&mut device).await.unwrap().expect("a chunk");
            assert_eq!(decode(CMD, &chunk.body).unwrap().service_type, expected);
        }
    }

    #[tokio::test]
    async fn a_truncated_chunk_is_an_error_rather_than_a_short_message() {
        let (mut car, mut device) = tokio::io::duplex(64);
        let message = encode(CMD, 0x0001_0002, 0, &[8, 1]);
        let head = outer_head(CMD, message.len());
        car.write_all(&head).await.unwrap();
        car.write_all(&message[..3]).await.unwrap();
        drop(car);

        let error = read_chunk(&mut device).await.expect_err("an error");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_channel_that_does_not_exist_is_refused_before_anything_is_read() {
        let (mut car, mut device) = tokio::io::duplex(64);
        car.write_all(&outer_head(9, 12)).await.unwrap();
        let error = read_chunk(&mut device).await.expect_err("an error");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn an_empty_pipe_is_the_end_and_not_an_error() {
        let (car, mut device) = tokio::io::duplex(64);
        drop(car);
        assert!(read_chunk(&mut device).await.unwrap().is_none());
    }

    /// A reader that behaves like the controller: it delivers as much of a
    /// packet as the request has room for, and drops the rest.
    struct Packets {
        queue: VecDeque<Vec<u8>>,
        asked: Vec<usize>,
    }

    impl AsyncRead for Packets {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let room = buf.remaining();
            self.asked.push(room);
            let Some(packet) = self.queue.pop_front() else {
                return Poll::Ready(Ok(()));
            };
            let taken = room.min(packet.len());
            buf.put_slice(&packet[..taken]);
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn a_frame_survives_a_controller_that_drops_what_does_not_fit() {
        // Header and body in one packet, as a host that buffers both writes
        // sends them. Read eight bytes at a time, the controller delivers the
        // header and drops the body with the packet. The bytes that follow then
        // decode as the next frame's header, and everything after is eight
        // bytes out of step.
        let message = encode(CMD, 0x0001_8001, 0, &[0x08, 0x03, 0x10, 0x00]);
        let mut packet = outer_head(CMD, message.len()).to_vec();
        packet.extend_from_slice(&message);

        let mut device = BufReader::with_capacity(
            READ_CHUNK,
            Packets {
                queue: VecDeque::from([packet]),
                asked: Vec::new(),
            },
        );

        let chunk = read_chunk(&mut device).await.unwrap().expect("a chunk");
        assert_eq!(chunk.channel, CMD);
        assert_eq!(chunk.body, message);

        // No read was smaller than a packet, so there was never a remainder to
        // drop.
        let asked = &device.get_ref().asked;
        assert!(
            asked.iter().all(|&room| room >= 512),
            "every read asks for at least a packet: {asked:?}"
        );
    }
}

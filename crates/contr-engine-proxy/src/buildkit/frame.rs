//! HTTP/2 frames (RFC 9113, section 4), read and written whole.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(super) const DATA: u8 = 0x0;
pub(super) const HEADERS: u8 = 0x1;
pub(super) const RST_STREAM: u8 = 0x3;
pub(super) const SETTINGS: u8 = 0x4;
pub(super) const WINDOW_UPDATE: u8 = 0x8;
pub(super) const CONTINUATION: u8 = 0x9;

pub(super) const END_STREAM: u8 = 0x1;
pub(super) const ACK: u8 = 0x1;
pub(super) const END_HEADERS: u8 = 0x4;
pub(super) const PADDED: u8 = 0x8;
pub(super) const PRIORITY: u8 = 0x20;

/// Largest payload every peer accepts, whatever its `SETTINGS_MAX_FRAME_SIZE`.
const UNIVERSAL_MAX_PAYLOAD: usize = 16_384;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    pub(super) kind: u8,
    pub(super) flags: u8,
    pub(super) stream: u32,
    pub(super) payload: Vec<u8>,
}

impl Frame {
    /// The next frame, or `None` when the input ends between frames.
    pub(super) async fn read<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<Option<Self>> {
        let mut header = [0; 9];
        if reader.read(&mut header[..1]).await? == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut header[1..]).await?;
        let [l0, l1, l2, kind, flags, s0, s1, s2, s3] = header;
        let length = usize::try_from(u32::from_be_bytes([0, l0, l1, l2])).map_err(io::Error::other)?;
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload).await?;

        Ok(Some(Self {
            kind,
            flags,
            stream: u32::from_be_bytes([s0, s1, s2, s3]) & 0x7fff_ffff,
            payload,
        }))
    }

    pub(super) async fn write<W: AsyncWrite + Unpin>(&self, writer: &mut W) -> io::Result<()> {
        let length = u32::try_from(self.payload.len()).map_err(io::Error::other)?;
        let mut bytes = Vec::with_capacity(9 + self.payload.len());
        bytes.extend_from_slice(&length.to_be_bytes()[1..]);
        bytes.extend_from_slice(&[self.kind, self.flags]);
        bytes.extend_from_slice(&self.stream.to_be_bytes());
        bytes.extend_from_slice(&self.payload);
        writer.write_all(&bytes).await?;
        writer.flush().await
    }

    /// DATA frames that carry `body` on `stream` and end it.
    pub(super) fn body(stream: u32, body: &[u8]) -> Vec<Self> {
        let chunks: Vec<&[u8]> = if body.is_empty() {
            vec![&[]]
        } else {
            body.chunks(UNIVERSAL_MAX_PAYLOAD).collect()
        };
        let last = chunks.len() - 1;

        chunks
            .into_iter()
            .enumerate()
            .map(|(index, chunk)| Self {
                kind: DATA,
                flags: if index == last { END_STREAM } else { 0 },
                stream,
                payload: chunk.to_vec(),
            })
            .collect()
    }

    /// The header block fragment of a HEADERS or CONTINUATION frame, or `None` when its padding is malformed.
    pub(super) fn fragment(&self) -> Option<&[u8]> {
        if self.kind != HEADERS {
            return Some(&self.payload);
        }
        let mut payload = &self.payload[..];
        let mut padding = 0;
        if self.flags & PADDED != 0 {
            let (&length, rest) = payload.split_first()?;
            padding = usize::from(length);
            payload = rest;
        }
        if self.flags & PRIORITY != 0 {
            payload = payload.get(5..)?;
        }

        payload.get(..payload.len().checked_sub(padding)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trips_frames() {
        let frame = Frame {
            kind: WINDOW_UPDATE,
            flags: 0,
            stream: 3,
            payload: vec![0, 0, 1, 0],
        };
        let mut bytes = Vec::new();
        frame.write(&mut bytes).await.unwrap();

        assert_eq!(bytes, [0, 0, 4, 8, 0, 0, 0, 0, 3, 0, 0, 1, 0]);
        let mut reader = &bytes[..];
        assert_eq!(Frame::read(&mut reader).await.unwrap(), Some(frame));
        assert_eq!(Frame::read(&mut reader).await.unwrap(), None);
        assert!(Frame::read(&mut &bytes[..5]).await.is_err());
    }

    #[test]
    fn splits_bodies_into_frames_every_peer_accepts() {
        let frames = Frame::body(5, &vec![1; 20_000]);

        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].payload.len(), frames[0].flags), (16_384, 0));
        assert_eq!((frames[1].payload.len(), frames[1].flags), (3_616, END_STREAM));
        assert_eq!(
            Frame::body(5, &[]),
            [Frame {
                kind: DATA,
                flags: END_STREAM,
                stream: 5,
                payload: Vec::new(),
            }]
        );
    }

    #[test]
    fn finds_the_fragment_behind_padding_and_priority() {
        let frame = Frame {
            kind: HEADERS,
            flags: PADDED | PRIORITY | END_HEADERS,
            stream: 1,
            payload: vec![2, 0, 0, 0, 0, 16, 0x82, 0x86, 0, 0],
        };
        assert_eq!(frame.fragment(), Some(&[0x82, 0x86][..]));

        let overpadded = Frame {
            payload: vec![9, 0x82],
            flags: PADDED,
            ..frame
        };
        assert_eq!(overpadded.fragment(), None);
    }
}

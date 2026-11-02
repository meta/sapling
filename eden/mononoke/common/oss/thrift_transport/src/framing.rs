/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::io::ErrorKind;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use bytes::BufMut;
use bytes::Bytes;
use bytes::BytesMut;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

/// Largest frame either end will read or write.
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

const STREAM_TYPE_SUCCESS: u8 = 0;
const STREAM_TYPE_DECLARED_EX: u8 = 1;
const STREAM_TYPE_APP_EX: u8 = 2;
const STREAM_TYPE_EOS: u8 = 3;

/// A frame that follows the initial response of a streaming reply.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StreamFrame {
    Success(Bytes),
    DeclaredException(Bytes),
    ApplicationException(Bytes),
    EndOfStream,
}

impl StreamFrame {
    pub(crate) fn encode(&self) -> Result<Bytes> {
        let (frame_type, payload) = match self {
            Self::Success(payload) => (STREAM_TYPE_SUCCESS, payload.clone()),
            Self::DeclaredException(payload) => (STREAM_TYPE_DECLARED_EX, payload.clone()),
            Self::ApplicationException(payload) => (STREAM_TYPE_APP_EX, payload.clone()),
            Self::EndOfStream => (STREAM_TYPE_EOS, Bytes::new()),
        };
        let frame_len = payload
            .len()
            .checked_add(1)
            .context("stream frame length overflow")?;
        if frame_len > MAX_FRAME_SIZE {
            bail!("stream frame too large: {frame_len} bytes (max {MAX_FRAME_SIZE})");
        }

        let mut frame = BytesMut::with_capacity(frame_len);
        frame.put_u8(frame_type);
        frame.put_slice(&payload);
        Ok(frame.freeze())
    }

    pub(crate) fn decode(frame: Bytes) -> Result<Self> {
        let Some(&frame_type) = frame.first() else {
            bail!("empty stream frame");
        };
        let payload = frame.slice(1..);
        match frame_type {
            STREAM_TYPE_SUCCESS => Ok(Self::Success(payload)),
            STREAM_TYPE_DECLARED_EX => Ok(Self::DeclaredException(payload)),
            STREAM_TYPE_APP_EX => Ok(Self::ApplicationException(payload)),
            STREAM_TYPE_EOS if payload.is_empty() => Ok(Self::EndOfStream),
            STREAM_TYPE_EOS => bail!("end-of-stream frame contains a payload"),
            other => bail!("unknown stream frame type: {other}"),
        }
    }
}

/// Read one frame. Returns `None` if the peer closed the connection cleanly
/// before the frame began.
pub(crate) async fn read_frame<R>(reader: &mut R) -> Result<Option<Bytes>>
where
    R: AsyncRead + Unpin,
{
    let first = match reader.read_u8().await {
        Ok(byte) => byte,
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("failed to read frame length"),
    };
    let mut rest = [0_u8; 3];
    reader
        .read_exact(&mut rest)
        .await
        .context("connection closed during frame length")?;

    let frame_len = u32::from_be_bytes([first, rest[0], rest[1], rest[2]]) as usize;
    if frame_len == 0 {
        bail!("zero-length frame");
    }
    if frame_len > MAX_FRAME_SIZE {
        bail!("frame too large: {frame_len} bytes (max {MAX_FRAME_SIZE})");
    }

    let mut frame = vec![0_u8; frame_len];
    reader
        .read_exact(&mut frame)
        .await
        .context("connection closed during frame body")?;
    Ok(Some(Bytes::from(frame)))
}

pub(crate) async fn write_frame<W>(writer: &mut W, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.is_empty() {
        bail!("refusing to write a zero-length frame");
    }
    if payload.len() > MAX_FRAME_SIZE {
        bail!(
            "frame too large: {} bytes (max {MAX_FRAME_SIZE})",
            payload.len()
        );
    }
    let frame_len = u32::try_from(payload.len()).context("frame length does not fit in u32")?;

    writer
        .write_all(&frame_len.to_be_bytes())
        .await
        .context("failed to write frame length")?;
    writer
        .write_all(payload)
        .await
        .context("failed to write frame body")?;
    writer.flush().await.context("failed to flush frame")
}

#[cfg(test)]
mod tests {
    use mononoke_macros::mononoke;

    use super::*;

    #[mononoke::test]
    fn stream_frame_round_trip() {
        let frames = [
            StreamFrame::Success(Bytes::from_static(b"success")),
            StreamFrame::DeclaredException(Bytes::from_static(b"declared")),
            StreamFrame::ApplicationException(Bytes::from_static(b"application")),
            StreamFrame::EndOfStream,
        ];

        for frame in frames {
            let encoded = frame.encode().expect("frame should encode");
            let decoded = StreamFrame::decode(encoded).expect("frame should decode");
            assert_eq!(decoded, frame);
        }
    }

    #[mononoke::test]
    fn stream_frame_rejects_invalid_frames() {
        assert!(StreamFrame::decode(Bytes::new()).is_err());
        assert!(StreamFrame::decode(Bytes::from_static(&[STREAM_TYPE_EOS, 1])).is_err());
        assert!(StreamFrame::decode(Bytes::from_static(&[99])).is_err());
    }

    #[mononoke::test]
    async fn frame_round_trip() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, b"payload")
            .await
            .expect("frame should be written");
        assert_eq!(
            read_frame(&mut &buffer[..])
                .await
                .expect("frame should be read"),
            Some(Bytes::from_static(b"payload"))
        );
    }

    #[mononoke::test]
    async fn read_frame_rejects_partial_input() {
        let mut partial_length = &b"\0\0"[..];
        assert!(read_frame(&mut partial_length).await.is_err());

        let mut partial_body = &b"\0\0\0\x04ab"[..];
        assert!(read_frame(&mut partial_body).await.is_err());
    }

    #[mononoke::test]
    async fn read_frame_rejects_oversized_and_empty_frames() {
        let oversized = u32::try_from(MAX_FRAME_SIZE + 1)
            .expect("the frame limit fits in a u32")
            .to_be_bytes();
        assert!(read_frame(&mut &oversized[..]).await.is_err());
        assert!(read_frame(&mut &b"\0\0\0\0"[..]).await.is_err());
    }

    #[mononoke::test]
    async fn read_frame_distinguishes_clean_eof() {
        let mut empty = &b""[..];
        assert_eq!(read_frame(&mut empty).await.expect("EOF is readable"), None);
    }
}

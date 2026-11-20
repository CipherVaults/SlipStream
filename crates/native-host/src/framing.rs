use serde::{de::DeserializeOwned, Serialize};
use slipstream_protocol::MAX_NATIVE_MESSAGE_BYTES;
use std::io::{self, Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

fn check_length(length: usize) -> io::Result<usize> {
    if (1..=MAX_NATIVE_MESSAGE_BYTES).contains(&length) {
        Ok(length)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid frame length",
        ))
    }
}

fn decode<T: DeserializeOwned>(payload: &[u8]) -> io::Result<T> {
    serde_json::from_slice(payload)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid message JSON"))
}

#[derive(Default)]
struct BoundedPayload(Vec<u8>);

impl Write for BoundedPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_length(self.0.len().saturating_add(bytes.len()))?;
        // Limit Vec's capacity as well as the number of bytes written.
        if self.0.len() + bytes.len() > self.0.capacity() {
            let capacity = (self.0.capacity() * 2)
                .max(self.0.len() + bytes.len())
                .min(MAX_NATIVE_MESSAGE_BYTES);
            self.0.reserve_exact(capacity - self.0.len());
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode<T: Serialize>(message: &T) -> io::Result<([u8; 4], Vec<u8>)> {
    let mut payload = BoundedPayload::default();
    serde_json::to_writer(&mut payload, message).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cannot serialize message within frame limit",
        )
    })?;
    let length = check_length(payload.0.len())?;
    Ok(((length as u32).to_le_bytes(), payload.0))
}

/// A clean EOF before the next prefix returns None. A partial frame is an error.
pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<Option<T>> {
    let mut prefix = [0; 4];
    match reader.read_exact(&mut prefix[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    reader.read_exact(&mut prefix[1..])?;
    let mut payload = vec![0; check_length(u32::from_le_bytes(prefix) as usize)?];
    reader.read_exact(&mut payload)?;
    decode(&payload).map(Some)
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let (prefix, payload) = encode(message)?;
    writer.write_all(&prefix)?;
    writer.write_all(&payload)?;
    writer.flush()
}

pub async fn read_frame_async<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> io::Result<Option<T>> {
    let mut prefix = [0; 4];
    if reader.read(&mut prefix[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut prefix[1..]).await?;
    let mut payload = vec![0; check_length(u32::from_le_bytes(prefix) as usize)?];
    reader.read_exact(&mut payload).await?;
    decode(&payload).map(Some)
}

pub async fn write_frame_async<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> io::Result<()> {
    let (prefix, payload) = encode(message)?;
    writer.write_all(&prefix).await?;
    writer.write_all(&payload).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use slipstream_protocol::{NativeBridgeRequest, NativeBridgeResponse};

    #[test]
    fn validates_lengths_before_reading_payload() {
        for length in [0, MAX_NATIVE_MESSAGE_BYTES as u32 + 1] {
            let prefix = length.to_le_bytes();
            let mut input = prefix.as_slice();
            assert_eq!(
                read_frame::<NativeBridgeRequest>(&mut input)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
        for truncated in [&[1, 0][..], &[4, 0, 0, 0, b'{'][..]] {
            assert_eq!(
                read_frame::<NativeBridgeRequest>(&mut &*truncated)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::UnexpectedEof
            );
        }
        assert!(read_frame::<NativeBridgeRequest>(&mut &[][..])
            .unwrap()
            .is_none());
    }

    #[test]
    fn roundtrips_max_length_and_leaves_output_untouched_on_overflow() {
        let overhead = br#"{"type":"ack","ok":true,"message":""}"#.len();
        let message = "x".repeat(MAX_NATIVE_MESSAGE_BYTES - overhead);
        let mut frame = Vec::new();
        write_frame(
            &mut frame,
            &NativeBridgeResponse::Ack {
                ok: true,
                message: Some(message.clone()),
            },
        )
        .unwrap();
        assert_eq!(frame.len(), 4 + MAX_NATIVE_MESSAGE_BYTES);
        assert!(matches!(
            read_frame::<NativeBridgeResponse>(&mut frame.as_slice()).unwrap(),
            Some(NativeBridgeResponse::Ack { ok: true, message: Some(text) }) if text == message
        ));
        frame.clear();
        assert!(write_frame(
            &mut frame,
            &NativeBridgeResponse::Ack {
                ok: true,
                message: Some(message + "x")
            }
        )
        .is_err());
        assert!(frame.is_empty());
    }

    #[tokio::test]
    async fn async_request_roundtrip_and_typed_json_rejection() {
        let mut output = Vec::new();
        write_frame_async(&mut output, &NativeBridgeRequest::GetStatus)
            .await
            .unwrap();
        assert!(matches!(
            read_frame_async::<NativeBridgeRequest>(&mut output.as_slice())
                .await
                .unwrap(),
            Some(NativeBridgeRequest::GetStatus)
        ));
        for malformed in [
            br#"{"type":"unknown"}"#.as_slice(),
            br#"{"type":"authenticate","token":5}"#,
            b"\xff",
        ] {
            let mut frame = (malformed.len() as u32).to_le_bytes().to_vec();
            frame.extend_from_slice(malformed);
            assert_eq!(
                read_frame_async::<NativeBridgeRequest>(&mut frame.as_slice())
                    .await
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
}

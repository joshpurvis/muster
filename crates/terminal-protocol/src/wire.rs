//! Length-delimited JSON control and raw binary output. Limits apply before allocation.
use std::io::{Read, Write};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 16 * 1024 * 1024;
const JSON: u8 = 0;
const REPLAY: u8 = 1;
const OUTPUT: u8 = 2;
const PING: u8 = 3;

#[derive(Debug)]
pub enum Frame {
    Json(Vec<u8>),
    Replay { next_offset: u64, data: Vec<u8> },
    Output { offset: u64, data: Vec<u8> },
    Ping,
}

impl Frame {
    pub fn json(value: &impl serde::Serialize) -> anyhow::Result<Self> {
        Ok(Self::Json(serde_json::to_vec(value)?))
    }

    pub fn decode<T: serde::de::DeserializeOwned>(self) -> anyhow::Result<T> {
        match self {
            Self::Json(data) => Ok(serde_json::from_slice(&data)?),
            _ => anyhow::bail!("expected a JSON control frame"),
        }
    }

    fn parts(&self) -> (u8, Option<u64>, &[u8]) {
        match self {
            Self::Json(data) => (JSON, None, data),
            Self::Replay { next_offset, data } => (REPLAY, Some(*next_offset), data),
            Self::Output { offset, data } => (OUTPUT, Some(*offset), data),
            Self::Ping => (PING, None, &[]),
        }
    }

    fn header(&self) -> anyhow::Result<[u8; 5]> {
        let (kind, offset, data) = self.parts();
        let len = data.len() + if offset.is_some() { 8 } else { 0 };
        anyhow::ensure!(len <= MAX_FRAME, "frame exceeds {MAX_FRAME} bytes");
        let mut header = [0; 5];
        header[0] = kind;
        header[1..].copy_from_slice(&(len as u32).to_be_bytes());
        Ok(header)
    }
}

fn length(header: &[u8; 5]) -> anyhow::Result<usize> {
    anyhow::ensure!(header[0] <= PING, "unknown frame type");
    let len = u32::from_be_bytes(header[1..].try_into()?) as usize;
    anyhow::ensure!(len <= MAX_FRAME, "frame exceeds {MAX_FRAME} bytes");
    anyhow::ensure!(header[0] != PING || len == 0, "invalid ping");
    anyhow::ensure!(
        !matches!(header[0], REPLAY | OUTPUT) || len >= 8,
        "missing output offset"
    );
    Ok(len)
}

fn decode(kind: u8, mut data: Vec<u8>) -> anyhow::Result<Frame> {
    Ok(match kind {
        JSON => Frame::Json(data),
        REPLAY | OUTPUT => {
            let offset = u64::from_be_bytes(data[..8].try_into()?);
            data.drain(..8);
            if kind == REPLAY {
                Frame::Replay {
                    next_offset: offset,
                    data,
                }
            } else {
                Frame::Output { offset, data }
            }
        }
        PING => Frame::Ping,
        _ => anyhow::bail!("unknown frame type"),
    })
}

pub fn read(reader: &mut impl Read) -> anyhow::Result<Frame> {
    let mut header = [0; 5];
    reader.read_exact(&mut header)?;
    let mut data = vec![0; length(&header)?];
    reader.read_exact(&mut data)?;
    decode(header[0], data)
}

pub fn write(writer: &mut impl Write, frame: &Frame) -> anyhow::Result<()> {
    writer.write_all(&frame.header()?)?;
    let (_, offset, data) = frame.parts();
    if let Some(offset) = offset {
        writer.write_all(&offset.to_be_bytes())?;
    }
    writer.write_all(data)?;
    Ok(())
}

pub async fn read_async(reader: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Frame> {
    let mut header = [0; 5];
    reader.read_exact(&mut header).await?;
    let mut data = vec![0; length(&header)?];
    reader.read_exact(&mut data).await?;
    decode(header[0], data)
}

pub async fn write_async(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &Frame,
) -> anyhow::Result<()> {
    writer.write_all(&frame.header()?).await?;
    let (_, offset, data) = frame.parts();
    if let Some(offset) = offset {
        writer.write_all(&offset.to_be_bytes()).await?;
    }
    writer.write_all(data).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_frames_preserve_offsets_and_arbitrary_bytes() {
        let mut bytes = vec![];
        write(
            &mut bytes,
            &Frame::Replay {
                next_offset: 700,
                data: vec![0, 255, 27],
            },
        )
        .unwrap();
        write(
            &mut bytes,
            &Frame::Output {
                offset: 700,
                data: vec![10, 0],
            },
        )
        .unwrap();
        let mut reader = bytes.as_slice();
        assert!(
            matches!(read(&mut reader).unwrap(), Frame::Replay { next_offset: 700, data } if data == [0, 255, 27])
        );
        assert!(
            matches!(read(&mut reader).unwrap(), Frame::Output { offset: 700, data } if data == [10, 0])
        );
        assert!(reader.is_empty());
    }
    #[test]
    fn rejects_oversized_and_malformed_frames_before_reading_payload() {
        for header in [
            [0, 255, 255, 255, 255],
            [1, 0, 0, 0, 7],
            [3, 0, 0, 0, 1],
            [4, 0, 0, 0, 0],
        ] {
            let error = read(&mut header.as_slice()).unwrap_err().to_string();
            assert!(!error.contains("failed to fill"), "{error}");
        }
    }
}

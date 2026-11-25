//! Binary request-response codec for pulling media chunks.
//! Requests are tiny JSON; responses are a 1-byte tag followed by raw bytes
//! (no JSON, so a 1 MiB chunk costs ~1 MiB on the wire).

use async_trait::async_trait;
use futures::prelude::*;
use libp2p::{request_response::Codec, StreamProtocol};
use serde::{Deserialize, Serialize};

use crate::media::MEDIA_CHUNK_SIZE;

pub const MEDIA_PROTOCOL: &str = "/decentpaste/media/1.0.0";

const MAX_REQUEST_SIZE: u64 = 1024;
/// Encrypted chunk = nonce (12) + plaintext + tag (16); plus the tag byte and slack.
const MAX_RESPONSE_SIZE: u64 = MEDIA_CHUNK_SIZE as u64 + 1024;

const TAG_CHUNK: u8 = 0;
const TAG_NOT_FOUND: u8 = 1;
const TAG_DENIED: u8 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaChunkRequest {
    pub sha256: String,
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MediaChunkResponse {
    /// AES-GCM encrypted chunk (AAD = `chunk_aad(sha256, index)`).
    Chunk(Vec<u8>),
    NotFound,
    Denied,
}

impl MediaChunkResponse {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            MediaChunkResponse::Chunk(data) => {
                let mut out = Vec::with_capacity(data.len() + 1);
                out.push(TAG_CHUNK);
                out.extend_from_slice(data);
                out
            }
            MediaChunkResponse::NotFound => vec![TAG_NOT_FOUND],
            MediaChunkResponse::Denied => vec![TAG_DENIED],
        }
    }

    pub fn decode(mut bytes: Vec<u8>) -> std::io::Result<Self> {
        match bytes.first().copied() {
            Some(TAG_CHUNK) => {
                bytes.remove(0);
                Ok(MediaChunkResponse::Chunk(bytes))
            }
            Some(TAG_NOT_FOUND) => Ok(MediaChunkResponse::NotFound),
            Some(TAG_DENIED) => Ok(MediaChunkResponse::Denied),
            _ => Err(invalid("unknown media response tag")),
        }
    }
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

async fn read_limited<T>(io: &mut T, limit: u64) -> std::io::Result<Vec<u8>>
where
    T: AsyncRead + Unpin + Send,
{
    let mut buf = Vec::new();
    io.take(limit + 1).read_to_end(&mut buf).await?;
    if buf.len() as u64 > limit {
        return Err(invalid("media message exceeds size limit"));
    }
    Ok(buf)
}

#[derive(Debug, Clone, Default)]
pub struct MediaCodec;

#[async_trait]
impl Codec for MediaCodec {
    type Protocol = StreamProtocol;
    type Request = MediaChunkRequest;
    type Response = MediaChunkResponse;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        let buf = read_limited(io, MAX_REQUEST_SIZE).await?;
        serde_json::from_slice(&buf).map_err(|e| invalid(&e.to_string()))
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> std::io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        MediaChunkResponse::decode(read_limited(io, MAX_RESPONSE_SIZE).await?)
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        let bytes = serde_json::to_vec(&req).map_err(|e| invalid(&e.to_string()))?;
        io.write_all(&bytes).await?;
        io.close().await
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> std::io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        io.write_all(&res.encode()).await?;
        io.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_round_trip() {
        for res in [
            MediaChunkResponse::Chunk(vec![1, 2, 3]),
            MediaChunkResponse::Chunk(vec![]),
            MediaChunkResponse::NotFound,
            MediaChunkResponse::Denied,
        ] {
            assert_eq!(MediaChunkResponse::decode(res.encode()).unwrap(), res);
        }
        assert!(MediaChunkResponse::decode(vec![]).is_err());
        assert!(MediaChunkResponse::decode(vec![9]).is_err());
    }

    #[test]
    fn rejects_oversized_message() {
        let data = vec![0u8; 2048];
        let mut io = futures::io::Cursor::new(data);
        let result = futures::executor::block_on(read_limited(&mut io, MAX_REQUEST_SIZE));
        assert!(result.is_err());
    }
}

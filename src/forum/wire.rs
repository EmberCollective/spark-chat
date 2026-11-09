use crate::protocol::forum::{Request, Response};
use async_trait::async_trait;
use futures::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use libp2p::{
    request_response::{self, ProtocolSupport},
    StreamProtocol, SwarmBuilder,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{io, time::Duration};

const MAX_FRAME: usize = 512 * 1024;
#[derive(Clone)]
pub struct Codec;
async fn read<T: AsyncRead + Unpin + Send, V: DeserializeOwned>(io: &mut T) -> io::Result<V> {
    let mut size = [0; 4];
    io.read_exact(&mut size).await?;
    let size = u32::from_be_bytes(size) as usize;
    if size > MAX_FRAME {
        return Err(io::Error::other("论坛帧过大"));
    }
    let mut bytes = vec![0; size];
    io.read_exact(&mut bytes).await?;
    postcard::from_bytes(&bytes).map_err(io::Error::other)
}
async fn write<T: AsyncWrite + Unpin + Send, V: Serialize>(io: &mut T, value: V) -> io::Result<()> {
    let bytes = postcard::to_allocvec(&value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("论坛帧过大"));
    }
    io.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    io.write_all(&bytes).await?;
    io.close().await
}
#[async_trait]
impl request_response::Codec for Codec {
    type Protocol = StreamProtocol;
    type Request = Request;
    type Response = Response;
    async fn read_request<T: AsyncRead + Unpin + Send>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Request> {
        read(io).await
    }
    async fn read_response<T: AsyncRead + Unpin + Send>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
    ) -> io::Result<Response> {
        read(io).await
    }
    async fn write_request<T: AsyncWrite + Unpin + Send>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        value: Request,
    ) -> io::Result<()> {
        write(io, value).await
    }
    async fn write_response<T: AsyncWrite + Unpin + Send>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        value: Response,
    ) -> io::Result<()> {
        write(io, value).await
    }
}
pub type Swarm = libp2p::Swarm<request_response::Behaviour<Codec>>;
pub fn swarm(key: libp2p::identity::Keypair) -> anyhow::Result<Swarm> {
    // No STUN, relay, mDNS or public DHT: joining requires an explicit campus connection code.
    Ok(SwarmBuilder::with_existing_identity(key)
        .with_tokio()
        .with_tcp(
            libp2p::tcp::Config::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_behaviour(|_| {
            request_response::Behaviour::with_codec(
                Codec,
                [(
                    StreamProtocol::new("/sparknet/forum/1"),
                    ProtocolSupport::Full,
                )],
                request_response::Config::default()
                    .with_request_timeout(Duration::from_secs(5))
                    .with_max_concurrent_streams(8),
            )
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oversized_frame_is_rejected_before_allocation() {
        futures::executor::block_on(async {
            let mut input = futures::io::Cursor::new(((MAX_FRAME + 1) as u32).to_be_bytes());
            assert!(read::<_, Request>(&mut input).await.is_err());
        });
    }
}

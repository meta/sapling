/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Client end of the transport.

use std::ffi::CStr;
use std::future::Future;
use std::io::Cursor;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use bytes::Bytes;
use bytes::BytesMut;
use fbthrift::BinaryProtocol;
use fbthrift::ClientFactory;
use fbthrift::ClientStreamElement;
use fbthrift::Framing;
use fbthrift::FramingDecoded;
use fbthrift::FramingEncodedFinal;
use fbthrift::Transport;
use futures::channel::mpsc;
use futures::future::FutureExt;
use futures::sink::SinkExt;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use mononoke_macros::mononoke;
use tokio::net::TcpStream;

use crate::framing::StreamFrame;
use crate::framing::read_frame;
use crate::framing::write_frame;
use crate::loopback::resolve_loopback;

const STREAM_CHANNEL_CAPACITY: usize = 8;

/// Create a client for the service `F` that talks to a server at `address`,
/// a `host:port` string that must resolve only to loopback addresses.
pub fn connect<F>(address: &str) -> Result<Arc<F::Api>>
where
    F: ClientFactory,
{
    let transport = ClientTransport::new(address)?;
    Ok(F::new(BinaryProtocol, transport))
}

/// Client transport for unary and streaming RPCs.
///
/// Each call opens its own connection. Reusing connections would force this
/// layer to decide, whenever an exchange fails, whether the server had
/// already acted on the request: a connection that dies after the request
/// was written looks the same whether the server processed it or had already
/// closed the idle connection. Some methods are writes, so guessing wrong
/// would re-execute them.
#[derive(Clone, Debug)]
pub struct ClientTransport {
    address: Arc<str>,
    resolved: Arc<[SocketAddr]>,
}

impl ClientTransport {
    /// `address` is a `host:port` string. It is resolved once, here, and
    /// must resolve only to loopback addresses.
    pub fn new(address: &str) -> Result<Self> {
        let resolved = resolve_loopback(address, address)?;
        Ok(Self {
            address: address.into(),
            resolved: resolved.into(),
        })
    }

    /// Succeeds if the server is accepting connections.
    pub async fn probe(&self) -> Result<()> {
        self.connect().await.map(|_| ())
    }

    async fn connect(&self) -> Result<TcpStream> {
        let stream = TcpStream::connect(&*self.resolved)
            .await
            .with_context(|| format!("failed to connect to {}", self.address))?;
        // Frames are flushed individually; Nagle would stall each one on the
        // peer's delayed ACK.
        stream
            .set_nodelay(true)
            .with_context(|| format!("failed to set TCP_NODELAY on {}", self.address))?;
        Ok(stream)
    }

    /// Send a request on a fresh connection and read the frame that opens
    /// the reply. The connection is returned so a streaming caller can keep
    /// reading from it.
    async fn exchange(&self, request: &[u8]) -> Result<(Bytes, TcpStream)> {
        let mut stream = self.connect().await?;
        write_frame(&mut stream, request).await?;
        match read_frame(&mut stream).await? {
            Some(frame) => Ok((frame, stream)),
            None => bail!("server closed the connection without a response"),
        }
    }
}

impl Framing for ClientTransport {
    type EncBuf = BytesMut;
    type DecBuf = Cursor<Bytes>;

    fn enc_with_capacity(cap: usize) -> Self::EncBuf {
        BytesMut::with_capacity(cap)
    }
}

impl Transport for ClientTransport {
    type RpcOptions = ();

    fn call(
        &self,
        _service_name: &'static CStr,
        _fn_name: &'static CStr,
        req: FramingEncodedFinal<Self>,
        _rpc_options: Self::RpcOptions,
    ) -> Pin<Box<dyn Future<Output = Result<FramingDecoded<Self>>> + Send + 'static>> {
        let transport = self.clone();
        async move {
            let (response, _stream) = transport.exchange(&req).await?;
            Ok(Cursor::new(response))
        }
        .boxed()
    }

    fn call_stream(
        &self,
        _service_name: &'static CStr,
        _fn_name: &'static CStr,
        req: FramingEncodedFinal<Self>,
        _rpc_options: Self::RpcOptions,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<(
                        FramingDecoded<Self>,
                        BoxStream<'static, Result<ClientStreamElement<FramingDecoded<Self>>>>,
                    )>,
                > + Send
                + 'static,
        >,
    > {
        let transport = self.clone();
        async move {
            let (first_frame, mut stream) = transport.exchange(&req).await?;
            let (mut tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);

            let reader = mononoke::spawn_task(async move {
                loop {
                    let frame = match read_frame(&mut stream).await {
                        Ok(Some(frame)) => frame,
                        Ok(None) => {
                            let _ = tx
                                .send(Err(anyhow!(
                                    "server closed stream before end-of-stream frame"
                                )))
                                .await;
                            break;
                        }
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            break;
                        }
                    };

                    let (element, terminal) = match StreamFrame::decode(frame) {
                        Ok(StreamFrame::Success(payload)) => {
                            (Ok(ClientStreamElement::Reply(Cursor::new(payload))), false)
                        }
                        Ok(StreamFrame::DeclaredException(payload)) => (
                            Ok(ClientStreamElement::DeclaredEx(Cursor::new(payload))),
                            true,
                        ),
                        Ok(StreamFrame::ApplicationException(payload)) => (
                            Ok(ClientStreamElement::ApplicationEx(Cursor::new(payload))),
                            true,
                        ),
                        Ok(StreamFrame::EndOfStream) => break,
                        Err(error) => (Err(error), true),
                    };

                    if tx.send(element).await.is_err() || terminal {
                        break;
                    }
                }
            });

            // The stream owns the reader, so dropping it closes the connection
            // even while the reader waits for a server that has stopped sending.
            let reader = AbortOnDrop(reader);
            let stream = rx.map(move |element| {
                let _ = &reader;
                element
            });
            Ok((Cursor::new(first_frame), stream.boxed()))
        }
        .boxed()
    }
}

/// Aborts a task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use futures::channel::oneshot;

    use super::*;

    /// Echo server that counts how many connections it accepts. When `reply`
    /// is false it reads the request and hangs up without answering, which
    /// is how a server that died mid-request looks to a client.
    async fn echo_server(reply: bool) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address")
            .to_string();
        let accepted = Arc::new(AtomicUsize::new(0));
        let seen = accepted.clone();
        mononoke::spawn_task(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                mononoke::spawn_task(async move {
                    while let Ok(Some(_)) = read_frame(&mut socket).await {
                        if !reply || write_frame(&mut socket, b"reply").await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (address, accepted)
    }

    async fn unary(transport: &ClientTransport) -> Result<Bytes> {
        transport
            .call(c"test", c"method", Bytes::from_static(b"request"), ())
            .await
            .map(|reply| reply.into_inner())
    }

    #[mononoke::test]
    async fn each_call_uses_its_own_connection() {
        let (address, accepted) = echo_server(true).await;
        let transport = ClientTransport::new(&address).expect("loopback should be accepted");

        for _ in 0..3 {
            assert_eq!(
                unary(&transport).await.expect("call should succeed"),
                Bytes::from_static(b"reply")
            );
        }

        assert_eq!(
            accepted.load(Ordering::SeqCst),
            3,
            "each of the three calls should have opened its own connection"
        );
    }

    /// A server that hangs up after reading the request fails the call
    /// instead of being sent it again: it may already have acted on it.
    #[mononoke::test]
    async fn a_request_is_never_resent_after_the_server_hangs_up() {
        let (address, accepted) = echo_server(false).await;
        let transport = ClientTransport::new(&address).expect("loopback should be accepted");

        let error = unary(&transport)
            .await
            .expect_err("a server that never replies should fail the call");
        assert!(error.to_string().contains("without a response"));

        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "the request should not have been retried on a second connection"
        );
    }

    #[mononoke::test]
    async fn probe_reports_whether_the_server_accepts_connections() {
        let (address, _) = echo_server(true).await;
        let transport = ClientTransport::new(&address).expect("loopback should be accepted");
        transport
            .probe()
            .await
            .expect("a listening server should be reachable");

        let closed = {
            let listener =
                std::net::TcpListener::bind("127.0.0.1:0").expect("listener should bind");
            listener
                .local_addr()
                .expect("listener should have an address")
                .to_string()
        };
        let transport = ClientTransport::new(&closed).expect("loopback should be accepted");
        assert!(transport.probe().await.is_err());
    }

    #[mononoke::test]
    fn non_loopback_addresses_are_rejected() {
        assert!(ClientTransport::new("192.0.2.1:1234").is_err());
    }

    async fn call_fake_stream(
        frames: Vec<Bytes>,
    ) -> BoxStream<'static, Result<ClientStreamElement<Cursor<Bytes>>>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        mononoke::spawn_task(async move {
            let (mut socket, _) = listener.accept().await.expect("client should connect");
            read_frame(&mut socket)
                .await
                .expect("request frame should be readable")
                .expect("request frame should be present");
            for frame in frames {
                write_frame(&mut socket, &frame)
                    .await
                    .expect("response frame should be writable");
            }
        });

        let transport =
            ClientTransport::new(&address.to_string()).expect("loopback should be accepted");
        let (_, stream) = transport
            .call_stream(c"test", c"stream", Bytes::from_static(b"request"), ())
            .await
            .expect("initial response should succeed");
        stream
    }

    #[mononoke::test]
    async fn streaming_requires_explicit_end_of_stream() {
        let mut stream = call_fake_stream(vec![Bytes::from_static(b"initial")]).await;
        let error = stream
            .next()
            .await
            .expect("stream should report premature EOF")
            .expect_err("premature EOF should fail the stream");
        assert!(error.to_string().contains("before end-of-stream"));
    }

    #[mononoke::test]
    async fn streaming_stops_on_unknown_frame_type() {
        let mut stream = call_fake_stream(vec![
            Bytes::from_static(b"initial"),
            Bytes::from_static(&[99]),
        ])
        .await;
        let error = stream
            .next()
            .await
            .expect("stream should report an invalid frame")
            .expect_err("unknown frame type should fail the stream");
        assert!(error.to_string().contains("unknown stream frame type"));
        assert!(
            stream.next().await.is_none(),
            "stream should terminate after error"
        );
    }

    /// Dropping a stream closes its connection even when the server has
    /// stopped sending without ending the stream.
    #[mononoke::test]
    async fn dropping_a_stream_closes_its_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        let (closed_tx, closed_rx) = oneshot::channel();
        mononoke::spawn_task(async move {
            let (mut socket, _) = listener.accept().await.expect("client should connect");
            read_frame(&mut socket)
                .await
                .expect("request frame should be readable")
                .expect("request frame should be present");
            write_frame(&mut socket, b"initial")
                .await
                .expect("initial response should be writable");
            // Send nothing more; this read returns once the client closes.
            let _ = read_frame(&mut socket).await;
            let _ = closed_tx.send(());
        });

        let transport =
            ClientTransport::new(&address.to_string()).expect("loopback should be accepted");
        let (_, stream) = transport
            .call_stream(c"test", c"stream", Bytes::from_static(b"request"), ())
            .await
            .expect("initial response should succeed");
        drop(stream);

        tokio::time::timeout(Duration::from_secs(10), closed_rx)
            .await
            .expect("dropping the stream should close the connection")
            .expect("the server should report the closed connection");
    }
}

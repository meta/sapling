/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Server end of the transport.
//!
//! A connection is served serially: read a request frame, await the
//! handler, write the reply, then read the next request. A streaming reply
//! holds its connection until the stream ends.

use std::ffi::CStr;
use std::io::Cursor;
use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use async_trait::async_trait;
use bytes::Bytes;
use fbthrift::ApplicationException;
use fbthrift::ApplicationExceptionErrorCode;
use fbthrift::BinaryProtocol;
use fbthrift::CompactProtocol;
use fbthrift::FramingDecoded;
use fbthrift::FramingEncodedFinal;
use fbthrift::MessageType;
use fbthrift::Protocol;
use fbthrift::ProtocolReader;
use fbthrift::ProtocolWriter;
use fbthrift::Serialize;
use fbthrift::processor::MethodMetadata;
use fbthrift::processor::ReplyState as ThriftReplyState;
use fbthrift::processor::SerializedStreamElement;
use fbthrift::processor::ThriftService;
use fbthrift::request_context::DummyRequestContext;
use fbthrift::thrift_protocol::ProtocolID;
use futures::stream::BoxStream;
use futures::stream::StreamExt;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tokio::time::timeout;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

use crate::framing::MAX_FRAME_SIZE;
use crate::framing::StreamFrame;
use crate::framing::read_frame;
use crate::framing::write_frame;
use crate::loopback::resolve_loopback;

const MAX_CONNECTIONS: usize = 256;
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// A frame that cannot be written within this time ends its connection,
/// including a stream whose client has stopped reading.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Request context passed to services. It carries no caller identity.
pub type RequestContext = DummyRequestContext<CStr, Bytes>;

/// A service as served by this transport.
pub type BoxService = Box<
    dyn ThriftService<Bytes, Handler = (), RequestContext = RequestContext, ReplyState = ReplyState>
        + Send
        + 'static,
>;

/// Creates the service for a connection, given the protocol the connection's
/// first request was written in.
pub type ServiceFactory =
    dyn Fn(ProtocolID) -> Result<BoxService, ApplicationException> + Send + Sync;

/// Box a generated Thrift service so it can be served.
pub fn box_service<T, H>(service: T) -> BoxService
where
    H: 'static,
    T: ThriftService<Bytes, Handler = H, RequestContext = RequestContext, ReplyState = ReplyState>
        + Send
        + Sync
        + 'static,
{
    Box::new(HandlerErased(service))
}

/// Bind a listener to `host` and `port`. `host` is resolved once, here, and
/// must resolve only to loopback addresses.
pub fn bind(host: &str, port: u16) -> Result<StdTcpListener> {
    let resolved = resolve_loopback((host, port), &format!("{host}:{port}"))?;
    let listener = StdTcpListener::bind(&resolved[..])
        .with_context(|| format!("failed to bind to {host}:{port}"))?;
    listener
        .set_nonblocking(true)
        .context("failed to make the listener non-blocking")?;
    Ok(listener)
}

/// Serve connections on `listener` until `shutdown` fires or its sender is
/// dropped. Then stop accepting, let each open connection finish the request
/// it is serving, and return.
pub async fn serve(
    listener: TcpListener,
    service_factory: Arc<ServiceFactory>,
    mut shutdown: oneshot::Receiver<()>,
) {
    let (connection_shutdown_tx, connection_shutdown_rx) = watch::channel(false);
    let mut connections = JoinSet::new();

    loop {
        tokio::select! {
            accepted = listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                match accepted {
                    Ok((stream, peer)) => {
                        debug!("accepted thrift connection from {peer}");
                        // Frames are flushed individually; Nagle would stall
                        // each one on the peer's delayed ACK.
                        if let Err(error) = stream.set_nodelay(true) {
                            warn!("failed to set TCP_NODELAY on connection from {peer}: {error:#}");
                        }
                        let factory = service_factory.clone();
                        let shutdown = connection_shutdown_rx.clone();
                        connections.spawn(async move {
                            if let Err(error) = handle_connection(stream, &*factory, shutdown).await {
                                debug!("thrift connection from {peer} ended: {error:#}");
                            }
                        });
                    }
                    Err(error) => {
                        error!("failed to accept thrift connection: {error:#}");
                        // Errors such as running out of file descriptors
                        // recur immediately; don't spin on them.
                        sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                }
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    error!("thrift connection task failed: {error:#}");
                }
            }
            _ = &mut shutdown => {
                info!("thrift server shutting down");
                break;
            }
        }
    }

    drop(listener);
    let _ = connection_shutdown_tx.send(true);
    while let Some(joined) = connections.join_next().await {
        if let Err(error) = joined {
            error!("thrift connection task failed during shutdown: {error:#}");
        }
    }
}

/// Reply sent from the service to its connection.
enum Reply {
    Unary(Bytes),
    Stream {
        first: Bytes,
        stream: Option<BoxStream<'static, SerializedStreamElement<FramingEncodedFinal<Bytes>>>>,
        protocol_id: ProtocolID,
    },
}

/// Hands the service's reply to the connection that received the request.
pub struct ReplyState {
    reply_tx: std::sync::Mutex<Option<oneshot::Sender<Reply>>>,
}

impl ReplyState {
    fn new(reply_tx: oneshot::Sender<Reply>) -> Self {
        Self {
            reply_tx: std::sync::Mutex::new(Some(reply_tx)),
        }
    }

    fn send(&self, reply: Reply) {
        if let Some(tx) = self.reply_tx.lock().expect("reply_tx lock poisoned").take() {
            let _ = tx.send(reply);
        }
    }

    fn send_unsupported_stream(
        &self,
        first: Bytes,
        protocol_id: ProtocolID,
        kind: &str,
    ) -> ApplicationException {
        let error = ApplicationException::new(
            ApplicationExceptionErrorCode::UnsupportedClientType,
            format!("{kind} RPCs are not supported by the OSS thrift transport"),
        );
        let outbound = error.clone();
        self.send(Reply::Stream {
            first,
            stream: Some(
                futures::stream::once(async move {
                    SerializedStreamElement::ApplicationException(outbound)
                })
                .boxed(),
            ),
            protocol_id,
        });
        error
    }
}

impl ThriftReplyState<Bytes> for ReplyState {
    type RequestContext = RequestContext;

    fn send_reply(&self, reply: FramingEncodedFinal<Bytes>) {
        self.send(Reply::Unary(reply));
    }

    fn send_stream_reply(
        &self,
        response: FramingEncodedFinal<Bytes>,
        stream: Option<BoxStream<'static, SerializedStreamElement<FramingEncodedFinal<Bytes>>>>,
        protocol_id: ProtocolID,
    ) -> Result<()> {
        self.send(Reply::Stream {
            first: response,
            stream,
            protocol_id,
        });
        Ok(())
    }

    fn send_sink_reply(
        &self,
        first_response: FramingEncodedFinal<Bytes>,
        _buffer_size: u64,
        _chunk_timeout: Duration,
        protocol_id: ProtocolID,
    ) -> (
        BoxStream<'static, Result<FramingDecoded<Bytes>, ApplicationException>>,
        impl FnOnce(SerializedStreamElement<FramingEncodedFinal<Bytes>>) + Send,
    ) {
        let error = self.send_unsupported_stream(first_response, protocol_id, "sink");
        (
            futures::stream::once(async move { Err(error) }).boxed(),
            |_| {},
        )
    }

    fn send_bidirectional_reply(
        &self,
        first_response: FramingEncodedFinal<Bytes>,
        _stream: BoxStream<'static, SerializedStreamElement<FramingEncodedFinal<Bytes>>>,
        protocol_id: ProtocolID,
    ) -> BoxStream<'static, Result<FramingDecoded<Bytes>, ApplicationException>> {
        let error =
            self.send_unsupported_stream(first_response, protocol_id, "bidirectional streaming");
        futures::stream::once(async move { Err(error) }).boxed()
    }
}

/// Erases the `Handler` marker type, which dispatch never uses, so services
/// of different types fit behind one [`BoxService`].
struct HandlerErased<T>(T);

#[async_trait]
impl<H, T> ThriftService<Bytes> for HandlerErased<T>
where
    H: 'static,
    T: ThriftService<Bytes, Handler = H, RequestContext = RequestContext, ReplyState = ReplyState>
        + Send
        + Sync
        + 'static,
{
    type Handler = ();
    type RequestContext = RequestContext;
    type ReplyState = ReplyState;

    async fn call(
        &self,
        req: FramingDecoded<Bytes>,
        req_ctxt: &RequestContext,
        reply_state: Arc<ReplyState>,
    ) -> Result<()> {
        self.0.call(req, req_ctxt, reply_state).await
    }

    fn create_interaction(
        &self,
        _name: &str,
    ) -> Result<
        Arc<
            dyn ThriftService<
                    Bytes,
                    Handler = (),
                    RequestContext = RequestContext,
                    ReplyState = ReplyState,
                > + Send
                + 'static,
        >,
    > {
        bail!("interactions are not supported by the OSS thrift transport")
    }

    fn get_method_metadata(&self) -> &'static [MethodMetadata] {
        self.0.get_method_metadata()
    }

    async fn on_termination(&self) {
        self.0.on_termination().await
    }
}

fn detect_protocol(data: &[u8]) -> Result<ProtocolID> {
    match data {
        [0x80, 0x01, ..] => Ok(ProtocolID::BinaryProtocol),
        [0x82, _, ..] => Ok(ProtocolID::CompactProtocol),
        [first, second, ..] => bail!("unrecognized thrift protocol: {first:02x} {second:02x}"),
        _ => bail!(
            "frame too short ({} bytes) for protocol detection",
            data.len()
        ),
    }
}

fn serialize_application_exception(ex: &ApplicationException, proto: ProtocolID) -> Bytes {
    match proto {
        ProtocolID::CompactProtocol => fbthrift::compact_protocol::serialize(ex),
        _ => fbthrift::binary_protocol::serialize(ex),
    }
}

/// An application exception, addressed to the method and sequence id the
/// caller of `request` is waiting on.
fn exception_reply(
    request: &Bytes,
    proto: ProtocolID,
    code: ApplicationExceptionErrorCode,
    message: String,
) -> Result<Bytes> {
    fn header<P: Protocol<Frame = Bytes>>(request: &Bytes) -> Result<(String, u32)> {
        let mut deserializer = P::deserializer(Cursor::new(request.clone()));
        let (method, _, seqid) = deserializer
            .read_message_begin(|method| String::from_utf8_lossy(method).into_owned())?;
        Ok((method, seqid))
    }
    let (method, seqid) = match proto {
        ProtocolID::CompactProtocol => header::<CompactProtocol>(request),
        _ => header::<BinaryProtocol>(request),
    }
    .context("cannot address an exception reply to the caller")?;

    let ex = ApplicationException::new(code, message);
    Ok(match proto {
        ProtocolID::CompactProtocol => fbthrift::serialize!(CompactProtocol, |p| {
            p.write_message_begin(&method, MessageType::Exception, seqid);
            ex.rs_thrift_write(p);
            p.write_message_end();
        }),
        _ => fbthrift::serialize!(BinaryProtocol, |p| {
            p.write_message_begin(&method, MessageType::Exception, seqid);
            ex.rs_thrift_write(p);
            p.write_message_end();
        }),
    })
}

async fn write_reply_frame(stream: &mut TcpStream, frame: &[u8]) -> Result<()> {
    timeout(WRITE_TIMEOUT, write_frame(stream, frame))
        .await
        .context("timed out writing thrift reply")?
}

/// Reply to `request` with an internal-error exception, then fail the
/// connection with `error`.
async fn fail_request(
    stream: &mut TcpStream,
    request: &Bytes,
    proto: ProtocolID,
    error: anyhow::Error,
) -> Result<()> {
    let reply = exception_reply(
        request,
        proto,
        ApplicationExceptionErrorCode::InternalError,
        format!("{error:#}"),
    )?;
    write_reply_frame(stream, &reply).await?;
    Err(error)
}

async fn handle_connection(
    mut stream: TcpStream,
    service_factory: &ServiceFactory,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut service = None;
    let result = handle_requests(&mut stream, service_factory, &mut shutdown, &mut service).await;
    if let Some((_, service)) = service {
        service.on_termination().await;
    }
    result
}

async fn handle_requests(
    stream: &mut TcpStream,
    service_factory: &ServiceFactory,
    shutdown: &mut watch::Receiver<bool>,
    service: &mut Option<(ProtocolID, BoxService)>,
) -> Result<()> {
    loop {
        let request = tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            frame = timeout(CONNECTION_IDLE_TIMEOUT, read_frame(stream)) => {
                frame.context("timed out waiting for thrift request")??
            }
        };
        let Some(request) = request else {
            return Ok(());
        };
        let proto = detect_protocol(&request)?;
        if service.is_none() {
            let created = service_factory(proto).map_err(|error| {
                anyhow!("failed to create service for protocol {proto:?}: {error}")
            })?;
            *service = Some((proto, created));
        }
        let (connection_proto, service) = service
            .as_ref()
            .context("service should exist after initialization")?;
        if *connection_proto != proto {
            bail!("connection changed thrift protocol from {connection_proto:?} to {proto:?}");
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        let reply_state = Arc::new(ReplyState::new(reply_tx));
        let request_context = RequestContext::new();
        if let Err(error) = service
            .call(Cursor::new(request.clone()), &request_context, reply_state)
            .await
        {
            return fail_request(
                stream,
                &request,
                proto,
                error.context("thrift service call failed"),
            )
            .await;
        }
        let reply = match reply_rx.await {
            Ok(reply) => reply,
            Err(_) => {
                return fail_request(stream, &request, proto, anyhow!("service did not reply"))
                    .await;
            }
        };

        match reply {
            Reply::Unary(response) => {
                let response = if response.len() > MAX_FRAME_SIZE {
                    error!(
                        "unary response is {} bytes, over the {MAX_FRAME_SIZE} byte frame limit; replying with an application exception",
                        response.len(),
                    );
                    exception_reply(
                        &request,
                        proto,
                        ApplicationExceptionErrorCode::InternalError,
                        format!(
                            "response exceeds the OSS transport frame limit of {MAX_FRAME_SIZE} bytes"
                        ),
                    )?
                } else {
                    response
                };
                write_reply_frame(stream, &response).await?;
            }
            Reply::Stream {
                first,
                stream: elements,
                protocol_id,
            } => {
                if first.len() > MAX_FRAME_SIZE {
                    return fail_request(
                        stream,
                        &request,
                        proto,
                        anyhow!(
                            "initial stream response exceeds the OSS transport frame limit of {MAX_FRAME_SIZE} bytes"
                        ),
                    )
                    .await;
                }
                write_reply_frame(stream, &first).await?;
                if !write_stream(stream, elements, protocol_id).await? {
                    return Ok(());
                }
            }
        }
    }
}

/// Write the elements of a streaming reply. Returns whether the stream ended
/// successfully, in which case the connection may serve another request.
async fn write_stream(
    stream: &mut TcpStream,
    elements: Option<BoxStream<'static, SerializedStreamElement<FramingEncodedFinal<Bytes>>>>,
    protocol_id: ProtocolID,
) -> Result<bool> {
    if let Some(mut elements) = elements {
        while let Some(element) = elements.next().await {
            let (frame, terminal) = match element {
                SerializedStreamElement::Success(payload) => (StreamFrame::Success(payload), false),
                SerializedStreamElement::DeclaredException { payload, .. } => {
                    (StreamFrame::DeclaredException(payload), true)
                }
                SerializedStreamElement::ApplicationException(ex) => (
                    StreamFrame::ApplicationException(serialize_application_exception(
                        &ex,
                        protocol_id,
                    )),
                    true,
                ),
            };
            let (frame, terminal) = match frame.encode() {
                Ok(frame) => (frame, terminal),
                Err(error) => {
                    error!("cannot encode stream element: {error:#}");
                    let ex = ApplicationException::new(
                        ApplicationExceptionErrorCode::InternalError,
                        "stream element exceeds the OSS transport frame limit".to_string(),
                    );
                    let frame = StreamFrame::ApplicationException(serialize_application_exception(
                        &ex,
                        protocol_id,
                    ))
                    .encode()
                    .context("failed to encode oversized-frame error")?;
                    (frame, true)
                }
            };
            write_reply_frame(stream, &frame).await?;
            if terminal {
                return Ok(false);
            }
        }
    }
    write_reply_frame(stream, &StreamFrame::EndOfStream.encode()?).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use fb303_core::fb303_status;
    use fb303_core_services::BaseService;
    use fb303_core_services::errors::GetNameExn;
    use fb303_core_services::errors::GetStatusDetailsExn;
    use fb303_core_services::errors::GetStatusExn;
    use fb303_core_services::make_BaseService_server;
    use fbthrift::Deserialize;
    use fbthrift::Framing;
    use fbthrift::Transport;
    use mononoke_macros::mononoke;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::client::ClientTransport;

    #[derive(Clone)]
    struct NamedService(&'static str);

    #[async_trait]
    impl BaseService for NamedService {
        async fn getName(&self) -> Result<String, GetNameExn> {
            Ok(self.0.to_owned())
        }

        async fn getStatus(&self) -> Result<fb303_status, GetStatusExn> {
            Ok(fb303_status::ALIVE)
        }

        async fn getStatusDetails(&self) -> Result<String, GetStatusDetailsExn> {
            Ok(String::new())
        }
    }

    /// Service that answers every request with a canned reply: unary when
    /// `elements` is `None`, streaming otherwise.
    #[derive(Clone)]
    struct CannedService {
        first: Bytes,
        elements: Option<Vec<Bytes>>,
        proto: ProtocolID,
    }

    #[async_trait]
    impl ThriftService<Bytes> for CannedService {
        type Handler = ();
        type RequestContext = RequestContext;
        type ReplyState = ReplyState;

        async fn call(
            &self,
            _req: FramingDecoded<Bytes>,
            _req_ctxt: &Self::RequestContext,
            reply_state: Arc<Self::ReplyState>,
        ) -> Result<()> {
            let Some(elements) = self.elements.clone() else {
                reply_state.send_reply(self.first.clone());
                return Ok(());
            };
            let stream =
                futures::stream::iter(elements.into_iter().map(SerializedStreamElement::Success))
                    .boxed();
            reply_state.send_stream_reply(self.first.clone(), Some(stream), self.proto)
        }

        fn create_interaction(
            &self,
            _name: &str,
        ) -> Result<
            Arc<
                dyn ThriftService<
                        Bytes,
                        Handler = (),
                        RequestContext = RequestContext,
                        ReplyState = ReplyState,
                    > + Send
                    + 'static,
            >,
        > {
            bail!("interactions not supported")
        }

        fn get_method_metadata(&self) -> &'static [MethodMetadata] {
            &[]
        }

        async fn on_termination(&self) {}
    }

    struct TestServer {
        address: String,
        shutdown: oneshot::Sender<()>,
        server: tokio::task::JoinHandle<()>,
    }

    impl TestServer {
        async fn start(factory: Arc<ServiceFactory>) -> Self {
            let listener =
                TcpListener::from_std(bind("127.0.0.1", 0).expect("loopback should bind"))
                    .expect("listener should convert");
            let address = listener
                .local_addr()
                .expect("listener should have an address")
                .to_string();
            let (shutdown, shutdown_rx) = oneshot::channel();
            let server = mononoke::spawn_task(serve(listener, factory, shutdown_rx));
            Self {
                address,
                shutdown,
                server,
            }
        }

        async fn connect(&self) -> TcpStream {
            TcpStream::connect(&self.address)
                .await
                .expect("client should connect")
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            self.server.await.expect("server task should not panic");
        }
    }

    fn named_service(name: &'static str) -> Arc<ServiceFactory> {
        Arc::new(move |proto| {
            Ok(box_service(make_BaseService_server(
                proto,
                NamedService(name),
            )?))
        })
    }

    fn canned_service(first: Bytes, elements: Option<Vec<Bytes>>) -> Arc<ServiceFactory> {
        Arc::new(move |proto| {
            Ok(box_service(CannedService {
                first: first.clone(),
                elements: elements.clone(),
                proto,
            }))
        })
    }

    fn binary_call(method: &str, seq_id: i32) -> Vec<u8> {
        let mut buf = Vec::new();
        // Version and message type CALL.
        buf.extend_from_slice(&[0x80, 0x01, 0x00, 0x01]);
        buf.extend_from_slice(&(method.len() as u32).to_be_bytes());
        buf.extend_from_slice(method.as_bytes());
        buf.extend_from_slice(&seq_id.to_be_bytes());
        // Empty argument struct.
        buf.push(0x00);
        buf
    }

    async fn send_frame(stream: &mut TcpStream, payload: &[u8]) {
        write_frame(stream, payload)
            .await
            .expect("request should be written");
    }

    async fn receive_frame(stream: &mut TcpStream) -> Bytes {
        read_frame(stream)
            .await
            .expect("reply should be readable")
            .expect("reply should be present")
    }

    #[mononoke::test]
    fn detects_protocols() {
        assert_eq!(
            detect_protocol(&[0x80, 0x01, 0x00, 0x01]).expect("binary should be detected"),
            ProtocolID::BinaryProtocol
        );
        assert_eq!(
            detect_protocol(&[0x82, 0x21, 0x00]).expect("compact should be detected"),
            ProtocolID::CompactProtocol
        );
        assert!(detect_protocol(&[0x00, 0x00, 0x00]).is_err());
        assert!(detect_protocol(&[0x80]).is_err());
    }

    #[mononoke::test]
    fn bind_refuses_non_loopback_hosts() {
        assert!(bind("0.0.0.0", 0).is_err());
        assert!(bind("::", 0).is_err());
    }

    #[mononoke::test]
    async fn serves_several_requests_on_one_connection() {
        let server = TestServer::start(named_service("TestServer")).await;
        let mut stream = server.connect().await;

        for seq_id in 1..=3 {
            send_frame(&mut stream, &binary_call("getName", seq_id)).await;
            let reply = receive_frame(&mut stream).await;
            assert_eq!(
                reply[..4],
                [0x80, 0x01, 0x00, 0x02],
                "reply {seq_id} should be a binary REPLY message"
            );
            assert!(
                String::from_utf8_lossy(&reply).contains("TestServer"),
                "reply {seq_id} should carry the service name"
            );
        }

        drop(stream);
        server.stop().await;
    }

    #[mononoke::test]
    async fn streams_elements_then_end_of_stream() {
        let elements = vec![
            Bytes::from_static(b"elem1"),
            Bytes::from_static(b"elem2"),
            Bytes::from_static(b"elem3"),
        ];
        let server = TestServer::start(canned_service(
            Bytes::from_static(b"initial"),
            Some(elements.clone()),
        ))
        .await;
        let mut stream = server.connect().await;

        send_frame(&mut stream, &binary_call("stream", 1)).await;
        assert_eq!(
            receive_frame(&mut stream).await,
            Bytes::from_static(b"initial")
        );
        let mut received = Vec::new();
        loop {
            match StreamFrame::decode(receive_frame(&mut stream).await).expect("valid stream frame")
            {
                StreamFrame::Success(payload) => received.push(payload),
                StreamFrame::EndOfStream => break,
                other => panic!("unexpected stream frame: {other:?}"),
            }
        }
        assert_eq!(received, elements);

        drop(stream);
        server.stop().await;
    }

    #[mononoke::test]
    async fn an_empty_stream_is_just_end_of_stream() {
        let server =
            TestServer::start(canned_service(Bytes::from_static(b"only"), Some(vec![]))).await;
        let mut stream = server.connect().await;

        send_frame(&mut stream, &binary_call("stream", 1)).await;
        assert_eq!(
            receive_frame(&mut stream).await,
            Bytes::from_static(b"only")
        );
        assert_eq!(
            StreamFrame::decode(receive_frame(&mut stream).await).expect("valid stream frame"),
            StreamFrame::EndOfStream
        );

        drop(stream);
        server.stop().await;
    }

    #[mononoke::test]
    async fn the_client_transport_reads_streams() {
        let elements = vec![
            Bytes::from_static(b"c_elem1"),
            Bytes::from_static(b"c_elem2"),
        ];
        let server = TestServer::start(canned_service(
            Bytes::from_static(b"first"),
            Some(elements.clone()),
        ))
        .await;
        let transport = ClientTransport::new(&server.address).expect("loopback should be accepted");

        let request = Bytes::from(binary_call("stream", 1));
        let mut encoded = ClientTransport::enc_with_capacity(request.len());
        encoded.extend_from_slice(&request);
        let (initial, stream) = transport
            .call_stream(c"test", c"stream", encoded.freeze(), ())
            .await
            .expect("streaming call should succeed");
        assert_eq!(initial.into_inner(), Bytes::from_static(b"first"));

        let received: Vec<_> = stream
            .map(
                |element| match element.expect("stream element should succeed") {
                    fbthrift::ClientStreamElement::Reply(cursor) => cursor.into_inner(),
                    other => panic!("unexpected stream element: {other:?}"),
                },
            )
            .collect()
            .await;
        assert_eq!(received, elements);

        server.stop().await;
    }

    #[mononoke::test]
    async fn an_oversized_stream_element_ends_the_stream_with_an_exception() {
        let server = TestServer::start(canned_service(
            Bytes::from_static(b"initial"),
            Some(vec![Bytes::from(vec![0; MAX_FRAME_SIZE])]),
        ))
        .await;
        let mut stream = server.connect().await;

        send_frame(&mut stream, &binary_call("stream", 1)).await;
        receive_frame(&mut stream).await;
        assert!(matches!(
            StreamFrame::decode(receive_frame(&mut stream).await).expect("valid stream frame"),
            StreamFrame::ApplicationException(_)
        ));
        let mut trailing = [0_u8; 1];
        let read = timeout(Duration::from_secs(1), stream.read(&mut trailing))
            .await
            .expect("server should close promptly")
            .expect("socket read should succeed");
        assert_eq!(
            read, 0,
            "server should close without an end-of-stream frame"
        );

        server.stop().await;
    }

    /// A unary response too large to frame comes back as an exception
    /// addressed to the caller, rather than as a bare connection close.
    #[mononoke::test]
    async fn an_oversized_unary_response_is_an_exception() {
        let server = TestServer::start(canned_service(
            Bytes::from(vec![0; MAX_FRAME_SIZE + 1]),
            None,
        ))
        .await;
        let mut stream = server.connect().await;

        send_frame(&mut stream, &binary_call("oversized", 7)).await;
        let reply = receive_frame(&mut stream).await;
        let mut deserializer = <BinaryProtocol>::deserializer(Cursor::new(reply));
        let (method, message_type, seqid) = deserializer
            .read_message_begin(|method| String::from_utf8_lossy(method).into_owned())
            .expect("reply should carry a message envelope");
        assert_eq!(method, "oversized");
        assert_eq!(message_type, MessageType::Exception);
        assert_eq!(seqid, 7);
        let exception = ApplicationException::rs_thrift_read(&mut deserializer)
            .expect("reply should carry an application exception");
        assert_eq!(
            exception.type_,
            ApplicationExceptionErrorCode::InternalError
        );
        assert!(exception.message.contains("frame limit"));

        server.stop().await;
    }

    #[mononoke::test]
    async fn shutdown_closes_idle_connections() {
        let server = TestServer::start(named_service("Idle")).await;
        let mut stream = server.connect().await;
        // Make sure the connection is being served before shutting down.
        send_frame(&mut stream, &binary_call("getName", 1)).await;
        receive_frame(&mut stream).await;

        timeout(Duration::from_secs(5), server.stop())
            .await
            .expect("shutdown should not wait for idle connections");
        let mut trailing = [0_u8; 1];
        assert_eq!(
            stream
                .read(&mut trailing)
                .await
                .expect("socket read should succeed"),
            0,
            "the idle connection should have been closed"
        );
        stream
            .shutdown()
            .await
            .expect("client side should shut down cleanly");
    }
}

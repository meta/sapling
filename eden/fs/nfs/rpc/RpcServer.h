/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <chrono>
#include <optional>
#include <vector>

#include <folly/SocketAddress.h>
#include <folly/io/async/AsyncServerSocket.h>
#include <folly/io/async/AsyncSocket.h>
#include <folly/net/NetworkSocket.h>

#include "eden/common/utils/ImmediateFuture.h"
#include "eden/fs/inodes/FsChannel.h"
#include "eden/fs/nfs/portmap/PortmapClient.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/utils/EventBaseState.h"

namespace folly {
class Executor;
}

namespace facebook::eden {

class EdenFsEventsLogger;

enum class RpcStopReason {
  /**
   * The socket was closed. For nfsd3, the socket closing means the
   * mount point was unmounted (perhaps by EdenFS). For mountd, this
   * means a normal connection was closed, but that case is typical.
   */
  UNMOUNT,
  /**
   * Reading from the socket failed. There's nothing else to do, so
   * the server stopped.
   */
  ERROR,
  /**
   * takeoverStop() was called.
   */
  TAKEOVER,
};

struct RpcStopData final : FsStopData {
  bool isUnmounted() override;
  FsChannelInfo extractTakeoverInfo() override;
  FsStopReason getStopReason() const override;

  /**
   * The reason why the connection was stopped.
   *
   * If multiple events occurred that triggered shutdown, only one will be
   * reported here.
   */
  RpcStopReason reason;

  /**
   * The socket for communicating with the kernel, if it is still valid
   * to use.
   */
  folly::File socketToKernel;
};

/**
 * Timeline of an RPC request's journey through the RPC server pipeline.
 * Each phase boundary is stamped as a time_point for per-phase duration
 * metrics (accept, queue_wait, processing, write_wait).
 */
struct RpcRequestTimeline {
  using TimePoint = std::optional<std::chrono::steady_clock::time_point>;
  TimePoint requestReceived;
  TimePoint dispatched;
  TimePoint handlerStart;
  TimePoint handlerDone;
  TimePoint responseSent;
  uint32_t procNumber{0};
};

class RpcServerProcessor {
 public:
  virtual ~RpcServerProcessor() = default;

  /**
   * Whether the server should parse the AUTH_SYS credential of incoming
   * calls at all. This is the single gate for the credential fast path:
   * when it returns false the parse is skipped entirely and
   * checkAuthentication/dispatchRpc see std::nullopt, which every
   * credential consumer already treats as "do nothing". Processors whose
   * credential-consuming features are all disabled should return false so
   * credential handling costs nothing on the per-request hot path.
   * Defaults to true.
   */
  virtual bool shouldParseAuthSysCreds();

  /**
   * Decide whether the call is allowed. authSysCreds carries the parsed
   * AUTH_SYS credential when the call had one (see parseAuthSysCreds and
   * shouldParseAuthSysCreds); it is std::nullopt for other flavors, for
   * malformed credential bodies, and when the parse was skipped. The
   * reference is only valid for the duration of the call.
   */
  virtual auth_stat checkAuthentication(
      const call_body& call_body,
      const std::optional<authsys_parms>& authSysCreds);

  /**
   * Handle one RPC call. authSysCreds is the parsed AUTH_SYS credential (see
   * checkAuthentication above); the reference is only valid until this
   * function returns, so implementations must copy what they need before
   * deferring work to the returned future.
   */
  virtual ImmediateFuture<folly::Unit> dispatchRpc(
      folly::io::Cursor deser,
      folly::io::QueueAppender ser,
      uint32_t xid,
      uint32_t progNumber,
      uint32_t progVersion,
      uint32_t procNumber,
      const std::optional<authsys_parms>& authSysCreds);
  virtual void clientConnected();
  virtual void onShutdown(RpcStopData stopData);
  virtual void onExtraConnection();
  virtual void onExtraConnectionRefused();

  /**
   * Whether this server may serve more than one concurrently connected
   * client. Mountd carries each mount protocol exchange on its own
   * short-lived connection, so it must keep accepting new connections.
   * Nfsd3 serves exactly one client — the kernel — and does not support
   * reconnects; since EOF on an accepted connection is treated as an
   * unmount, a single-client server must refuse extra connections rather
   * than accept them.
   */
  virtual bool acceptsMultipleConnections() const {
    return true;
  }

  /**
   * Return true to enable fast-path handling of certain RPCs directly on
   * the EventBase thread, bypassing the thread pool. When enabled:
   *   - null RPCs (proc=0) get an inline SUCCESS reply
   *   - unimplemented procs (per isUnimplementedProc) get PROC_UNAVAIL
   */
  virtual bool shouldFastPathRPCs() const {
    return false;
  }

  /**
   * Return true for procedures that are unimplemented and should be
   * fast-pathed as PROC_UNAVAIL directly on the EventBase thread.
   */
  virtual bool isUnimplementedProc(uint32_t /*proc*/) const {
    return false;
  }

  /**
   * Result of an inline rejection check on the EventBase thread.
   * Three valid states:
   *   - rejected=true, permit=nullptr: request was rejected, response
   * serialized
   *   - rejected=false, permit!=nullptr: permit acquired, proceed to thread
   * pool
   *   - rejected=false, permit=nullptr: rate limiting not configured, proceed
   */
  struct InlineRejectResult {
    bool rejected = false;
    std::unique_ptr<RequestPermit> permit;
  };

  /**
   * Called on the EventBase thread to check whether rate limiting denies
   * this request. Default: no rejection.
   */
  virtual InlineRejectResult tryInlineReject() {
    return {};
  }

  /**
   * Serialize the rejection response into ser. Only called after
   * tryInlineReject returned rejected=true.
   */
  virtual void serializeInlineReject(
      uint32_t /*proc*/,
      uint32_t /*xid*/,
      folly::io::QueueAppender& /*ser*/) {}

  /**
   * Called after a request completes and the response has been written.
   * Subclasses implement this to record timing metrics and log anomalies.
   * Called on the EventBase thread from the socket write callback.
   */
  virtual void onRequestComplete(const RpcRequestTimeline& /*timeline*/) {}
};

class RpcServer;

/**
 * Size of each freshly allocated recv(2) buffer.
 *
 * This is folly::IOBufQueue::preallocate's `newAllocationSize` argument, whose
 * signature is preallocate(min, newAllocationSize, max = SIZE_MAX). It sizes a
 * newly allocated buffer; it is not a cap on how much a single recv(2) may
 * return. We leave `max` at its default, and preallocate's fast path returns
 * the whole available tailroom, which can exceed this value.
 */
inline constexpr size_t kDefaultReadBufferAllocationSize = 64 * 1024;

/**
 * RpcConnectionHandler manages connected RPC sockets, whether for NFS or Mountd
 * or Rpcbind.
 *
 * Right now, it only supports stream sockets, such as TCP. If we add support
 * for UDP or unix datagram sockets, rename this to RpcStreamHandler and
 * introduce an RpcDatagramHandler.
 */
class RpcConnectionHandler : public folly::DelayedDestruction,
                             private folly::AsyncReader::ReadCallback,
                             private folly::AsyncWriter::WriteCallback {
 public:
  using UniquePtr = std::
      unique_ptr<RpcConnectionHandler, folly::DelayedDestruction::Destructor>;

  RpcConnectionHandler(const RpcConnectionHandler&) = delete;
  RpcConnectionHandler& operator=(const RpcConnectionHandler&) = delete;
  RpcConnectionHandler(RpcConnectionHandler&&) = delete;
  RpcConnectionHandler& operator=(RpcConnectionHandler&&) = delete;

  /**
   * Build a RpcConnectionHandler.
   *
   * When the returned UniquePtr is dropped, this class will stay alive until
   * the client drops the connection, at which time the memory will be released
   * and the socket will be closed.
   */
  template <class... Args>
  static UniquePtr create(Args&&... args) {
    return UniquePtr(
        new RpcConnectionHandler(std::forward<Args>(args)...),
        folly::DelayedDestruction::Destructor());
  }

  /**
   * Unregister the reader, so that no more data is read from the socket.
   *
   * This must be called on the main event base of the socket. This is because
   * we are going to access state_ which can only be accessed on the main
   * eventbase and we do operations on the socket (which generally can only be
   * done on the main eventbase).
   */
  folly::SemiFuture<folly::Unit> takeoverStop();

  /**
   * Record per-phase timing metrics from the request timeline.
   */
  void recordPhaseTimings(const RpcRequestTimeline& timeline) noexcept;

 private:
  RpcConnectionHandler(
      std::shared_ptr<RpcServerProcessor> proc,
      folly::AsyncSocket::UniquePtr&& socket,
      std::shared_ptr<folly::Executor> threadPool,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      std::weak_ptr<RpcServer> owningServer,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval);

  ~RpcConnectionHandler() override;

  // AsyncReader::ReadCallback

  void getReadBuffer(void** bufP, size_t* lenP) override;

  void readDataAvailable(size_t len) noexcept override;

  bool isBufferMovable() noexcept override;

  void readBufferAvailable(
      std::unique_ptr<folly::IOBuf> readBuf) noexcept override;

  void readEOF() noexcept override;

  void readErr(const folly::AsyncSocketException& ex) noexcept override;

  // AsyncWriter::WriteCallback

  void writeSuccess() noexcept override {}

  void writeErr(
      size_t /*bytesWritten*/,
      const folly::AsyncSocketException& ex) noexcept override;

  /**
   * Parse the buffer that was just read from the socket. Complete RPC buffers
   * will be dispatched to the RpcServerProcessor.
   */
  void tryConsumeReadBuffer() noexcept;

  /**
   * Serialize and send an inline RPC reply on the EventBase thread.
   * The body serializer is called with a QueueAppender positioned after
   * the fragment header placeholder.
   */
  template <typename F>
  void writeInlineReply(F&& serializeBody);

  /**
   * Delete the reader, called when the socket is closed or on takeover.
   *
   * This must be called on the main event base of the socket. This is
   * because we are going to access state_ which can only be accessed on the
   * main eventbase and we do operations on the socket (which generally can
   * only be done on the main eventbase).
   */
  folly::SemiFuture<folly::Unit> resetReader(RpcStopReason stopReason);

  /**
   * Try to read one request from the buffer.
   *
   * Return a nullptr if no complete RPC request can be read.
   */
  std::unique_ptr<folly::IOBuf> readOneRequest() noexcept;

  /**
   * Dispatch the RPC request contained in the input buffer to the
   * RpcServerProcessor.
   */
  void dispatchAndReply(
      std::unique_ptr<folly::IOBuf> input,
      DestructorGuard guard,
      std::unique_ptr<RequestPermit> permit,
      RpcRequestTimeline timeline);

  using ReplyResult =
      std::pair<std::unique_ptr<folly::IOBuf>, RpcRequestTimeline>;

  /**
   * Write a finished reply to the socket. Must run on the EventBase thread.
   */
  void writeReply(folly::Try<ReplyResult> result);

  /**
   * Account for a request that has been answered, completing shutdown if it
   * was the last one. Must run on the EventBase thread.
   */
  void finishRequest();

  /**
   * Reply to an rpc call with an error.
   * This function assumes that some data may have already been written to the
   * output buffer. That output is discarded and we reply with an error message
   * instead.
   * This should be used if an exception is thrown while disptaching a request
   * and the exception bubbles up to the rpc server.
   */
  void replyServerError(
      accept_stat err,
      uint32_t xid,
      std::unique_ptr<folly::IOBufQueue>& outputBuffer);

  /**
   * Locally logs an error due to parsing an NFS request as well as log
   * externally, so we can monitor these errors.
   */
  void recordParsingError(
      const RpcParsingError& err,
      std::unique_ptr<folly::IOBuf> input);

  /**
   * Processor to handle the requests.
   */
  std::shared_ptr<RpcServerProcessor> proc_;

  /**
   * Socket we are listening on.
   */
  folly::AsyncSocket::UniquePtr sock_;

  /**
   * NFS requests will be dispatched to this Executor. This is done to avoid
   * blocking the event base that is reading on the socket.
   */
  std::shared_ptr<folly::Executor> threadPool_;

  /**
   * This is a logger for error events. Inside a Meta environment, these events
   * are exported off the machine this EdenFS instance is running on. This is
   * where you log anomalous things that you want to monitor across the fleet.
   */
  std::shared_ptr<EdenFsEventsLogger> errorLogger_;

  folly::IOBufQueue readBuf_{folly::IOBufQueue::cacheChainLength()};

  /**
   * Status for the rpc connection. The State may only be accessed from the
   * socket's eventbase thread. We use this invariant so that we don't have to
   * add a lock to the state which would be used in the hot path for every
   * request.
   */
  struct State {
    // If set, shutdown has started.
    std::optional<RpcStopReason> stopReason;

    // number of requests we are in the middle of processing
    size_t pendingRequests = 0;

    // We log to scuba when clients see a high number of NFS requests. It's
    // likely we exceed this threshold many times in a row, so we only log
    // once every EdenConfig::highFsRequestsLogInterval. This keeps track of
    // the last time we logged to scuba.
    std::chrono::steady_clock::time_point lastHighNfsRequestsLog_;
  };

  EventBaseState<State> state_;

  /**
   * Promise that we set during shutdown when we finish processing all the
   * pending requests.
   *
   * pendingRequestsComplete_ is fulfilled the first time
   * pendingRequests == 0 and stopReason.has_value().
   */
  folly::Promise<folly::Unit> pendingRequestsComplete_;

  /**
   * RpcServer that initiated this RpcConnectionHandler. We keep a reference to
   * this so that we can notify the server when we are shutting down. The server
   * should outlive all of it's connections, but if the server has already been
   * shutdown then we can just skip notifying it that we are shutting down.
   */
  std::weak_ptr<RpcServer> owningServer_;

  /**
   * The maximum number of requests that can be processed at one time.
   * Currently, not enforced.
   */
  size_t maximumInFlightRequests_;

  /**
   * We log when the number of pending requests exceeds maximumInFlightRequests,
   * however to avoid spamming the logs once per highNfsRequestsLogInterval.
   */
  std::chrono::nanoseconds highNfsRequestsLogInterval_;
};

class RpcServer final : public std::enable_shared_from_this<RpcServer>,
                        private folly::AsyncServerSocket::AcceptCallback {
 public:
  /**
   * Create an RPC server.
   *
   * Request will be received on the passed EventBase and dispatched to the
   * RpcServerProcessor on the passed in threadPool.
   */
  static std::shared_ptr<RpcServer> create(
      std::shared_ptr<RpcServerProcessor> proc,
      folly::EventBase* evb,
      std::shared_ptr<folly::Executor> threadPool,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval,
      size_t socketBufferSize = 0);

  /**
   * RpcServer must be torn down on its EventBase. destroy() is called by the
   * shared_ptr deleter.
   */
  void destroy();

  /**
   * Bind this server to the passed in address and start accepting
   * connections.
   */
  void initialize(folly::SocketAddress addr);

  /**
   * Initialize this server from an existing connected socket.
   */
  void initializeConnectedSocket(folly::File socket);

  /**
   * Initialize this server from an existing server socket.
   */
  void initializeServerSocket(folly::File socket);

  /**
   * Resume accepting on an existing server socket after takeoverStop() paused
   * the accept callback.
   */
  void resumeAccepting();

  /**
   * Stop reading new requests, wait for pending requests, and detach and return
   * the connected socket for handoff to a new process.
   *
   * Must be called on the RpcServer's EventBase.
   */
  folly::SemiFuture<folly::File> takeoverStop();

  /**
   * Enables the service mapping with rpcbind/portmap.
   *
   * Must be called on the EventBase.
   */
  void registerService(uint32_t progNumber, uint32_t progVersion);

  /**
   * Return the EventBase that this RpcServer is running on.
   */
  folly::EventBase* getEventBase() const {
    return evb_;
  }

  /**
   * Returns the address that this RPC server is listening on.
   */
  folly::SocketAddress getAddr() const;

  /**
   * The socket underlying handlerToErase was closed and so the handler
   * is shutting down. This informs the server so that the server can stop
   * tracking it.
   *
   * Must be called on the EventBase.
   */
  void unregisterRpcHandler(RpcConnectionHandler* handlerToErase);

 private:
  RpcServer(
      std::shared_ptr<RpcServerProcessor> proc,
      folly::EventBase* evb,
      std::shared_ptr<folly::Executor> threadPool,
      const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
      size_t maximumInFlightRequests,
      std::chrono::nanoseconds highNfsRequestsLogInterval,
      size_t socketBufferSize);

  ~RpcServer() override;

  /**
   * Apply socketBufferSize_ to a connected socket, if configured.
   */
  void configureSocket(folly::AsyncSocket& socket);

  // AsyncServerSocket::AcceptCallback

  void connectionAccepted(
      folly::NetworkSocket fd,
      const folly::SocketAddress& clientAddr,
      AcceptInfo /* info */) noexcept override;

  void acceptError(const std::exception& ex) noexcept override;

  void acceptStopped() noexcept override;

  // main event base that is used for socket interactions. Do not block this
  // event base, it needs to be available to process incoming reads and writes
  // on the socket.
  // TODO: This should probably be KeepAlive<EventBase>.
  folly::EventBase* evb_;

  // Threadpool for processing requests off the main event base.
  std::shared_ptr<folly::Executor> threadPool_;

  // Logger for logging anomalous things to Scuba
  std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger_;

  // listening socket for this server.
  folly::AsyncServerSocket::UniquePtr serverSocket_;

  // used to handle requests on the connected sockets.
  std::shared_ptr<RpcServerProcessor> proc_;

  struct PortmapState {
    PortmapClient portMap;
    std::vector<PortmapMapping4> mappedPorts;
  };

  struct State {
    // Set to true when acceptStopped() is called so we can assert that it was
    // synchronous.
    bool acceptStopped = false;

    std::optional<PortmapState> portmapState;

    // Existing handlers that have an open socket and are processing requests
    // from their socket.
    std::vector<RpcConnectionHandler::UniquePtr> connectionHandlers;
  };

  EventBaseState<State> state_;

  // The maximum number of requests that can be processed at one time.
  // Currently, not enforced.
  size_t maximumInFlightRequests_;

  // We log when the number of pending requests exceeds maximumInFlightRequests,
  // however to avoid spamming the logs once per highNfsRequestsLogInterval.
  std::chrono::nanoseconds highNfsRequestsLogInterval_;

  // Send and receive buffer size for connected Unix domain sockets, 0 for
  // the kernel default.
  size_t socketBufferSize_;
};

} // namespace facebook::eden

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/rpc/RpcServer.h"

#include <tuple>

#include <folly/String.h>
#include <folly/futures/Future.h>
#include <folly/io/IOBufQueue.h>
#include <folly/io/async/AsyncSocket.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Sockets.h>

#include "eden/common/utils/Throw.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"
#include "eden/fs/telemetry/LogEvent.h"
#include "eden/fs/utils/FsChannelTypes.h"

using folly::AsyncServerSocket;
using folly::AsyncSocket;
using folly::Future;
using folly::IOBuf;
using folly::SocketAddress;

namespace facebook::eden {

bool RpcStopData::isUnmounted() {
  return !socketToKernel;
}

FsChannelInfo RpcStopData::extractTakeoverInfo() {
  return NfsChannelData{std::move(socketToKernel)};
}

FsStopReason RpcStopData::getStopReason() const {
  switch (reason) {
    case RpcStopReason::UNMOUNT:
      return {"nfs_eof", true};
    case RpcStopReason::ERROR:
      return {"nfs_read_error", true};
    case RpcStopReason::TAKEOVER:
      return {"takeover", false};
  }
  return {"unknown", false};
}

void RpcConnectionHandler::getReadBuffer(void** bufP, size_t* lenP) {
  // TODO(xavierd): Should kDefaultReadBufferAllocationSize be configured to be
  // at least the configured NFS iosize?
  constexpr size_t minReadSize = 4 * 1024;

  // We want to issue a recv(2) of at least minReadSize, and to reuse whatever
  // the readBuf_ already has room for before allocating again, to minimize
  // allocation cost. This guarantees reading large buffers, and minimize the
  // number of calls to tryConsumeReadBuffer.
  auto minSize = std::max(readBuf_.tailroom(), minReadSize);

  auto [buf, len] =
      readBuf_.preallocate(minSize, kDefaultReadBufferAllocationSize);
  *lenP = len;
  *bufP = buf;
}

void RpcConnectionHandler::readDataAvailable(size_t len) noexcept {
  readBuf_.postallocate(len);
  tryConsumeReadBuffer();
}

bool RpcConnectionHandler::isBufferMovable() noexcept {
  // prefer to have getReadBuffer / readDataAvailable called
  // rather than readBufferAvailable.
  return true;
}

void RpcConnectionHandler::readBufferAvailable(
    std::unique_ptr<IOBuf> readBuf) noexcept {
  readBuf_.append(std::move(readBuf));
  tryConsumeReadBuffer();
}

void RpcConnectionHandler::readEOF() noexcept {
  // The socket was closed on us.
  //
  // For mountd, this is just a connection closing which is normal after every
  // request. We don't care about the stop date.
  //
  // For nfsd, this means the mountpoint was unmounted, so record the stop
  // reason as UNMOUNT.
  //
  // We intentionally "drop" this future. This is fine: there is no need to
  // block the caller on completing the shutdown. We need to update the
  // handler's state inline with this so that we could not have multiple
  // versions of shutdown running in parallel, but we can wait for all requests
  // to finish asynchronously from this call.
  if (!sock_) {
    XLOGF(ERR, "readEOF called after the socket was already closed");
    return;
  }
  XLOGF(ERR, "readEOF - the socket was closed on us. Deleting the reader");
  folly::futures::detachOn(
      sock_->getEventBase(), resetReader(RpcStopReason::UNMOUNT));
}

void RpcConnectionHandler::readErr(
    const folly::AsyncSocketException& ex) noexcept {
  XLOGF(ERR, "Error while reading: {}", folly::exceptionStr(ex));
  // Reading from the socket failed. There's nothing else to do, so
  // close the connection.  See the comment in readEOF() for more
  // context.
  if (!sock_) {
    XLOGF(ERR, "readErr called after the socket was already closed");
    return;
  }
  folly::futures::detachOn(
      sock_->getEventBase(), resetReader(RpcStopReason::ERROR));
}

void RpcConnectionHandler::writeErr(
    size_t /*bytesWritten*/,
    const folly::AsyncSocketException& ex) noexcept {
  // TODO: Should we assume the connection is broken, and we should close the
  // socket, aborting existing requests?
  XLOGF(ERR, "Error while writing: {}", folly::exceptionStr(ex));
}

RpcConnectionHandler::RpcConnectionHandler(
    std::shared_ptr<RpcServerProcessor> proc,
    AsyncSocket::UniquePtr&& socket,
    std::shared_ptr<folly::Executor> threadPool,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    std::weak_ptr<RpcServer> owningServer,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval)
    : proc_(proc),
      sock_(std::move(socket)),
      threadPool_(std::move(threadPool)),
      errorLogger_(edenFsEventsLogger),
      state_(sock_->getEventBase()),
      owningServer_(std::move(owningServer)),
      maximumInFlightRequests_(maximumInFlightRequests),
      highNfsRequestsLogInterval_(highNfsRequestsLogInterval) {
  sock_->setReadCB(this);
  proc_->clientConnected();
}

RpcConnectionHandler::~RpcConnectionHandler() {
  // Destroying sock_ closes it, and closing a socket calls its read callback's
  // readEOF(), which would run on this partly destroyed handler.
  if (sock_) {
    sock_->setReadCB(nullptr);
  }
}

folly::SemiFuture<folly::Unit> RpcConnectionHandler::takeoverStop() {
  XLOG(DBG4, "Takeover requested: locking state to change the status");
  // note its essential that this runs inline with the pending requests
  // check in resetReader. This ensures that we don't double set the pending
  // requests promise.
  {
    auto& state = state_.get();
    if (state.stopReason.has_value()) {
      // TODO: Ensure takeoverStop call sites handle exceptions
      // appropriately, and remove this makeSemiFutureWith.
      return folly::makeSemiFutureWith([&] {
        throwf<std::runtime_error>(
            "Takeover attempt failed: RpcServer already shutting down because {}",
            fmt::underlying(*state.stopReason));
      });
    }
  }
  XLOG(DBG4, "Stop reading from the socket");
  // as far as I can tell this will deliver all reads to the reader before this
  // completes. So we should not see any new requests after this point.
  // Note: it is important this is done inline with the caller. i.e. if we start
  // a graceful restart then move off the main event base thread and do some
  // work then comeback to the main eventbase and do this. In the time, we were
  // off the main eventbase a readErr or readEOF could have been sent that
  // triggered a forced shutdown. Then we could have a graceful restart and a
  // shutdown running in parallel! Bad! Technically maybe these could co-exist,
  // but as the code is currently written that will result in too many
  // getSemiFuture from a promise calls and trying to set a promise more than
  // once.
  sock_->setReadCB(nullptr);

  // Trigger the reader to shutdown now, this will shutdown the handler as well.
  return resetReader(RpcStopReason::TAKEOVER);
}

folly::SemiFuture<folly::Unit> RpcConnectionHandler::resetReader(
    RpcStopReason stopReason) {
  // The lifetimes here are tricky. resetReader() is called by AsyncSocket
  // callbacks under EOF or error conditions, and `this` must stay alive for the
  // duration of this callback.
  DestructorGuard dg{this};
  const auto stopRequestedAt = std::chrono::steady_clock::now();

  {
    auto& state = state_.get();

    // it is important that we do this inline with our caller, so that we
    // could not start a graceful restart and then start a forced shutdown.
    // i.e. during a forced shutdown if we move off the main event base do some
    // work, then come back to the main eventbase and update the stopReason.
    // Then a graceful restart could start in the time we are off the
    // mainEventBase then we come back here and start a forced restart. that
    // could lead to too many getSemiFuture calls and too many promise
    // fulfillments in the next bit of code.
    state.stopReason = stopReason;

    // If we have already finished processing all requests, we set the promise.
    // If we don't set the promise in this case we would get stuck forever
    // waiting on the pending requests future.
    // Note this must run on the main eventbase for the socket, and inline with
    // setting the stop reason This ensures that we don't accidentally set this
    // promise twice.
    XLOGF(DBG4, "Pending requests: {}", state.pendingRequests);
    if (state.pendingRequests == 0) {
      pendingRequestsComplete_.setValue();
    }
  }

  XLOG(DBG4, "waiting for pending requests to complete");
  return pendingRequestsComplete_.getFuture().ensure(
      [this, proc = proc_, dg = std::move(dg), stopReason, stopRequestedAt]() {
        XLOG(
            DBG4,
            "Pending requests complete; finishing destroying this RPC handler");
        this->sock_->getEventBase()->checkIsInEventBaseThread();
        if (auto owningServer = this->owningServer_.lock()) {
          owningServer->unregisterRpcHandler(this);
        }

        RpcStopData data{};
        data.reason = stopReason;
        data.stopRequestedAt = stopRequestedAt;
        if (stopReason == RpcStopReason::TAKEOVER) {
          // We've already set readCB to nullptr, so detach the
          // network socket and transfer it to the process taking over
          // the connection.
          data.socketToKernel =
              folly::File{this->sock_->detachNetworkSocket().toFd(), true};
          this->sock_.reset();
        }

        // We could move the onSocketClosed call earlier, but it
        // triggers a lot of destruction, so first we finish cleaning up our
        // socket reading and then trigger the socket closed callback.
        proc->onShutdown(std::move(data));
      });
}

namespace {
std::string displayBuffer(folly::IOBuf* buf) {
  auto bytes = buf->coalesce();
  return folly::hexDump(bytes.data(), bytes.size());
}

void serializeRpcMismatch(folly::io::QueueAppender& ser, uint32_t xid) {
  rpc_msg_reply reply{
      xid,
      msg_type::REPLY,
      reply_body{{
          reply_stat::MSG_DENIED,
          rejected_reply{{
              reject_stat::RPC_MISMATCH,
              mismatch_info{kRPCVersion, kRPCVersion},
          }},
      }},
  };

  XdrTrait<rpc_msg_reply>::serialize(ser, reply);
}

void serializeAuthError(
    folly::io::QueueAppender& ser,
    auth_stat auth,
    uint32_t xid) {
  rpc_msg_reply reply{
      xid,
      msg_type::REPLY,
      reply_body{{
          reply_stat::MSG_DENIED,
          rejected_reply{{
              reject_stat::AUTH_ERROR,
              auth,
          }},
      }},
  };

  XdrTrait<rpc_msg_reply>::serialize(ser, reply);
}

/**
 * Make an RPC fragment by computing the size of the iobufQueue.
 *
 * Return an IOBuf chain that can be directly written to a socket.
 */
std::unique_ptr<folly::IOBuf> finalizeFragment(
    std::unique_ptr<folly::IOBufQueue> iobufQueue) {
  auto chainLength = iobufQueue->chainLength();
  auto resultBuffer = iobufQueue->move();

  // Fill out the fragment header.
  auto len = uint32_t(chainLength - sizeof(uint32_t));
  auto fragment = (uint32_t*)resultBuffer->writableData();
  *fragment = folly::Endian::big(len | 0x80000000);

  XLOGF(
      DBG8,
      "Sending:\n{}",
      folly::hexDump(resultBuffer->data(), resultBuffer->length()));
  return resultBuffer;
}

/**
 * Rebuild a record received as several fragments into a single fragment, so
 * that the rest of the server only ever sees one fragment header followed by
 * the whole call. The payload buffers move from the record into the result;
 * only a buffer that holds a fragment boundary is shared, nothing is copied.
 */
std::unique_ptr<folly::IOBuf> coalesceFragments(
    std::unique_ptr<folly::IOBuf> record) {
  folly::IOBufQueue fragments{folly::IOBufQueue::cacheChainLength()};
  fragments.append(std::move(record));

  folly::IOBufQueue result{folly::IOBufQueue::cacheChainLength()};
  folly::io::QueueAppender appender(&result, sizeof(uint32_t));
  XdrTrait<uint32_t>::serialize(appender, 0); // fragment header placeholder

  while (true) {
    const auto fragmentHeader =
        folly::io::Cursor(fragments.front()).readBE<uint32_t>();
    fragments.trimStart(sizeof(uint32_t));
    if (const auto len = fragmentHeader & 0x7fffffff; len != 0) {
      result.append(fragments.split(len));
    }
    if ((fragmentHeader & 0x80000000) != 0) {
      break;
    }
  }

  auto len = uint32_t(result.chainLength() - sizeof(uint32_t));
  auto resultBuffer = result.move();
  auto fragment = (uint32_t*)resultBuffer->writableData();
  *fragment = folly::Endian::big(len | 0x80000000);
  return resultBuffer;
}
} // namespace

template <typename F>
void RpcConnectionHandler::writeInlineReply(F&& serializeBody) {
  auto iobufQueue = std::make_unique<folly::IOBufQueue>(
      folly::IOBufQueue::cacheChainLength());
  folly::io::QueueAppender ser(iobufQueue.get(), 64);
  XdrTrait<uint32_t>::serialize(ser, 0); // fragment header placeholder
  serializeBody(ser);
  sock_->writeChain(this, finalizeFragment(std::move(iobufQueue)));
}

void RpcConnectionHandler::tryConsumeReadBuffer() noexcept {
  // Iterate over all the complete fragments and dispatch these to the
  // threadPool_.
  while (true) {
    auto buf = readOneRequest();
    if (!buf) {
      break;
    }
    auto requestReceived = std::chrono::steady_clock::now();
    XLOG(DBG7, "received a request");

    // Pre-parse the RPC header for inline operations on the EventBase thread.
    auto peek = peekRpcCallHeader(*buf);

    std::unique_ptr<RequestPermit> permit;
    if (peek) {
      // Fast-path and backpressure: handle inline on the EventBase thread
      // when the fast-path config is enabled.
      if (proc_->shouldFastPathRPCs()) {
        if (peek->proc == 0) {
          writeInlineReply([&](auto& ser) {
            serializeReply(ser, accept_stat::SUCCESS, peek->xid);
          });
          XLOG(DBG7, "Fast-pathed null RPC reply");
          continue;
        }
        if (proc_->isUnimplementedProc(peek->proc)) {
          writeInlineReply([&](auto& ser) {
            serializeReply(ser, accept_stat::PROC_UNAVAIL, peek->xid);
          });
          XLOG(DBG7, "Fast-pathed PROC_UNAVAIL reply");
          continue;
        }

        // JUKEBOX backpressure: reject inline if the rate limiter denies
        // a permit. This sheds load on the EventBase thread before
        // requests queue in the thread pool.
        {
          auto result = proc_->tryInlineReject();
          if (result.rejected) {
            XDCHECK(!result.permit)
                << "tryInlineReject returned rejected=true with a non-null permit";
            writeInlineReply([&](auto& ser) {
              proc_->serializeInlineReject(peek->proc, peek->xid, ser);
            });
            XLOG(DBG7, "JUKEBOX backpressure: rejected inline");
            continue;
          }
          permit = std::move(result.permit);
        }
      }
    }

    RpcRequestTimeline timeline;
    timeline.requestReceived = requestReceived;

    auto should_log = false;
    {
      // state isn't actually locked, this scoping is just for show.
      // we skipped the lock since it's only ever accessed by one thread so a
      // lock is unnecessary.
      auto& state = state_.get();
      state.pendingRequests += 1;

      if (maximumInFlightRequests_ > 0 &&
          state.pendingRequests == maximumInFlightRequests_ &&
          requestReceived >=
              state.lastHighNfsRequestsLog_ + highNfsRequestsLogInterval_) {
        should_log = true;
        state.lastHighNfsRequestsLog_ = requestReceived;
      }
    }

    if (should_log) {
      if (errorLogger_) {
        errorLogger_->logEvent(ManyLiveFsChannelRequests{});
      }
    }

    timeline.dispatched = std::chrono::steady_clock::now();

    // Send the work to a thread pool to increase the number of inflight
    // requests that can be handled concurrently.
    threadPool_->add([this,
                      buf = std::move(buf),
                      guard = DestructorGuard(this),
                      permit = std::move(permit),
                      timeline]() mutable {
      timeline.handlerStart = std::chrono::steady_clock::now();
      XLOGF(DBG8, "Received:\n{}", displayBuffer(buf.get()));
      // We use a scope so that the cursor is not still around after we
      // delete part of the IOBuf later. Attempting to use this cursor
      // after mutating the buffer could result in bad memory accesses.
      {
        folly::io::Cursor c(buf.get());
        uint32_t fragmentHeader = c.readBE<uint32_t>();
        bool isLast = (fragmentHeader & 0x80000000) != 0;

        // readOneRequest() rebuilds multi-fragment records into a single
        // fragment, so the worker only ever sees the last fragment.
        XCHECK(isLast);
      }

      // Trim off the fragment header.
      // We need to upgrade to an IOBufQueue because the IOBuf here is
      // actually part of a chain. The first buffer in the chain may not
      // have the full fragment header. Thus we need to be trimming off the
      // whole chain and not just from the first buffer.
      //
      // For example, this IOBuf might be the head of a chain of two IOBufs,
      // and the first IOBuf only contains 2 bytes. Trimming the IOBuf
      // would fail in this case.
      folly::IOBufQueue bufQueue{};
      bufQueue.append(std::move(buf));
      bufQueue.trimStart(sizeof(uint32_t));

      dispatchAndReply(
          bufQueue.move(),
          std::move(guard),
          std::move(permit),
          std::move(timeline));
    });
  }
}

std::unique_ptr<folly::IOBuf> RpcConnectionHandler::readOneRequest() noexcept {
  if (!readBuf_.front()) {
    return nullptr;
  }
  folly::io::Cursor c(readBuf_.front());
  size_t fragmentCount = 0;
  while (true) {
    uint32_t fragmentHeader;
    if (!c.tryReadBE<uint32_t>(fragmentHeader)) {
      // We can't even read the fragment header, bail out.
      return nullptr;
    }
    auto len = fragmentHeader & 0x7fffffff;
    bool isLast = (fragmentHeader & 0x80000000) != 0;
    if (!c.canAdvance(len)) {
      // we don't have a complete request, so try again later
      return nullptr;
    }
    c.skip(len);
    fragmentCount++;
    if (isLast) {
      break;
    }
  }
  auto record = readBuf_.split(c.getCurrentPosition());
  if (fragmentCount == 1) {
    return record;
  }
  return coalesceFragments(std::move(record));
}

void RpcConnectionHandler::recordParsingError(
    const RpcParsingError& err,
    std::unique_ptr<folly::IOBuf> input) {
  std::string message = fmt::format(
      "{} during {}. Full request {}.",
      err.what(),
      err.getProcedureContext(),
      folly::hexlify(input->coalesce()));

  XLOG(ERR, message);

  if (errorLogger_) {
    errorLogger_->logEvent(
        NfsParsingError{
            folly::to<std::string>("FS", " - ", err.getProcedureContext()),
            message});
  }
}

void RpcConnectionHandler::replyServerError(
    accept_stat err,
    uint32_t xid,
    std::unique_ptr<folly::IOBufQueue>& outputBuffer) {
  // We don't know how much was already written to  the outputBuffer,
  // thus let's clear it and write an error onto it.
  outputBuffer->reset();
  folly::io::QueueAppender errSer(outputBuffer.get(), 1024);
  XdrTrait<uint32_t>::serialize(errSer, 0); // reserve space for fragment header
  serializeReply(errSer, err, xid);
}

namespace {
class RequestWriteCallback : public folly::AsyncWriter::WriteCallback {
  using DestructorGuard = folly::DelayedDestruction::DestructorGuard;

 public:
  RequestWriteCallback(
      RpcRequestTimeline timeline,
      RpcConnectionHandler* handler,
      DestructorGuard guard)
      : timeline_(std::move(timeline)),
        handler_(handler),
        guard_(std::move(guard)) {}

  void writeSuccess() noexcept override {
    timeline_.responseSent = std::chrono::steady_clock::now();
    handler_->recordPhaseTimings(timeline_);
    delete this;
  }

  void writeErr(
      size_t /*bytesWritten*/,
      const folly::AsyncSocketException& ex) noexcept override {
    XLOGF(
        ERR,
        "Write error in RequestWriteCallback: {}",
        folly::exceptionStr(ex));
    timeline_.responseSent = std::chrono::steady_clock::now();
    handler_->recordPhaseTimings(timeline_);
    delete this;
  }

 private:
  RpcRequestTimeline timeline_;
  RpcConnectionHandler* handler_;
  DestructorGuard guard_;
};
} // namespace

void RpcConnectionHandler::writeReply(folly::Try<ReplyResult> result) {
  if (result.hasException()) {
    XLOGF(
        DFATAL,
        "Unexpected exception in RPC response pipeline: {}",
        folly::exceptionStr(result.exception()));
    return;
  }
  auto [resultBuffer, tl] = std::move(result).value();
  XLOG(DBG7, "About to write to the socket.");
  auto* writeCb =
      new RequestWriteCallback(std::move(tl), this, DestructorGuard(this));
  sock_->writeChain(writeCb, std::move(resultBuffer));
}

void RpcConnectionHandler::finishRequest() {
  XLOG(DBG7, "Request complete");
  auto& state = state_.get();
  state.pendingRequests -= 1;
  XLOGF(DBG7, "{} more requests to process", state.pendingRequests);
  if (state.pendingRequests == 0 && state.stopReason.has_value()) {
    // We are shutting down and the last request has been handled, so signal
    // that all pending requests have completed.
    pendingRequestsComplete_.setValue();
  }
}

void RpcConnectionHandler::dispatchAndReply(
    std::unique_ptr<folly::IOBuf> input,
    DestructorGuard guard,
    std::unique_ptr<RequestPermit> permit,
    RpcRequestTimeline timeline) {
  auto reply =
      makeImmediateFutureWith([&]() mutable -> ImmediateFuture<ReplyResult> {
        folly::io::Cursor deser(input.get());
        rpc_msg_call call = XdrTrait<rpc_msg_call>::deserialize(deser);
        timeline.procNumber = call.cbody.proc;

        auto iobufQueue = std::make_unique<folly::IOBufQueue>(
            folly::IOBufQueue::cacheChainLength());
        folly::io::QueueAppender ser(iobufQueue.get(), 1024);
        XdrTrait<uint32_t>::serialize(
            ser, 0); // reserve space for fragment header

        if (call.cbody.rpcvers != kRPCVersion) {
          serializeRpcMismatch(ser, call.xid);
          timeline.handlerDone = std::chrono::steady_clock::now();
          return std::pair{
              finalizeFragment(std::move(iobufQueue)), std::move(timeline)};
        }

        // Parse the AUTH_SYS credential once, and share the result with
        // checkAuthentication and dispatchRpc. Processors whose
        // credential-consuming features are all off skip the parse
        // entirely (see shouldParseAuthSysCreds).
        std::optional<authsys_parms> authSysCreds;
        if (proc_->shouldParseAuthSysCreds()) {
          authSysCreds = parseAuthSysCreds(call.cbody.cred);
        }

        if (auto auth = proc_->checkAuthentication(call.cbody, authSysCreds);
            auth != auth_stat::AUTH_OK) {
          serializeAuthError(ser, auth, call.xid);
          timeline.handlerDone = std::chrono::steady_clock::now();
          return std::pair{
              finalizeFragment(std::move(iobufQueue)), std::move(timeline)};
        }

        XLOG(DBG7, "dispatching a request");
        auto fut = makeImmediateFutureWith([&]() mutable {
          return proc_->dispatchRpc(
              std::move(deser),
              std::move(ser),
              call.xid,
              call.cbody.prog,
              call.cbody.vers,
              call.cbody.proc,
              authSysCreds);
        });

        return std::move(fut).thenTry(
            [this,
             input = std::move(input),
             iobufQueue = std::move(iobufQueue),
             call = std::move(call),
             timeline = std::move(timeline)](
                folly::Try<folly::Unit> result) mutable -> ReplyResult {
              XLOG(DBG7, "Request done, sending response.");
              if (result.hasException()) {
                if (auto* err =
                        result.exception().get_exception<RpcParsingError>()) {
                  recordParsingError(*err, std::move(input));
                  replyServerError(
                      accept_stat::GARBAGE_ARGS, call.xid, iobufQueue);
                } else {
                  XLOGF(
                      WARN,
                      "Server failed to dispatch proc {} to {}:{}: {}",
                      call.cbody.proc,
                      call.cbody.prog,
                      call.cbody.vers,
                      folly::exceptionStr(*result.exception().get_exception()));

                  replyServerError(
                      accept_stat::SYSTEM_ERR, call.xid, iobufQueue);
                }
              }
              timeline.handlerDone = std::chrono::steady_clock::now();
              return {
                  finalizeFragment(std::move(iobufQueue)), std::move(timeline)};
            });
      });

  // Most requests complete inline, on this worker thread. Their reply only
  // needs the hop to the EventBase that owns the socket; re-queueing them on
  // the thread pool first, as the deferred path does, woke another worker for
  // nothing. The permit is released when the request has been accounted for.
  if (reply.isReady()) {
    sock_->getEventBase()->runInEventBaseThread(
        [this,
         result = std::move(reply).getTry(),
         guard = std::move(guard),
         permit = std::move(permit)]() mutable {
          // The request is accounted for even when the reply cannot be
          // written, as the deferred path's ensure() guarantees; otherwise
          // the pending count never reaches zero and shutdown waits forever.
          try {
            writeReply(std::move(result));
          } catch (...) {
            XLOGF(
                WARN,
                "failed to write an NFS reply: {}",
                folly::exceptionStr(std::current_exception()));
          }
          finishRequest();
        });
    return;
  }

  std::move(reply)
      .semi()
      // A deferred request is fulfilled on some other thread, such as a
      // backing store importer; finish serializing its reply on the thread
      // pool rather than there.
      .via(threadPool_.get())
      // Then move it back to the EventBase to write the result to the socket.
      .via(this->sock_->getEventBase())
      .then([this](folly::Try<ReplyResult> result) {
        writeReply(std::move(result));
      })
      .ensure([this, guard = std::move(guard), permit = std::move(permit)]() {
        (void)permit; // held for RAII lifetime, released when request completes
        finishRequest();
      });
}

void RpcConnectionHandler::recordPhaseTimings(
    const RpcRequestTimeline& t) noexcept {
  proc_->onRequestComplete(t);
}

void RpcServer::connectionAccepted(
    folly::NetworkSocket fd,
    const folly::SocketAddress& clientAddr,
    AcceptInfo /* info */) noexcept {
  XLOGF(DBG7, "Accepted connection from: {}", clientAddr.describe());
  auto socket = AsyncSocket::newSocket(evb_, fd);
  configureSocket(*socket);
  auto& state = state_.get();

  // EOF on any connection with a handler stops the server (see readEOF), so
  // a server that only supports a single client must refuse further
  // connections rather than accept them: otherwise any process able to
  // connect to this socket takes the server down by disconnecting again.
  const bool isExtraConnection = !state.connectionHandlers.empty();
  if (isExtraConnection) {
    proc_->onExtraConnection();
  }
  if (!proc_->acceptsMultipleConnections() && isExtraConnection) {
    proc_->onExtraConnectionRefused();
    XLOGF_EVERY_MS(
        WARN,
        60000,
        "Refusing connection from {}: this server already has its single supported client",
        clientAddr.describe());
    return;
  }

  state.connectionHandlers.push_back(
      RpcConnectionHandler::create(
          proc_,
          std::move(socket),
          threadPool_,
          edenFsEventsLogger_,
          weak_from_this(),
          maximumInFlightRequests_,
          highNfsRequestsLogInterval_));

  // The accept callback stays registered even for single-client (nfsd3)
  // servers: additional connection attempts are refused above rather than
  // by unregistering the callback, which is tricky to do safely here. The
  // callback is cleaned up on shutdown.
  //
  // TODO: Is it really tricky to unregister the accept callback? We could
  // call stopAccepting() here and removeAcceptCallback.
}

void RpcServer::acceptError(const std::exception& ex) noexcept {
  XLOGF(ERR, "acceptError: {}", folly::exceptionStr(ex));
}

void RpcServer::acceptStopped() noexcept {
  state_.get().acceptStopped = true;
}

bool RpcServerProcessor::shouldParseAuthSysCreds() {
  return true;
}

auth_stat RpcServerProcessor::checkAuthentication(
    const call_body& /*call_body*/,
    const std::optional<authsys_parms>& /*authSysCreds*/) {
  // Completely ignore authentication.
  // TODO: something reasonable here
  return auth_stat::AUTH_OK;
}

ImmediateFuture<folly::Unit> RpcServerProcessor::dispatchRpc(
    folly::io::Cursor /*deser*/,
    folly::io::QueueAppender /*ser*/,
    uint32_t /*xid*/,
    uint32_t /*progNumber*/,
    uint32_t /*progVersion*/,
    uint32_t /*procNumber*/,
    const std::optional<authsys_parms>& /*authSysCreds*/) {
  return folly::unit;
}

void RpcServerProcessor::onShutdown(RpcStopData) {}
void RpcServerProcessor::clientConnected() {}
void RpcServerProcessor::onExtraConnection() {}
void RpcServerProcessor::onExtraConnectionRefused() {}

std::shared_ptr<RpcServer> RpcServer::create(
    std::shared_ptr<RpcServerProcessor> proc,
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval,
    size_t socketBufferSize) {
  return std::shared_ptr<RpcServer>{
      new RpcServer{
          std::move(proc),
          evb,
          std::move(threadPool),
          edenFsEventsLogger,
          maximumInFlightRequests,
          highNfsRequestsLogInterval,
          socketBufferSize},
      [](RpcServer* p) { p->destroy(); }};
}

RpcServer::RpcServer(
    std::shared_ptr<RpcServerProcessor> proc,
    folly::EventBase* evb,
    std::shared_ptr<folly::Executor> threadPool,
    const std::shared_ptr<EdenFsEventsLogger>& edenFsEventsLogger,
    size_t maximumInFlightRequests,
    std::chrono::nanoseconds highNfsRequestsLogInterval,
    size_t socketBufferSize)
    : evb_(evb),
      threadPool_(threadPool),
      edenFsEventsLogger_(edenFsEventsLogger),
      serverSocket_(new AsyncServerSocket(evb_)),
      proc_(std::move(proc)),
      state_{evb},
      maximumInFlightRequests_{maximumInFlightRequests},
      highNfsRequestsLogInterval_{highNfsRequestsLogInterval},
      socketBufferSize_{socketBufferSize} {}

void RpcServer::configureSocket(folly::AsyncSocket& socket) {
  if (socketBufferSize_ == 0) {
    return;
  }
  // TCP sockets keep the kernel's buffers: they start out large enough, and
  // on Linux an explicit size turns off the kernel's autotuning of them.
  folly::SocketAddress address;
  try {
    socket.getLocalAddress(&address);
  } catch (const std::exception& ex) {
    XLOGF(WARN, "could not get the RPC socket's address: {}", ex.what());
    return;
  }
  if (address.getFamily() != AF_UNIX) {
    return;
  }
  int err = socket.setSendBufSize(socketBufferSize_);
  if (err == 0) {
    err = socket.setRecvBufSize(socketBufferSize_);
  }
  if (err != 0) {
    XLOGF(
        WARN,
        "failed to set the RPC socket buffer size to {}: {}",
        socketBufferSize_,
        folly::errnoStr(err));
  }
}

void RpcServer::destroy() {
  evb_->runInEventBaseThread([this] { delete this; });
}

void RpcServer::initialize(folly::SocketAddress addr) {
  evb_->checkIsInEventBaseThread();

  // Ask kernel to assign us a port on the loopback interface
  serverSocket_->bind(addr);
  serverSocket_->listen(1024);

  serverSocket_->addAcceptCallback(this, nullptr);
  serverSocket_->startAccepting();
}

void RpcServer::initializeConnectedSocket(folly::File socket) {
  XLOGF(DBG7, "Initializing server from connected socket: {}", socket.fd());
  // Note we don't initialize the accepting socket in this case. This is
  // meant for server that only ever has one connected socket (nfsd3). Since
  // we already have the one connected socket, we will not need the
  // accepting socket to make any more connections.
  auto asyncSocket = AsyncSocket::newSocket(
      evb_, folly::NetworkSocket::fromFd(socket.release()));
  configureSocket(*asyncSocket);
  auto& state = state_.get();
  state.connectionHandlers.push_back(
      RpcConnectionHandler::create(
          proc_,
          std::move(asyncSocket),
          threadPool_,
          edenFsEventsLogger_,
          weak_from_this(),
          maximumInFlightRequests_,
          highNfsRequestsLogInterval_));
}

void RpcServer::initializeServerSocket(folly::File socket) {
  evb_->checkIsInEventBaseThread();

  XLOGF(DBG7, "Initializing server from server socket: {}", socket.fd());

  serverSocket_->useExistingSocket(
      folly::NetworkSocket::fromFd(socket.release()));
  serverSocket_->addAcceptCallback(this, nullptr);
  serverSocket_->startAccepting();
}

void RpcServer::resumeAccepting() {
  evb_->checkIsInEventBaseThread();

  auto& state = state_.get();
  if (!state.acceptStopped) {
    return;
  }

  state.acceptStopped = false;

  serverSocket_->addAcceptCallback(this, nullptr);
  serverSocket_->startAccepting();
}

void RpcServer::unregisterRpcHandler(RpcConnectionHandler* handlerToErase) {
  auto& state = state_.get();
  auto& handlers = state.connectionHandlers;
  handlers.erase(
      std::remove_if(
          handlers.begin(),
          handlers.end(),
          [handlerToErase](RpcConnectionHandler::UniquePtr& handler) {
            return handler.get() == handlerToErase;
          }),
      handlers.end());
}

folly::SemiFuture<folly::File> RpcServer::takeoverStop() {
  auto& state = state_.get();

  XLOG(DBG7, "Removing accept callback");

  if (serverSocket_->getAccepting()) {
    serverSocket_->removeAcceptCallback(this, nullptr);
    XCHECK(state.acceptStopped)
        << "We always accept on the same primary socket EventBase, so it "
           "should be guaranteed that acceptStopped() ran synchronously.";

    // Removing the last accept callback implicitly paused accepting.
  }

  // No more connections will be made after this point.

  XLOG(DBG7, "calling takeover stop on handlers");
  // todo should this return the file descriptor for the socket?
  std::vector<RpcConnectionHandler::UniquePtr> handlers;
  handlers.swap(state.connectionHandlers);

  std::vector<folly::SemiFuture<folly::Unit>> futures;
  futures.reserve(handlers.size());
  for (auto& handler : handlers) {
    futures.emplace_back(handler->takeoverStop());
  }

  auto fd = serverSocket_->getNetworkSocket().toFd();
  return collectAll(futures)
      .via(evb_) // make sure we are running on the eventbase to do some more
                 // socket operations
      .thenValue([fd](auto&&) {
        if (fd == -1) {
          return folly::File{};
        }
        // TODO: This needs Windows-specific handling. folly::File and
        // NetworkSocket are not compatible on Windows.

        // The AsyncServerSocket owns the socket handle, so we can't
        // steal ownership here. Duplicate the existing fd and send
        // the duplicated fd to the taking-over process.
        return folly::File(fd, false).dupCloseOnExec();
      });
}

RpcServer::~RpcServer() = default;

folly::SocketAddress RpcServer::getAddr() const {
  return serverSocket_->getAddress();
}

} // namespace facebook::eden

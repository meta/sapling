/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/rpc/StreamClient.h"

#include <folly/Exception.h>
#include <folly/String.h>
#include <folly/logging/xlog.h>
#include "eden/fs/nfs/rpc/Rpc.h"

using folly::IOBuf;

namespace facebook::eden {

constexpr size_t kDefaultBufferSize = 1024;

StreamClient::StreamClient(folly::SocketAddress&& addr)
    : addr_(std::move(addr)) {}

void StreamClient::connect() {
  sockaddr_storage socketAddress;
  auto len = addr_.getAddress(&socketAddress);

  s_ = folly::netops::socket(addr_.getFamily(), SOCK_STREAM, IPPROTO_TCP);
  folly::checkUnixError(
      folly::netops::connect(s_, (sockaddr*)&socketAddress, len), "connect");
}

std::pair<std::unique_ptr<folly::IOBufQueue>, folly::io::QueueAppender>
StreamClient::serializeCallHeader(
    uint32_t progNumber,
    uint32_t progVersion,
    uint32_t procNumber) {
  auto buf = std::make_unique<folly::IOBufQueue>(
      folly::IOBufQueue::cacheChainLength());
  folly::io::QueueAppender appender(buf.get(), kDefaultBufferSize);

  XdrTrait<uint32_t>::serialize(
      appender, 0); // reserve space for fragment header
  rpc_msg_call call{
      nextXid_,
      msg_type::CALL,
      call_body{
          kRPCVersion,
          progNumber,
          progVersion,
          procNumber,
          opaque_auth{
              auth_flavor::AUTH_NONE,
              OpaqueBytes{},
          },
          opaque_auth{
              auth_flavor::AUTH_NONE,
              OpaqueBytes{},
          }}};
  XdrTrait<rpc_msg_call>::serialize(appender, call);

  return {std::move(buf), std::move(appender)};
}

uint32_t StreamClient::fillFrameAndSend(
    std::unique_ptr<folly::IOBufQueue> buf) {
  auto iobuf = buf->move();
  auto bytes = iobuf->coalesce();

  // Populate the TCP transport fragment header that was previous reserved in
  // serializeCallHeader.  The MSB is set if this is the final fragment.  The
  // remaining bits are the length of this fragment.  Since we send a single
  // fragment, we just set this to the overall the length and set the MSB.
  // We also subsract the size of the fragment header as it is not counted in
  // the fragment size.
  auto frameSize = (uint32_t*)bytes.data();
  *frameSize = folly::Endian::big<uint32_t>(
      uint32_t(bytes.size() - sizeof(uint32_t)) | 0x80000000);

  auto totalLen = bytes.size();
  auto data = bytes.data();
  XLOGF(DBG8, "sending:\n{}", folly::hexDump(data, totalLen));
  while (totalLen > 0) {
    auto len = folly::netops::send(s_, data, totalLen, 0);
    folly::checkUnixError(len, "send failed");
    XLOGF(DBG8, "sent {} bytes", len);
    totalLen -= len;
    data += len;
  }

  return nextXid_++;
}

std::tuple<std::unique_ptr<folly::IOBuf>, folly::io::Cursor, uint32_t>
StreamClient::receiveChunk() {
  while (true) {
    uint32_t frag;
    auto len = folly::netops::recv(s_, &frag, sizeof(frag), 0);
    folly::checkUnixError(len, "recv failed");
    if (len != sizeof(frag)) {
      throw std::runtime_error("short read when reading fragment header");
    }

    frag = folly::Endian::big(frag);
    XLOGF(DBG8, "resp frag: {:x}", frag);

    bool isLast = (frag & 0x80000000) != 0;
    auto fragLen = frag & 0x7fffffff;

    while (fragLen > 0) {
      auto [buf, bufLen] = readBuf_.preallocate(fragLen, 4096, 8192);

      len = folly::netops::recv(s_, buf, bufLen, 0);
      folly::checkUnixError(len, "recv failed");
      readBuf_.postallocate(len);
      fragLen -= len;
    }

    if (isLast) {
      break;
    }
  }

  auto buf = readBuf_.pop_front();
  XLOGF(DBG8, "recv:\n{}", folly::hexDump(buf->data(), buf->length()));
  folly::io::Cursor cursor(buf.get());

  rpc_msg_reply reply = XdrTrait<rpc_msg_reply>::deserialize(cursor);

  switch (reply.rbody.tag) {
    case reply_stat::MSG_ACCEPTED:
      switch (std::get<accepted_reply>(reply.rbody.v).stat) {
        case accept_stat::SUCCESS:
          return {std::move(buf), std::move(cursor), reply.xid};
        case accept_stat::PROG_UNAVAIL:
          throw std::runtime_error("PROG_UNAVAIL");
        case accept_stat::PROG_MISMATCH:
          throw std::runtime_error("PROG_MISMATCH");
        case accept_stat::PROC_UNAVAIL:
          throw std::runtime_error("PROC_UNAVAIL");
        case accept_stat::GARBAGE_ARGS:
          throw std::runtime_error("GARBAGE_ARGS");
        case accept_stat::SYSTEM_ERR:
          throw std::runtime_error("SYSTEM_ERR");
        default:
          throw std::runtime_error("invalid accept_stat value");
      }
    case reply_stat::MSG_DENIED:
      throw std::runtime_error("MSG_DENIED");
    default:
      throw std::runtime_error("invalid reply_stat value");
  }
}

} // namespace facebook::eden

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/testharness/FakeFuse.h"

#include <folly/Exception.h>
#include <folly/FileUtil.h>
#include <folly/chrono/Conv.h>
#include <folly/logging/xlog.h>
#include <folly/portability/Sockets.h>
#include <sys/types.h>

using namespace std::chrono_literals;
using folly::ByteRange;
using folly::StringPiece;

namespace facebook::eden {

FakeFuse::FakeFuse() = default;

folly::File FakeFuse::start() {
  std::array<int, 2> sockets;
  folly::checkUnixError(
      socketpair(AF_UNIX, SOCK_SEQPACKET, 0, sockets.data()),
      "socketpair() failed");
  conn_ = folly::File(sockets[0], /* ownsFd */ true);
  auto userConn = folly::File(sockets[1], /* ownsFd */ true);

  // Set a timeout so the tests will fail quickly if we don't have
  // data ready when we expect to.
  setTimeout(1s);

  started_.store(true, std::memory_order_release);
  return userConn;
}

void FakeFuse::close() {
  conn_.close();
  started_.store(false, std::memory_order_release);
}

bool FakeFuse::isStarted() const {
  return started_.load(std::memory_order_acquire);
}

void FakeFuse::setTimeout(std::chrono::milliseconds timeout) {
  auto tv = folly::to<struct timeval>(timeout);
  // recvResponse() and sendRequest() both perform blocking I/O.
  // We simply set to the socket timeout to force the blocking recv/send calls
  // to time out if they do not complete within the specified timeout.
  folly::checkUnixError(
      setsockopt(conn_.fd(), SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)));
  folly::checkUnixError(
      setsockopt(conn_.fd(), SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)));
}

uint32_t FakeFuse::sendRequest(uint32_t opcode, uint64_t inode, ByteRange arg) {
  auto requestID = requestID_;
  ++requestID_;
  XLOGF(DBG5, "injecting FUSE request ID {}: opcode= {}", requestID, opcode);

  fuse_in_header header = {};
  header.len = sizeof(struct fuse_in_header) + arg.size();
  header.opcode = opcode;
  header.unique = requestID;
  header.nodeid = inode;

  std::array<iovec, 2> iov;
  iov[0].iov_base = &header;
  iov[0].iov_len = sizeof(struct fuse_in_header);
  iov[1].iov_base = const_cast<uint8_t*>(arg.data());
  iov[1].iov_len = arg.size();

  folly::checkUnixError(
      folly::writevFull(conn_.fd(), iov.data(), iov.size()),
      "failed to send FUSE request ");
  return requestID;
}

namespace {
// recvmsg() here only blocks when a reply is slow to arrive, and a blocked
// call can be interrupted by any signal the process happens to use. Retry
// rather than turning that into a test failure; SO_RCVTIMEO still bounds how
// long this can spin.
ssize_t recvmsgRetryOnEintr(int fd, msghdr* message, int flags) {
  while (true) {
    ssize_t bytesRead = recvmsg(fd, message, flags);
    if (bytesRead >= 0 || errno != EINTR) {
      return bytesRead;
    }
    XLOG(DBG5, "recvmsg on FUSE socket interrupted; retrying");
  }
}
} // namespace

FakeFuse::Response FakeFuse::recvResponse() {
  Response response;

  std::array<iovec, 2> iov;
  iov[0].iov_base = &response.header;
  iov[0].iov_len = sizeof(response.header);

  msghdr message{};
  message.msg_iov = iov.data();
  message.msg_iovlen = 1;

  ssize_t bytesRead = recvmsgRetryOnEintr(conn_.fd(), &message, MSG_PEEK);
  folly::checkUnixError(bytesRead, "recvmsg failed on FUSE socket");

  if (static_cast<size_t>(bytesRead) < sizeof(fuse_out_header)) {
    throw std::runtime_error{folly::to<std::string>(
        "received FUSE response with incomplete header: ",
        bytesRead,
        " is shorter than the response header.")};
  }

  const auto packetLength = response.header.len;
  if (packetLength < sizeof(fuse_out_header)) {
    throw std::runtime_error{folly::to<std::string>(
        "received FUSE response with invalid length: ",
        packetLength,
        " is shorter than the response header.")};
  }

  const auto dataLength = packetLength - sizeof(fuse_out_header);
  response.body.resize(dataLength);

  iov[1].iov_base = response.body.data();
  iov[1].iov_len = dataLength;
  message.msg_iovlen = 2;

  bytesRead = recvmsgRetryOnEintr(conn_.fd(), &message, 0);
  folly::checkUnixError(bytesRead, "recvmsg failed on FUSE socket");

  if (bytesRead != packetLength) {
    throw std::runtime_error{folly::to<std::string>(
        "received FUSE response with incorrect message size: ",
        bytesRead,
        " expected ",
        packetLength)};
  }

  return response;
}

std::vector<FakeFuse::Response> FakeFuse::getAllResponses() {
  std::vector<FakeFuse::Response> responses;
  try {
    while (true) {
      responses.emplace_back(recvResponse());
    }
  } catch (const std::system_error& e) {
    if (e.code().value() != EAGAIN) {
      throw;
    }
  }
  return responses;
}

uint32_t FakeFuse::sendInitRequest(
    uint32_t majorVersion,
    uint32_t minorVersion,
    uint32_t maxReadahead,
    uint32_t flags) {
  struct fuse_init_in initArg = {};
  initArg.major = majorVersion;
  initArg.minor = minorVersion;
  initArg.max_readahead = maxReadahead;
  initArg.flags = flags;

  return sendRequest(FUSE_INIT, FUSE_ROOT_ID, initArg);
}

uint32_t FakeFuse::sendLookup(uint64_t inode, StringPiece pathComponent) {
  return sendRequest(FUSE_LOOKUP, inode, folly::ByteRange(pathComponent));
}

} // namespace facebook::eden

#endif

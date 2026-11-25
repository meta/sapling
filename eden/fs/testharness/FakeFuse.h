/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/File.h>
#include <folly/Range.h>
#include <atomic>
#include <chrono>
#include <vector>

#include "eden/fs/utils/FsChannelTypes.h"

namespace facebook::eden {

#ifndef _WIN32

/**
 * FakeFuse helps implement a fake FUSE device.
 *
 * FakeFuse is implemented internally as a socket pair.  One side behaves like
 * the user-space side of a FUSE channel, and the other side behaves like the
 * kernel-space side.  Test harness code can control the kernel-space side of
 * the connection to exercise the EdenMount object that has the user-space side
 * of the connection.
 */
class FakeFuse {
 public:
  struct Response {
    fuse_out_header header;
    std::vector<uint8_t> body;
  };

  FakeFuse();

  /**
   * Start this FakeFuse device, and return the FUSE file descriptor to use to
   * communicate with it.
   */
  folly::File start();
  bool isStarted() const;

  /**
   * Explicitly close the FUSE descriptor.
   *
   * The destructor will automatically close the descriptor, but this can be
   * used to trigger the close before the FakeFuse object itself is destroyed.
   */
  void close();

  /**
   * Set the timeout for this FakeFuse object.
   *
   * This will cause recvResponse() to fail with an error if the FUSE
   * implementation does not send a response within the specified timeout.
   * Similarly, sendRequest() will fail with a timeout if the request cannot be
   * written within the given timeout.
   */
  void setTimeout(std::chrono::milliseconds timeout);

  /**
   * Send a new request on the FUSE channel.
   *
   * Returns the newly allocated request ID.
   */
  template <typename ArgType>
  uint32_t sendRequest(uint32_t opcode, uint64_t inode, const ArgType& arg) {
    folly::ByteRange argBytes(
        reinterpret_cast<const uint8_t*>(&arg), sizeof(arg));
    return sendRequest(opcode, inode, argBytes);
  }
  uint32_t sendRequest(uint32_t opcode, uint64_t inode, folly::ByteRange arg);

  Response recvResponse();

  /**
   * Get all the responses until the channel is empty
   */
  std::vector<Response> getAllResponses();

  /**
   * Send an INIT request.
   *
   * Returns the unique request ID.
   */
  uint32_t sendInitRequest(
      uint32_t majorVersion = FUSE_KERNEL_VERSION,
      uint32_t minorVersion = FUSE_KERNEL_MINOR_VERSION,
      uint32_t maxReadahead = 0,
      uint32_t flags = 0);

  uint32_t sendLookup(uint64_t inode, folly::StringPiece pathComponent);

 private:
  FakeFuse(FakeFuse const&) = delete;
  FakeFuse& operator=(FakeFuse const&) = delete;

  /**
   * Our end of the FUSE channel.
   * We pretend to be the kernel-side of the FUSE connection.  We can use this
   * connection to send requests to the EdenMount on the other side.
   */
  folly::File conn_;

  // Set by start() (on the mount thread) once conn_ is ready; isStarted()
  // polls it from the test thread.
  std::atomic<bool> started_{false};

  /**
   * The next request ID to use when sending requests.
   * We increment this for each request we send.
   */
  uint32_t requestID_{0};
};

#endif

} // namespace facebook::eden

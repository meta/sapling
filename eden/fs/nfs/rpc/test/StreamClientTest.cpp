/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/nfs/rpc/StreamClient.h"

#include <system_error>

#include <gtest/gtest.h>

namespace {

using namespace facebook::eden;

TEST(StreamClientTest, send_on_unconnected_client_throws) {
  folly::SocketAddress addr;
  addr.setFromIpPort("::1", 1);
  StreamClient client{std::move(addr)};

  // The call is framed before it is sent, so this frames a whole call; only
  // the send fails, as the client never connected.
  EXPECT_THROW(
      client.serializeCall(100005, 3, 0, uint32_t{0}), std::system_error);
}

} // namespace

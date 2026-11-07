/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include "eden/fs/inodes/RequestContext.h"
#include "eden/fs/nfs/NfsdRpc.h"
#include "eden/fs/nfs/rpc/Rpc.h"
#include "eden/fs/telemetry/EdenFsEventsLogger.h"

namespace facebook::eden {

class NfsRequestContext : public RequestContext {
 public:
  /**
   * Constructs a new NfsRequestContext. The context should live for the
   * duration of the NFS request.
   * `startRequest` should be called at the beginning and `finishRequest` at the
   * end of the request. `proc` identifies the NFS procedure used as the
   * fetch cause detail.
   *
   * When the request carried a parsable AUTH_SYS credential, `authSysCreds`
   * holds it and the client uid/gid are exposed through the fetch context's
   * getClientUid/getClientGid. The credential is copied into the context, so
   * the reference only needs to be valid for the duration of this
   * constructor.
   */
  explicit NfsRequestContext(
      uint32_t xid,
      nfsv3Procs proc,
      std::shared_ptr<ProcessAccessLog> processAccessLog,
      std::shared_ptr<EdenFsEventsLogger> edenFsEventsLogger,
      std::chrono::nanoseconds longRunningFsRequestThreshold,
      const std::optional<authsys_parms>& authSysCreds = std::nullopt);

  NfsRequestContext(const NfsRequestContext&) = delete;
  NfsRequestContext& operator=(const NfsRequestContext&) = delete;
  NfsRequestContext(NfsRequestContext&&) = delete;
  NfsRequestContext& operator=(NfsRequestContext&&) = delete;

  uint32_t getXid() const {
    return xid_;
  }

  /**
   * Whether this LOOKUP found no entry. A negative lookup hands the client no
   * handle, so it does not count as resolving the directory's entries.
   */
  bool isNegativeLookup() const {
    return negativeLookup_;
  }

  void markNegativeLookup() {
    negativeLookup_ = true;
  }

 private:
  uint32_t xid_;
  bool negativeLookup_{false};
};

} // namespace facebook::eden

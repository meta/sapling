/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/store/ObjectFetchContext.h"
#include <folly/CppAttributes.h>
#include "eden/fs/utils/MiniTracer.h"
#include "eden/fs/utils/SourceLocation.h"

namespace {

using namespace facebook::eden;

class NullObjectFetchContext : public ObjectFetchContext {
 public:
  NullObjectFetchContext() = default;

  explicit NullObjectFetchContext(CauseDetail causeDetail)
      : causeDetail_(std::move(causeDetail)) {}

  Cause getCause() const override {
    return Cause::Unknown;
  }

  std::optional<std::string_view> getCauseDetail() const override {
    return causeDetail_.asStringView();
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }

 private:
  CauseDetail causeDetail_;
};

class NullFSObjectFetchContext : public ObjectFetchContext {
 public:
  NullFSObjectFetchContext() = default;

  Cause getCause() const override {
    return Cause::Fs;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }
};

class NullPrefetchObjectFetchContext : public ObjectFetchContext {
 public:
  NullPrefetchObjectFetchContext() = default;

  Cause getCause() const override {
    return Cause::Prefetch;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }
};

class NullInodeGcObjectFetchContext : public ObjectFetchContext {
 public:
  NullInodeGcObjectFetchContext() = default;

  Cause getCause() const override {
    return Cause::InodeGc;
  }

  const std::unordered_map<std::string, std::string>* FOLLY_NULLABLE
  getRequestInfo() const override {
    return nullptr;
  }
};

} // namespace

namespace facebook::eden {

ObjectFetchContext::StaticCauseDetail
ObjectFetchContext::StaticCauseDetail::fromSourceLocation(
    SourceLocation sourceLocation) noexcept {
  return StaticCauseDetail{sourceLocation.function_name()};
}

ObjectFetchContextPtr ObjectFetchContext::getNullContext() {
  static auto* p = new NullObjectFetchContext;
  return ObjectFetchContextPtr::singleton(*p);
}

ObjectFetchContextPtr ObjectFetchContext::getNullContextWithCauseDetail(
    CauseDetail causeDetail) {
  return ObjectFetchContextPtr::singleton(
      *new NullObjectFetchContext{std::move(causeDetail)});
}

ObjectFetchContextPtr ObjectFetchContext::getNullFsContext() {
  static auto* p = new NullFSObjectFetchContext;
  return ObjectFetchContextPtr::singleton(*p);
}

ObjectFetchContextPtr ObjectFetchContext::getNullPrefetchContext() {
  static auto* p = new NullPrefetchObjectFetchContext;
  return ObjectFetchContextPtr::singleton(*p);
}

ObjectFetchContextPtr ObjectFetchContext::getNullInodeGcContext() {
  static auto* p = new NullInodeGcObjectFetchContext;
  return ObjectFetchContextPtr::singleton(*p);
}

} // namespace facebook::eden

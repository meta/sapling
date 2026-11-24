/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/futures/Future.h>
#include <rust/cxx.h>
#include <memory>

namespace facebook::eden {

struct MercurialMatcher;

class MatcherPromise {
 public:
  explicit MatcherPromise(folly::Promise<rust::Box<MercurialMatcher>> matcher)
      : promise(std::move(matcher)) {}

  folly::Promise<rust::Box<MercurialMatcher>> promise;
};

void set_matcher_promise_result(
    std::unique_ptr<MatcherPromise> promise,
    rust::Box<::facebook::eden::MercurialMatcher>);

void set_matcher_promise_error(
    std::unique_ptr<MatcherPromise> promise,
    rust::String error);

} // namespace facebook::eden

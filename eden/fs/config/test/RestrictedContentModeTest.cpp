/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/config/RestrictedContentMode.h"

#include <gtest/gtest.h>

#include "eden/fs/config/EdenConfig.h"
#include "eden/fs/config/FieldConverter.h"

using namespace facebook::eden;

TEST(RestrictedContentModeTest, fromString) {
  FieldConverter<RestrictedContentMode> converter;
  std::map<std::string, std::string> convData;

  EXPECT_EQ(
      RestrictedContentMode::Restricted,
      converter.fromString("restricted", convData).value());
  EXPECT_EQ(
      RestrictedContentMode::Omitted,
      converter.fromString("omitted", convData).value());
  EXPECT_TRUE(converter.fromString("bogus", convData).hasError());
}

TEST(RestrictedContentModeTest, defaultIsOmitted) {
  auto edenConfig = EdenConfig::createTestEdenConfig();
  EXPECT_EQ(
      RestrictedContentMode::Omitted,
      edenConfig->restrictedContentMode.getValue());
}

/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/utils/MountInfoTable.h"
#include "eden/fs/utils/Statmount.h"

#include <linux/filter.h>
#include <linux/seccomp.h>
#include <sys/prctl.h>
#include <unistd.h>

#include <cerrno>
#include <cstring>

#include <folly/portability/GTest.h>

namespace facebook::eden {
namespace {

constexpr MountInfoOptions kAllFields{
    .includeMountSource = true,
    .includeMountOptions = true,
    .includeMountId = true};
constexpr uint64_t kBasicMask = STATMOUNT_SB_BASIC | STATMOUNT_MNT_ROOT |
    STATMOUNT_MNT_POINT | STATMOUNT_FS_TYPE;

TEST(MountInfoTableTest, statmountRequiresAllRequestedFields) {
  std::vector<char> buffer(sizeof(struct statmount) + 1);
  auto& sm = *reinterpret_cast<struct statmount*>(buffer.data());
  sm.mask = kBasicMask;

  // Linux 6.9 returns these fields, but not the source or mount options.
  EXPECT_TRUE(detail::parseStatmount(sm, {}).hasValue());
  EXPECT_EQ(EOPNOTSUPP, detail::parseStatmount(sm, kAllFields).error());
  EXPECT_EQ(
      EOPNOTSUPP,
      detail::parseStatmount(sm, {.includeMountSource = true}).error());
  EXPECT_EQ(
      EOPNOTSUPP,
      detail::parseStatmount(sm, {.includeMountOptions = true}).error());

  sm.mask |= STATMOUNT_SB_SOURCE;
  EXPECT_TRUE(
      detail::parseStatmount(sm, {.includeMountSource = true}).hasValue());
  EXPECT_EQ(EOPNOTSUPP, detail::parseStatmount(sm, kAllFields).error());

  sm.mask |= STATMOUNT_MNT_OPTS | STATMOUNT_MNT_BASIC;
  EXPECT_TRUE(detail::parseStatmount(sm, kAllFields).hasValue());
  for (auto field :
       {STATMOUNT_SB_BASIC,
        STATMOUNT_MNT_ROOT,
        STATMOUNT_MNT_POINT,
        STATMOUNT_FS_TYPE}) {
    sm.mask &= ~field;
    EXPECT_EQ(EOPNOTSUPP, detail::parseStatmount(sm, {}).error());
    sm.mask |= field;
  }
  sm.mask &= ~STATMOUNT_MNT_BASIC;
  EXPECT_TRUE(detail::parseStatmount(sm, {}).hasValue());
  EXPECT_EQ(
      EOPNOTSUPP, detail::parseStatmount(sm, {.includeMountId = true}).error());
}

TEST(MountInfoTableTest, parsesStatmountMetadata) {
  constexpr char strings[] =
      "/data/users/gzuo/fbsource\0fuse\0edenfs:\0user_id=229400\0/\0";
  std::vector<char> buffer(sizeof(struct statmount) + sizeof(strings));
  auto& sm = *reinterpret_cast<struct statmount*>(buffer.data());
  sm.mask = kBasicMask | STATMOUNT_MNT_BASIC | STATMOUNT_SB_SOURCE |
      STATMOUNT_MNT_OPTS;
  sm.sb_dev_minor = 344;
  sm.mnt_id = 0x100000065;
  sm.mnt_id_old = 101;
  std::memcpy(sm.str, strings, sizeof(strings));
  sm.fs_type = static_cast<uint32_t>(std::strlen(sm.str) + 1);
  sm.sb_source =
      static_cast<uint32_t>(sm.fs_type + std::strlen(sm.str + sm.fs_type) + 1);
  sm.mnt_opts = static_cast<uint32_t>(
      sm.sb_source + std::strlen(sm.str + sm.sb_source) + 1);

  sm.mnt_root = static_cast<uint32_t>(
      sm.mnt_opts + std::strlen(sm.str + sm.mnt_opts) + 1);

  auto result = detail::parseStatmount(sm, kAllFields);
  ASSERT_TRUE(result.hasValue());
  EXPECT_EQ(0, detail::parseStatmount(sm, {})->mountId);
  EXPECT_EQ(101, result->mountId);
  EXPECT_EQ(0, result->devMajor);
  EXPECT_EQ(344, result->devMinor);
  EXPECT_EQ("/", result->mountRoot);
  EXPECT_EQ("/data/users/gzuo/fbsource", result->mountPoint);
  EXPECT_EQ("fuse", result->fsType);
  EXPECT_EQ("edenfs:", result->mountSource);
  EXPECT_EQ("user_id=229400", result->mountOptions);
}

bool listmountSupported() {
  struct mnt_id_req req{};
  req.size = MNT_ID_REQ_SIZE_VER0;
  req.mnt_id = LSMT_ROOT;
  uint64_t id = 0;
  return syscall(__NR_listmount, &req, &id, 1, 0) >= 0;
}

TEST(MountInfoTableTest, skipsMountsUnmountedAfterListing) {
  if (!listmountSupported()) {
    GTEST_SKIP() << "listmount(2) is not supported";
  }
  // Every mount looks unmounted after listmount returned it.
  ASSERT_EXIT(
      ([] {
        sock_filter filter[] = {
            BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(seccomp_data, nr)),
            BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_statmount, 0, 1),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | ENOENT),
            BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
        };
        sock_fprog program{
            static_cast<unsigned short>(std::size(filter)), filter};
        if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
            prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) != 0) {
          _exit(1);
        }
        auto mounts = getAllMounts(kAllFields);
        auto root = getMountInfoForPath("/", kAllFields);
        _exit(
            mounts.hasValue() && mounts->empty() && root.hasValue() &&
                    !root->has_value()
                ? 0
                : 2);
      }()),
      ::testing::ExitedWithCode(0),
      "");
}

TEST(MountInfoTableTest, fallsBackOnlyForUnsupportedOrOversizedResults) {
  // Keep syscall filters in child processes so other tests are unaffected.
  for (const uint32_t error : {ENOSYS, EOPNOTSUPP, EOVERFLOW, EPERM}) {
    SCOPED_TRACE(error);
    ASSERT_EXIT(
        ([error] {
          sock_filter filter[] = {
              BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(seccomp_data, nr)),
              BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_statmount, 1, 0),
              BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, __NR_listmount, 0, 1),
              BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | error),
              BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
          };
          sock_fprog program{
              static_cast<unsigned short>(std::size(filter)), filter};
          if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 ||
              prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program) != 0) {
            _exit(1);
          }
          auto mounts = getAllMounts(kAllFields);
          auto root = getMountInfoForPath("/", kAllFields);
          if (error == EPERM) {
            _exit(
                mounts.hasError() && mounts.error() == EPERM &&
                        root.hasError() && root.error() == EPERM
                    ? 0
                    : 2);
          }
          _exit(
              mounts.hasValue() && !mounts->empty() && root.hasValue() &&
                      root->has_value() && root->value().mountPoint == "/" &&
                      !root->value().mountRoot.empty() &&
                      !root->value().fsType.empty()
                  ? 0
                  : 3);
        }()),
        ::testing::ExitedWithCode(0),
        "");
  }
}

} // namespace
} // namespace facebook::eden

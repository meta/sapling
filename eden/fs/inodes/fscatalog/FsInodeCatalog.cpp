/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/inodes/fscatalog/FsInodeCatalog.h"

#include <algorithm>
#include <array>
#include <bit>
#include <new>

#include <boost/filesystem.hpp>

#include <dirent.h>

#include <folly/Conv.h>
#include <folly/Exception.h>
#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/Range.h>
#include <folly/String.h>
#include <folly/Varint.h>
#include <folly/container/F14Map.h>
#include <folly/io/Cursor.h>
#include <folly/io/IOBuf.h>
#include <folly/lang/ToAscii.h>
#include <folly/logging/xlog.h>
#include <thrift/lib/cpp2/protocol/Serializer.h>

#include <thrift/lib/cpp2/protocol/CompactProtocol.h>

#include "eden/common/utils/Bug.h"
#include "eden/common/utils/FileUtils.h"
#include "eden/common/utils/PathFuncs.h"
#include "eden/common/utils/Throw.h"
#include "eden/fs/inodes/fscatalog/InodePath.h"
#include "eden/fs/model/TreeEntry.h"
#include "eden/fs/service/gen-cpp2/eden_types.h"
#include "eden/fs/utils/EdenError.h"

namespace facebook::eden {

using apache::thrift::CompactSerializer;
using folly::ByteRange;
using folly::fbvector;
using folly::File;
using folly::IOBuf;
using folly::MutableStringPiece;
using folly::StringPiece;
using folly::literals::string_piece_literals::operator""_sp;
using std::optional;
using std::string;

/* Relative to the localDir, the metaFile holds the serialized rendition
 * of the overlay_ data.  We use thrift CompactSerialization for this.
 */
constexpr StringPiece kInfoFile{"info"};
constexpr const char* kNextInodeNumberFile{"next-inode-number"};

/**
 * 4-byte magic identifier to put at the start of the info file.
 * This merely helps confirm that we are in fact reading an overlay info file
 */
constexpr StringPiece kInfoHeaderMagic{"\xed\xe0\x00\x01"};
constexpr StringPiece kShardedTmpDirName{"sharded_tmp"};

/**
 * A version number for the overlay directory format.
 *
 * If we change the overlay storage format in the future we can bump this
 * version number to help identify when eden is reading overlay data created by
 * an older version of the code.
 */
constexpr uint32_t kOverlayVersion = 1;
constexpr size_t kInfoHeaderSize =
    kInfoHeaderMagic.size() + sizeof(kOverlayVersion);
constexpr size_t kWalAclRootStateTailSize = sizeof(uint8_t) + sizeof(uint8_t);

bool getWalEntryIsRestricted(const overlay::OverlayEntry& entry) {
  return apache::thrift::is_non_optional_field_set_manually_or_by_serializer(
             entry.isRestricted())
      ? *entry.isRestricted()
      : false;
}

AclRootState getWalEntryAclRootState(const overlay::OverlayEntry& entry) {
  if (auto state = entry.aclRootState()) {
    if (auto aclRootState = aclRootStateFromInt(*state)) {
      return *aclRootState;
    }
    XLOGF(
        WARN,
        "Invalid WAL ACL root state {}; falling back to legacy isRestricted",
        *state);
  }
  return makeAclRootState(
      getWalEntryIsRestricted(entry), std::optional<bool>{std::nullopt});
}

static void doFormatSubdirPath(
    uint64_t inodeNum,
    MutableStringPiece subdirPath) {
  constexpr char hexdigit[] = "0123456789abcdef";
  XDCHECK_EQ(subdirPath.size(), FsFileContentStore::kShardDirPathLength);
  subdirPath[0] = hexdigit[(inodeNum >> 4) & 0xf];
  subdirPath[1] = hexdigit[inodeNum & 0xf];
}

bool FsInodeCatalog::initialized() const {
  return core_->initialized();
}

void FsFileContentStore::formatSubdirPath(
    InodeNumber inodeNum,
    MutableStringPiece subdirPath) {
  return doFormatSubdirPath(inodeNum.get(), subdirPath);
}

void FsFileContentStore::formatSubdirShardPath(
    ShardID shardID,
    MutableStringPiece subdirPath) {
  XDCHECK_LE(shardID, 0xfful);
  return doFormatSubdirPath(shardID, subdirPath);
}

bool FsFileContentStore::initialize(
    bool createIfNonExisting,
    bool bypassLockFile) {
  // Read the info file.
  auto infoPath = localDir_ + PathComponentPiece{kInfoFile};
  int fd = folly::openNoInt(infoPath.value().c_str(), O_RDONLY | O_CLOEXEC);
  bool overlayCreated{false};
  if (fd >= 0) {
    // This is an existing overlay directory.
    // Read the info file and make sure we are compatible with its version.
    infoFile_ = File{fd, /* ownsFd */ true};
    validateExistingOverlay(infoFile_.fd());
  } else if (errno != ENOENT) {
    folly::throwSystemError(
        "error reading eden overlay info file ", infoPath.view());
  } else {
    if (!createIfNonExisting) {
      folly::throwSystemError("overlay does not exist at ", localDir_.view());
    }
    // This is a brand new overlay directory.
    initNewOverlay();
    infoFile_ = File{infoPath.value().c_str(), O_RDONLY | O_CLOEXEC};
    overlayCreated = true;
  }

  if (!infoFile_.try_lock() && !bypassLockFile) {
    folly::throwSystemError(
        "failed to acquire overlay lock on ", infoPath.view());
  }

  // Open a handle on the overlay directory itself
  int dirFd = open(
      localDir_.c_str(),
      O_RDONLY |
#ifdef O_PATH
          O_PATH |
#endif
          O_DIRECTORY | O_CLOEXEC);
  folly::checkUnixError(
      dirFd, "error opening overlay directory handle for ", localDir_.value());
  dirFile_ = File{dirFd, /* ownsFd */ true};

  if (!overlayCreated) {
    // Ensure sharded tmp directory and its subdirectories exist for existing
    // overlays that were created before sharded tmp was introduced.
    ensureShardedTmpDirectories(dirFile_.fd());
  }

  return overlayCreated;
}

std::optional<InodeNumber> FsInodeCatalog::initOverlay(
    bool createIfNonExisting,
    bool bypassLockFile) {
  bool overlayCreated = core_->initialize(createIfNonExisting, bypassLockFile);

  if (overlayCreated) {
    return InodeNumber{kRootNodeId.get() + 1};
  }
  return core_->tryLoadNextInodeNumber();
}

void FsFileContentStore::ensureShardDirectories(int parentDirFd, mode_t mode) {
  std::array<char, kShardDirPathLength + 1> subdirPath{};
  subdirPath[kShardDirPathLength] = '\0';
  MutableStringPiece subdirStringPiece{subdirPath.data(), kShardDirPathLength};
  for (ShardID n = 0; n < kNumShards; ++n) {
    formatSubdirShardPath(n, subdirStringPiece);
    auto result = ::mkdirat(parentDirFd, subdirPath.data(), mode);
    if (result != 0 && errno != EEXIST) {
      folly::throwSystemError(
          "error creating eden overlay shard directory ",
          StringPiece{subdirPath.data()});
    }
  }
}

void FsFileContentStore::ensureShardedTmpDirectories(int overlayDirFd) {
  auto result = ::mkdirat(overlayDirFd, kShardedTmpDirName.data(), 0700);
  if (result != 0 && errno != EEXIST) {
    folly::throwSystemError("failed to create overlay tmp directory");
  }
  auto tmpDirFd = openat(
      overlayDirFd,
      kShardedTmpDirName.data(),
      O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  folly::checkUnixError(tmpDirFd, "failed to open overlay tmp directory");
  File tmpDir{tmpDirFd, /* ownsFd */ true};
  ensureShardDirectories(tmpDir.fd(), 0700);
}

struct statfs FsFileContentStore::statFs() const {
  struct statfs fs = {};
  fstatfs(infoFile_.fd(), &fs);
  return fs;
}

void FsFileContentStore::close() {
  walFileCache_.wlock()->entries.clear();
  dirFile_.close();
  infoFile_.close();
}

void FsInodeCatalog::close(std::optional<InodeNumber> inodeNumber) {
  if (inodeNumber) {
    core_->saveNextInodeNumber(inodeNumber.value());
  }
  core_->close();
}

std::optional<InodeNumber> FsFileContentStore::tryLoadNextInodeNumber() {
  // If we ever want to extend this file, it should be renamed and a proper
  // header with version number added. In the meantime, we enforce the file is
  // 8 bytes.
  int fd = openat(dirFile_.fd(), kNextInodeNumberFile, O_RDONLY | O_CLOEXEC);
  if (fd == -1) {
    if (errno == ENOENT) {
      // No max inode number file was written which usually means either Eden
      // was not shut down cleanly or an old overlay is being loaded.
      // Either way, a full scan of the overlay is necessary, so leave
      // nextInodeNumber_ at 0.
      return std::nullopt;
    } else {
      folly::throwSystemError("Failed to open ", kNextInodeNumberFile);
    }
  }

  folly::File nextInodeNumberFile{fd, /* ownsFd */ true};

  // Immediately unlink - the presence of the file indicates a clean shutdown.
  if (unlinkat(dirFile_.fd(), kNextInodeNumberFile, 0)) {
    folly::throwSystemError(
        "Failed to unlink ", kNextInodeNumberFile, " in overlay");
  }

  uint64_t nextInodeNumber;
  auto readResult =
      folly::readFull(fd, &nextInodeNumber, sizeof(nextInodeNumber));
  if (readResult < 0) {
    folly::throwSystemError(
        "Failed to read ", kNextInodeNumberFile, " from overlay");
  }
  if (readResult != sizeof(nextInodeNumber)) {
    XLOGF(
        WARN,
        "Failed to read entire inode number. Only read {} bytes. Full overlay scan required.",
        readResult);
    return std::nullopt;
  }

  if (nextInodeNumber <= kRootNodeId.get()) {
    XLOGF(
        WARN,
        "Invalid max inode number {}. Full overlay scan required.",
        nextInodeNumber);
    return std::nullopt;
  }
  return InodeNumber{nextInodeNumber};
}

void FsFileContentStore::saveNextInodeNumber(InodeNumber nextInodeNumber) {
  auto nextInodeNumberPath =
      localDir_ + PathComponentPiece{kNextInodeNumberFile};

  auto nextInodeVal = nextInodeNumber.get();
  auto result = writeFileAtomic(
      nextInodeNumberPath,
      ByteRange(
          reinterpret_cast<const uint8_t*>(&nextInodeVal),
          reinterpret_cast<const uint8_t*>(&nextInodeVal + 1)));
  if (result.hasException()) {
    XLOGF(
        WARN,
        "Failed to save next inode number to {}: {}",
        nextInodeNumberPath,
        folly::exceptionStr(result.exception()));

    bool isENOENT = false;
    result.exception().with_exception([&](const std::system_error& ex) {
      isENOENT = ex.code() == std::errc::no_such_file_or_directory;
    });
    if (!isENOENT) {
      // Remove the file so tryLoadNextInodeNumber() returns nullopt on next
      // startup, triggering a full overlay scan rather than using stale data.
      if (unlinkat(dirFile_.fd(), kNextInodeNumberFile, 0) != 0) {
        XLOGF(
            WARN,
            "Failed to remove {}: {}",
            nextInodeNumberPath,
            folly::errnoStr(errno));
      }
    }
    // The file or the directory is removed, next startup will trigger a full
    // overlay scan.
  }
}

void FsFileContentStore::validateExistingOverlay(int infoFD) {
  // Read the info file header
  std::array<uint8_t, kInfoHeaderSize> infoHeader;
  auto sizeRead = folly::readFull(infoFD, infoHeader.data(), infoHeader.size());
  folly::checkUnixError(
      sizeRead, "error reading from overlay info file in ", localDir_.view());
  if (sizeRead != infoHeader.size()) {
    throw_<std::runtime_error>(
        "truncated info file in overlay directory ", localDir_);
  }
  // Verify the magic value is correct
  if (memcmp(
          infoHeader.data(),
          kInfoHeaderMagic.data(),
          kInfoHeaderMagic.size()) != 0) {
    throw_<std::runtime_error>("bad data in overlay info file for ", localDir_);
  }
  // Extract the version number
  uint32_t version;
  memcpy(
      &version, infoHeader.data() + kInfoHeaderMagic.size(), sizeof(version));
  version = folly::Endian::big(version);

  // Make sure we understand this version number
  if (version != kOverlayVersion) {
    throw_<std::runtime_error>(
        "Unsupported eden overlay format ", version, " in ", localDir_);
  }
}

void FsFileContentStore::initNewOverlay() {
  // Make sure the overlay directory itself exists.  It's fine if it already
  // exists (although presumably it should be empty).
  auto result = ::mkdir(localDir_.value().c_str(), 0755);
  if (result != 0 && errno != EEXIST) {
    folly::throwSystemError(
        "error creating eden overlay directory ", localDir_.view());
  }
  auto localDirFile = File(localDir_.view(), O_RDONLY);

  // We split the inode files across 256 subdirectories.
  // Populate these subdirectories now.
  ensureShardDirectories(localDirFile.fd(), 0755);

  // Create the "tmp/" directory for backward compatibility with older Eden
  // versions that use it for temporary files during overlay writes.
  folly::checkUnixError(
      ::mkdirat(localDirFile.fd(), "tmp", 0700),
      "failed to create overlay tmp directory");

  // Create sharded tmp directories (sharded_tmp/00 through sharded_tmp/ff) to
  // avoid lock_rename contention during concurrent overlay writes.
  ensureShardedTmpDirectories(localDirFile.fd());

  // For now we just write a simple header, with a magic number to identify
  // this as an eden overlay file, and the version number of the overlay
  // format.
  std::array<uint8_t, kInfoHeaderSize> infoHeader;
  memcpy(infoHeader.data(), kInfoHeaderMagic.data(), kInfoHeaderMagic.size());
  auto version = folly::Endian::big(kOverlayVersion);
  memcpy(
      infoHeader.data() + kInfoHeaderMagic.size(), &version, sizeof(version));

  auto infoPath = localDir_ + PathComponentPiece{kInfoFile};
  writeFileAtomic(infoPath, ByteRange(infoHeader.data(), infoHeader.size()))
      .value();
}

optional<overlay::OverlayDir> FsInodeCatalog::loadOverlayDir(
    InodeNumber inodeNumber) {
  return core_->deserializeOverlayDir(inodeNumber);
}

std::optional<overlay::OverlayDir> FsInodeCatalog::loadAndRemoveOverlayDir(
    InodeNumber inodeNumber) {
  auto result = loadOverlayDir(inodeNumber);
  removeOverlayDir(inodeNumber);
  return result;
}

void FsInodeCatalog::saveOverlayDir(
    InodeNumber inodeNumber,
    overlay::OverlayDir&& odir,
    bool crashSafe) {
  // Ask thrift to serialize it.
  auto serializedData = CompactSerializer::serialize<std::string>(odir);

  // Add header to the overlay directory.
  auto header = FsFileContentStore::createHeader(
      FsFileContentStore::kHeaderIdentifierDir,
      FsFileContentStore::kHeaderVersion);

  std::array<struct iovec, 2> iov;
  iov[0].iov_base = header.data();
  iov[0].iov_len = header.size();
  iov[1].iov_base = const_cast<char*>(serializedData.data());
  iov[1].iov_len = serializedData.size();
  (void)core_->createOverlayFileImpl(
      inodeNumber,
      iov.data(),
      iov.size(),
      crashSafe,
      /*removeOnFailure=*/false);
}

void FsInodeCatalog::saveOverlayEntries(
    InodeNumber inodeNumber,
    size_t count,
    OverlayEntrySource source,
    bool crashSafe) {
  using apache::thrift::protocol::TType;

  folly::IOBufQueue queue(folly::IOBufQueue::cacheChainLength());
  apache::thrift::CompactProtocolWriter writer;
  writer.setOutput(&queue);

  writer.writeStructBegin("OverlayDir");
  writer.writeFieldBegin("entries", TType::T_MAP, 1);
  writer.writeMapBegin(
      TType::T_STRING, TType::T_STRUCT, static_cast<uint32_t>(count));

  source([&](const std::string& name, const overlay::OverlayEntry& entry) {
    writer.writeString(name);
    entry.write(&writer);
  });

  writer.writeMapEnd();
  writer.writeFieldEnd();
  writer.writeFieldStop();
  writer.writeStructEnd();

  auto serializedBuf = queue.move();

  auto header = FsFileContentStore::createHeader(
      FsFileContentStore::kHeaderIdentifierDir,
      FsFileContentStore::kHeaderVersion);

  folly::fbvector<struct iovec> iov;
  iov.resize(1);
  iov[0].iov_base = header.data();
  iov[0].iov_len = header.size();
  serializedBuf->appendToIov(&iov);
  (void)core_->createOverlayFileImpl(
      inodeNumber,
      iov.data(),
      iov.size(),
      crashSafe,
      /*removeOnFailure=*/false);
}

namespace {

void loadOverlayEntriesFromData(
    InodeNumber inodeNumber,
    folly::StringPiece rawData,
    InodeCatalog::OverlayEntryLoader loader) {
  using apache::thrift::protocol::TType;

  auto buf = folly::IOBuf::wrapBuffer(rawData.data(), rawData.size());
  apache::thrift::CompactProtocolReader reader;
  reader.setInput(buf.get());

  std::string structName;
  auto parse = [&](std::string_view stage, auto&& fn) {
    try {
      fn();
    } catch (const std::bad_alloc&) {
      // An allocation failure is transient, not evidence of on-disk
      // corruption. Let it propagate so online fsck does not persistently
      // replace valid data with a partially parsed prefix.
      throw;
    } catch (const OverlayDataCorruption&) {
      throw;
    } catch (const std::exception& ex) {
      throw OverlayDataCorruption{
          EIO,
          EdenErrorType::POSIX_ERROR,
          "corrupt overlay for inode ",
          inodeNumber,
          " at directory data offset ",
          rawData.size() - reader.getCursor().totalLength(),
          " while reading ",
          stage,
          ": ",
          ex.what()};
    }
  };

  parse("directory struct", [&] { reader.readStructBegin(structName); });

  std::string fieldName;
  TType fieldType;
  int16_t fieldId;
  parse("entries field", [&] {
    reader.readFieldBegin(fieldName, fieldType, fieldId);
  });

  if (fieldType != TType::T_MAP || fieldId != 1) {
    throw OverlayDataCorruption{
        EIO,
        EdenErrorType::POSIX_ERROR,
        "corrupt overlay for inode ",
        inodeNumber,
        " at directory data offset ",
        rawData.size() - reader.getCursor().totalLength(),
        ": expected map field with id 1, got type=",
        static_cast<int>(fieldType),
        " id=",
        fieldId};
  }

  TType keyType;
  TType valueType;
  uint32_t mapSize;
  parse(
      "entries map", [&] { reader.readMapBegin(keyType, valueType, mapSize); });

  // Compact protocol omits key/value types for empty maps, so only validate
  // types when there are entries.
  if (mapSize > 0) {
    if (keyType != TType::T_STRING || valueType != TType::T_STRUCT) {
      throw OverlayDataCorruption{
          EIO,
          EdenErrorType::POSIX_ERROR,
          "corrupt overlay for inode ",
          inodeNumber,
          " at directory data offset ",
          rawData.size() - reader.getCursor().totalLength(),
          ": expected map<string,struct>, got key=",
          static_cast<int>(keyType),
          " value=",
          static_cast<int>(valueType)};
    }
  }

  // Each map entry is at least a few bytes (varint string length + struct
  // stop field), so mapSize cannot plausibly exceed the raw data size.
  if (mapSize > rawData.size()) {
    throw OverlayDataCorruption{
        EIO,
        EdenErrorType::POSIX_ERROR,
        "corrupt overlay for inode ",
        inodeNumber,
        " at directory data offset ",
        rawData.size() - reader.getCursor().totalLength(),
        ": map size ",
        mapSize,
        " exceeds data size ",
        rawData.size()};
  }

  loader(mapSize, [&](InodeCatalog::OverlayEntryVisitor visitor) {
    std::string name;
    for (uint32_t i = 0; i < mapSize; ++i) {
      overlay::OverlayEntry entry;
      parse("entry name", [&] { reader.readString(name); });
      parse("entry fields", [&] { entry.read(&reader); });

      visitor(name, entry);
    }

    parse("directory end", [&] {
      reader.readMapEnd();
      reader.readFieldEnd();
      reader.readFieldBegin(fieldName, fieldType, fieldId);
    });
    if (fieldType != TType::T_STOP) {
      throw OverlayDataCorruption{
          EIO,
          EdenErrorType::POSIX_ERROR,
          "unexpected overlay field for inode ",
          inodeNumber,
          " at directory data offset ",
          rawData.size() - reader.getCursor().totalLength(),
          ": expected STOP, got type=",
          static_cast<int>(fieldType),
          " id=",
          fieldId};
    }
    parse("directory struct end", [&] { reader.readStructEnd(); });
    if (!reader.getCursor().isAtEnd()) {
      throw OverlayDataCorruption{
          EIO,
          EdenErrorType::POSIX_ERROR,
          "trailing overlay data for inode ",
          inodeNumber,
          " at directory data offset ",
          rawData.size() - reader.getCursor().totalLength(),
          ": ",
          reader.getCursor().totalLength(),
          " bytes remain"};
    }
  });
}

template <typename... Args>
fsck::InodeInfo makeInodeError(InodeNumber number, Args&&... args) {
  return fsck::InodeInfo(
      number,
      fsck::InodeType::Error,
      fmt::to_string(
          fmt::join(std::make_tuple(std::forward<Args>(args)...), "")));
}

fsck::InodeInfo readInodeInfoFromFile(InodeNumber number, folly::File& file) {
  std::array<char, FsFileContentStore::kHeaderLength> headerContents{};
  auto readResult =
      folly::readFull(file.fd(), headerContents.data(), headerContents.size());
  if (readResult < 0) {
    return makeInodeError(
        number, "error reading from file: ", folly::errnoStr(errno));
  } else if (readResult != FsFileContentStore::kHeaderLength) {
    return makeInodeError(
        number,
        fmt::format(
            "file was too short to contain overlay header: read {} bytes, expected {} bytes",
            readResult,
            FsFileContentStore::kHeaderLength));
  }

  static_assert(
      FsFileContentStore::kHeaderIdentifierDir.size() ==
          FsFileContentStore::kHeaderIdentifierFile.size(),
      "both header IDs must have the same length");
  StringPiece typeID(
      headerContents.data(),
      headerContents.data() + FsFileContentStore::kHeaderIdentifierDir.size());

  uint32_t versionBE;
  memcpy(
      &versionBE,
      headerContents.data() + FsFileContentStore::kHeaderIdentifierDir.size(),
      sizeof(uint32_t));
  auto version = folly::Endian::big(versionBE);
  if (version != FsFileContentStore::kHeaderVersion) {
    return makeInodeError(
        number, "unknown overlay file format version ", version);
  }

  if (typeID == FsFileContentStore::kHeaderIdentifierDir) {
    return fsck::InodeInfo(number, fsck::InodeType::Dir);
  } else if (typeID == FsFileContentStore::kHeaderIdentifierFile) {
    return fsck::InodeInfo(number, fsck::InodeType::File);
  }
  return makeInodeError(
      number,
      "unknown overlay file type ID: ",
      folly::hexlify(ByteRange{typeID}));
}

folly::File openFileNoVerifyWithFlags(
    int dirFd,
    AbsolutePathPiece localDir,
    InodeNumber inodeNumber,
    const InodePath& path,
    int flags) {
  flags |= O_CLOEXEC | O_NOFOLLOW;
#ifdef O_NOATIME
  // Reading an overlay file can update atime and force a later fdatasync() to
  // persist otherwise-clean metadata. Overlay files normally belong to the
  // daemon user; retry without O_NOATIME if an overlay has different ownership.
  int fd = openat(dirFd, path.c_str(), flags | O_NOATIME);
  if (fd < 0 && errno == EPERM) {
    XLOGF(
        DBG3,
        "O_NOATIME open returned EPERM for overlay inode {} in {}; retrying without O_NOATIME",
        inodeNumber,
        localDir.view());
    fd = openat(dirFd, path.c_str(), flags);
  }
#else
  int fd = openat(dirFd, path.c_str(), flags);
#endif
  folly::checkUnixError(
      fd,
      fmt::format(
          "error opening overlay file for inode {} in {}",
          inodeNumber,
          localDir.view()));
  return folly::File{fd, /* ownsFd */ true};
}

} // namespace

bool FsInodeCatalog::loadOverlayEntries(
    InodeNumber inodeNumber,
    OverlayEntryLoader loader) {
  auto rawData = core_->loadRawOverlayDir(inodeNumber);
  if (!rawData) {
    return false;
  }

  loadOverlayEntriesFromData(inodeNumber, *rawData, loader);
  return true;
}

InodePath FsFileContentStore::getFilePath(InodeNumber inodeNumber) {
  InodePath outPath;
  auto& outPathArray = outPath.rawData();
  formatSubdirPath(
      inodeNumber,
      MutableStringPiece{outPathArray.data(), kShardDirPathLength});
  outPathArray[kShardDirPathLength] = '/';
  auto numberPathStart = kShardDirPathLength + 1;
  auto index = folly::to_ascii_decimal(
      outPathArray.data() + numberPathStart,
      outPathArray.end(),
      inodeNumber.get());
  XDCHECK_LT(index + numberPathStart, outPathArray.size());
  outPathArray[index + numberPathStart] = '\0';
  return outPath;
}

AbsolutePath FsFileContentStore::getAbsoluteFilePath(
    InodeNumber inodeNumber) const {
  auto inodePath = getFilePath(inodeNumber);
  return localDir_ + RelativePathPiece(inodePath.c_str());
}

WalPath FsFileContentStore::getWalPath(InodeNumber inodeNumber) {
  // The .wal file lives in the overlay alongside its corresponding
  // directory listing file: "XX/<inode>" holds the serialized OverlayDir,
  // "XX/<inode>.wal" holds the pending write-ahead-log entries for that
  // same directory.
  auto inodePath = getFilePath(inodeNumber);
  WalPath walPath;
  auto& walData = walPath.rawData();
  auto inodeStr = inodePath.c_str();
  auto len = strlen(inodeStr);
  XCHECK_LT(len + 4, walData.size()) << "WAL path exceeds maximum length";
  memcpy(walData.data(), inodeStr, len);
  memcpy(walData.data() + len, ".wal", 4);
  walData[len + 4] = '\0';
  return walPath;
}

namespace {
constexpr size_t kWalVersionOffset =
    FsFileContentStore::kWalHeaderIdentifier.size();
constexpr size_t kWalVersionedPrefixLength =
    kWalVersionOffset + sizeof(uint32_t);

// The v2 header is only the shared identifier and version prefix.
static_assert(
    FsFileContentStore::kWalVersion2HeaderLength == kWalVersionedPrefixLength);

/** Bytes before the first record in a file of `format`; v1 is headerless. */
size_t walHeaderLength(WalFormat format) {
  switch (format) {
    case WalFormat::Version1:
      return 0;
    case WalFormat::Version2:
      return FsFileContentStore::kWalVersion2HeaderLength;
  }
  throw std::runtime_error(
      fmt::format(
          "cannot size WAL header for format {}", static_cast<int>(format)));
}

// A v1 frame needs at least an entryLen and an op byte. Anything shorter
// cannot be a record in any format.
constexpr size_t kWalMinFrameSize = sizeof(uint32_t) + sizeof(uint8_t);

/** Map an OVWL header version to its format; throws for unknown versions. */
WalFormat walFormatForVersion(uint32_t version, InodeNumber parent) {
  // Add a case here when introducing a new on-disk WAL version.
  switch (version) {
    case FsFileContentStore::kWalVersion2:
      return WalFormat::Version2;
    default:
      throw std::runtime_error(
          fmt::format(
              "unsupported WAL format version {} for inode {}",
              version,
              parent));
  }
}

struct WalHeader {
  WalFormat format{WalFormat::Version1};
  size_t recordsOffset{0};
  // The file is too short to hold any record, or ends inside its versioned
  // header. Either way it holds nothing recoverable; callers treat it as a
  // torn write to heal rather than an error to surface.
  bool torn{false};
};

/**
 * Classify a WAL from its leading bytes. `prefix` is the first
 * min(fileSize, kWalVersionedPrefixLength) bytes of a file of `fileSize`
 * bytes; the reader passes the whole file. Headerless files are Version1.
 * Throws for an unknown version, so a newer file is never misread as
 * Version1 records.
 */
WalHeader
readWalHeader(folly::StringPiece prefix, size_t fileSize, InodeNumber parent) {
  XDCHECK_GE(
      prefix.size(), std::min(fileSize, size_t{kWalVersionedPrefixLength}));
  if (fileSize < kWalMinFrameSize) {
    return {.torn = true};
  }
  if (!prefix.starts_with(FsFileContentStore::kWalHeaderIdentifier)) {
    return {};
  }
  if (fileSize < kWalVersionedPrefixLength) {
    return {.torn = true};
  }
  uint32_t versionBE;
  memcpy(&versionBE, prefix.data() + kWalVersionOffset, sizeof(versionBE));
  const auto format =
      walFormatForVersion(folly::Endian::big(versionBE), parent);
  const auto headerLength = walHeaderLength(format);
  if (fileSize < headerLength) {
    return {.torn = true};
  }
  return {.format = format, .recordsOffset = headerLength};
}

/**
 * Write the header for a new WAL of `format` into `dest`, which must hold
 * walHeaderLength(format) bytes. Returns the number of bytes written.
 */
size_t writeWalHeader(uint8_t* dest, WalFormat format) {
  switch (format) {
    case WalFormat::Version1:
      return 0;
    case WalFormat::Version2: {
      memcpy(
          dest,
          FsFileContentStore::kWalHeaderIdentifier.data(),
          FsFileContentStore::kWalHeaderIdentifier.size());
      const auto versionBE =
          folly::Endian::big(FsFileContentStore::kWalVersion2);
      memcpy(dest + kWalVersionOffset, &versionBE, sizeof(versionBE));
      return FsFileContentStore::kWalVersion2HeaderLength;
    }
  }
  throw std::runtime_error(
      fmt::format(
          "cannot write WAL header for format {}", static_cast<int>(format)));
}
} // namespace

FsFileContentStore::CachedWalFilePtr FsFileContentStore::getCachedWalFile(
    InodeNumber parent) {
  if (cacheWalFiles_) {
    auto cache = walFileCache_.wlock();
    auto iter = cache->entries.find(parent);
    if (iter != cache->entries.end()) {
      return iter->second;
    }
  }

  auto walPath = getWalPath(parent);
  int fd = openat(
      dirFile_.fd(),
      walPath.c_str(),
      O_APPEND | O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW,
      0600);
  folly::checkUnixError(
      fd, fmt::format("error opening WAL file for inode {}", parent));
  folly::File file{fd, /* ownsFd */ true};

  off_t size = ::lseek(fd, 0, SEEK_END);
  folly::checkUnixError(
      size, fmt::format("error stat'ing WAL file for inode {}", parent));
  // A new or empty file takes the configured format; an existing file keeps
  // whatever its header says, so a config change never mixes formats within
  // one file.
  auto format = walFormat_;
  if (size != 0) {
    // Classify from the file size plus the leading bytes. A short read of
    // a regular file at a known size is an I/O failure, not a short file.
    std::array<char, kWalVersionedPrefixLength> prefix{};
    const auto want = std::min(static_cast<size_t>(size), prefix.size());
    const auto count = folly::preadFull(fd, prefix.data(), want, 0);
    folly::checkUnixError(
        count, fmt::format("error reading WAL header for inode {}", parent));
    if (static_cast<size_t>(count) != want) {
      folly::throwSystemErrorExplicit(
          EIO,
          fmt::format(
              "short read of WAL header for inode {} ({} of {} bytes)",
              parent,
              count,
              want));
    }
    const auto header = readWalHeader(
        folly::StringPiece{prefix.data(), want},
        static_cast<size_t>(size),
        parent);
    if (header.torn) {
      // Nothing recoverable is in the file. Start it over rather than
      // wedging every append behind bytes that can never be parsed, or
      // appending v1 records after a partial versioned header.
      XLOGF(
          WARN,
          "WAL for inode {} is {} bytes and holds no record; truncating",
          parent,
          size);
      folly::checkUnixError(
          ::ftruncate(fd, 0),
          fmt::format("error truncating torn WAL for inode {}", parent));
      size = 0;
    } else {
      format = header.format;
    }
  }
  auto cached = std::make_shared<CachedWalFile>(
      std::move(file), static_cast<uint64_t>(size), format);

  if (!cacheWalFiles_) {
    // The caller drops the last reference once the append completes, which
    // closes the fd, matching the uncached behavior.
    return cached;
  }

  auto cache = walFileCache_.wlock();
  auto iter = cache->entries.find(parent);
  if (iter != cache->entries.end()) {
    return iter->second;
  }
  cache->entries.set(parent, cached);
  return cached;
}

void FsFileContentStore::invalidateCachedWalFile(InodeNumber parent) {
  walFileCache_.wlock()->entries.erase(parent);
}

namespace {
/**
 * Integer fields of a WAL record. Version1 stores each at a fixed width in
 * native little-endian byte order. Version2 stores every one as an unsigned
 * LEB128 varint, so a v2 record has no byte-order dependence.
 */
enum class WalField { EntryLen, NameLen, Mode, InodeNumber, ObjectIdLen };

/** Largest value the reader accepts for a field in any format. */
uint64_t walFieldMax(WalField field) {
  return field == WalField::InodeNumber ? std::numeric_limits<uint64_t>::max()
                                        : std::numeric_limits<uint32_t>::max();
}

/** Encoded size of `value` in `field`; v1 widths do not depend on the value. */
size_t walFieldSize(uint64_t value, WalFormat format, WalField field) {
  switch (format) {
    case WalFormat::Version1:
      switch (field) {
        case WalField::EntryLen:
          return sizeof(uint32_t);
        case WalField::NameLen:
          return sizeof(uint16_t);
        case WalField::Mode:
          return sizeof(int32_t);
        case WalField::InodeNumber:
          return sizeof(int64_t);
        case WalField::ObjectIdLen:
          return sizeof(uint8_t);
      }
      throw std::runtime_error(
          fmt::format("unknown WAL field {}", static_cast<int>(field)));
    case WalFormat::Version2:
      return static_cast<size_t>(folly::encodeVarintSize(value));
  }
  throw std::runtime_error(
      fmt::format(
          "cannot size WAL field for format {}", static_cast<int>(format)));
}

/**
 * Encode `value` at the front of `dest` and advance `dest` past it. Checks
 * that the field fits before writing. Version1 callers must have checked
 * that the value fits the field's fixed width.
 */
void writeWalField(
    folly::MutableByteRange& dest,
    uint64_t value,
    WalFormat format,
    WalField field) {
  const auto size = walFieldSize(value, format, field);
  if (size > dest.size()) {
    EDEN_BUG() << "WAL record buffer too small for field "
               << static_cast<int>(field) << ": need " << size << ", have "
               << dest.size();
  }
  switch (format) {
    case WalFormat::Version1:
      // Little-endian host: the low bytes of `value` are the fixed-width
      // encoding.
      memcpy(dest.data(), &value, size);
      break;
    case WalFormat::Version2: {
      [[maybe_unused]] const auto written =
          folly::encodeVarint(value, dest.data());
      XDCHECK_EQ(written, size);
      break;
    }
  }
  dest.advance(size);
}
} // namespace

uint64_t FsFileContentStore::appendWalEntry(
    InodeNumber parent,
    WalOpType op,
    PathComponentPiece childName,
    const overlay::OverlayEntry* entry) {
  // Framed WAL records:
  //   [entryLen]
  //   [uint8_t op]
  //   [nameLen]
  //   [name bytes]
  // For ADD, the payload additionally contains:
  //   [mode][inodeNumber][hashLen][hash bytes]
  //   [uint8_t isRestricted][uint8_t aclRootState]
  // Headerless v1 WALs store the bracketed integers at fixed widths
  // (uint32_t, uint16_t, int32_t, int64_t, uint8_t) in native byte order.
  // V2 WALs have an OVWL+version header and store every one of them as an
  // unsigned varint. entryLen covers everything after the entryLen field
  // itself, and is used during replay to detect torn writes.
  static_assert(
      std::endian::native == std::endian::little,
      "The v1 WAL wire format is native little-endian; a big-endian target "
      "needs explicit byte-swapping in writeWalField and readWalField.");

  // Precondition: entry must be non-null iff op == ADD. Catches caller
  // bugs where a stale entry pointer is passed for REMOVE/MATERIALIZE
  // (silently ignored otherwise) or a missing entry for ADD.
  switch (op) {
    case WalOpType::ADD:
      if (entry == nullptr) {
        EDEN_BUG() << "ADD WAL entry requires an OverlayEntry";
      }
      break;
    case WalOpType::REMOVE:
    case WalOpType::MATERIALIZE:
      if (entry != nullptr) {
        EDEN_BUG() << "REMOVE/MATERIALIZE WAL entry must not carry an "
                      "OverlayEntry";
      }
      break;
  }

  const auto nameStr = childName.view();
  const size_t hashSize = op == WalOpType::ADD && entry->hash().has_value()
      ? entry->hash()->size()
      : 0;

  auto walFile = getCachedWalFile(parent);
  const auto format = walFile->format;

  // Sizes come from the filesystem and the backing store, so an entry that
  // does not fit is an input condition, not a bug: throw and let the Overlay
  // persist the directory with a full write instead. Checked against the
  // file's actual format, so a brand-new WAL may be left empty on rejection;
  // the Overlay's full write removes it.
  size_t maxName = std::numeric_limits<uint32_t>::max();
  size_t maxHash = std::numeric_limits<uint32_t>::max();
  if (format == WalFormat::Version1) {
    maxName = std::numeric_limits<uint16_t>::max();
    maxHash = std::numeric_limits<uint8_t>::max();
  }
  if (nameStr.size() > maxName) {
    throw WalEntryTooLargeError(
        fmt::format(
            "name of {} bytes does not fit the WAL for inode {}",
            nameStr.size(),
            parent));
  }
  if (hashSize > maxHash) {
    throw WalEntryTooLargeError(
        fmt::format(
            "object ID of {} bytes does not fit the WAL for inode {}",
            hashSize,
            parent));
  }

  const auto nameLen = static_cast<uint32_t>(nameStr.size());
  const auto hashLen = static_cast<uint32_t>(hashSize);
  // Written as unsigned so negative values round-trip bit-for-bit; the
  // reader casts back to the Thrift field types.
  uint32_t mode = 0;
  uint64_t inodeNum = 0;
  if (op == WalOpType::ADD) {
    mode = static_cast<uint32_t>(*entry->mode());
    inodeNum = static_cast<uint64_t>(*entry->inodeNumber());
  }

  size_t payloadSize = sizeof(uint8_t) +
      walFieldSize(nameLen, format, WalField::NameLen) + nameLen;
  if (op == WalOpType::ADD) {
    payloadSize += walFieldSize(mode, format, WalField::Mode) +
        walFieldSize(inodeNum, format, WalField::InodeNumber) +
        walFieldSize(hashLen, format, WalField::ObjectIdLen) + hashLen +
        kWalAclRootStateTailSize;
  }
  if (payloadSize > std::numeric_limits<uint32_t>::max()) {
    throw WalEntryTooLargeError(
        fmt::format(
            "WAL record of {} bytes exceeds the frame limit for inode {}",
            payloadSize,
            parent));
  }

  const auto headerSize = walFile->size == 0 ? walHeaderLength(format) : 0;
  size_t totalSize = headerSize +
      walFieldSize(payloadSize, format, WalField::EntryLen) + payloadSize;
  // 1024-byte inline cap covers NAME_MAX (255) entries; larger records
  // fall back to heap storage.
  folly::small_vector<uint8_t, 1024> buf(totalSize);
  folly::MutableByteRange out{buf.data(), buf.size()};
  auto writeBytes = [&](const void* src, size_t size) {
    if (size > out.size()) {
      EDEN_BUG() << "WAL record buffer too small: need " << size << ", have "
                 << out.size();
    }
    memcpy(out.data(), src, size);
    out.advance(size);
  };
  auto writeByte = [&](uint8_t byte) { writeBytes(&byte, sizeof(byte)); };

  if (headerSize != 0) {
    if (headerSize > out.size()) {
      EDEN_BUG() << "WAL record buffer too small for its header";
    }
    out.advance(writeWalHeader(out.data(), format));
  }

  writeWalField(out, payloadSize, format, WalField::EntryLen);
  writeByte(static_cast<uint8_t>(op));
  writeWalField(out, nameLen, format, WalField::NameLen);
  writeBytes(nameStr.data(), nameLen);

  if (op == WalOpType::ADD) {
    writeWalField(out, mode, format, WalField::Mode);
    writeWalField(out, inodeNum, format, WalField::InodeNumber);
    writeWalField(out, hashLen, format, WalField::ObjectIdLen);
    if (hashLen > 0) {
      writeBytes(apache::thrift::can_throw(entry->hash())->data(), hashLen);
    }

    const auto aclRootState = getWalEntryAclRootState(*entry);
    writeByte(
        static_cast<uint8_t>(aclRootState == AclRootState::RestrictedAclRoot));
    writeByte(static_cast<uint8_t>(aclRootState));
  }

  if (!out.empty()) {
    EDEN_BUG() << "WAL record sized " << totalSize << " but " << out.size()
               << " bytes were left unwritten";
  }

  auto fd = walFile->file.fd();
  auto sizeBefore = walFile->size;
  // The tracked size must match the on-disk size exactly: the short-write
  // recovery below truncates to sizeBefore, so a stale-low value would chop
  // off valid entries. A mismatch means a writer bypassed appendWalEntry or
  // the per-parent serialization contract broke.
  XDCHECK_EQ(static_cast<off_t>(sizeBefore), ::lseek(fd, 0, SEEK_END));
  SCOPE_FAIL {
    invalidateCachedWalFile(parent);
  };

  // No fsync after the write. EdenFS does not promise power-loss
  // durability for overlay state — saveOverlayDir takes the same stance
  // (only the overlay root inode is fsync'd, see writeThriftStructToFile
  // below). Adding fsync to every WAL append would dominate the fast-
  // path cost and erase the latency win over a full directory rewrite.
  auto written = folly::writeFull(fd, buf.data(), totalSize);
  if (FOLLY_UNLIKELY(
          written < 0 || static_cast<size_t>(written) != totalSize)) {
    // Failed or short write (e.g. ENOSPC). Truncate the torn tail
    // so subsequent O_APPEND writes land on a valid prefix instead of
    // burying the tear mid-file. Capture errno before ftruncate clobbers it.
    int writeErrno = errno;
    int truncErrno = 0;
    if (::ftruncate(fd, static_cast<off_t>(sizeBefore)) != 0) {
      truncErrno = errno;
    }
    folly::throwSystemErrorExplicit(
        writeErrno != 0 ? writeErrno : EIO,
        fmt::format(
            "failed or short WAL write for inode {} ({} of {} bytes; "
            "ftruncate-recovery errno={})",
            parent,
            written,
            totalSize,
            truncErrno));
  }

  walFile->size = sizeBefore + static_cast<uint64_t>(written);
  return walFile->size;
}

bool FsFileContentStore::hasWal(InodeNumber parent) {
  auto walPath = getWalPath(parent);
  // fstatat + S_ISREG so a symlink planted in the shard dir does not
  // pose as a WAL (faccessat ignores AT_SYMLINK_NOFOLLOW on Linux glibc).
  // Mirror loadRawOverlayDir: ENOENT means absent, anything else throws —
  // returning false on EIO/EACCES would silently skip replay and let the
  // caller overwrite the on-disk WAL.
  struct stat st{};
  if (::fstatat(dirFile_.fd(), walPath.c_str(), &st, AT_SYMLINK_NOFOLLOW) ==
      0) {
    return S_ISREG(st.st_mode);
  }
  int err = errno;
  if (err == ENOENT) {
    return false;
  }
  folly::throwSystemErrorExplicit(
      err, fmt::format("error stat'ing WAL file for inode {}", parent));
}

namespace {
bool isValidPathComponent(const std::string& name) {
  try {
    PathComponentPiece{name};
    return true;
  } catch (const PathComponentValidationError&) {
    return false;
  }
}

/**
 * Read one integer field without consuming bytes outside its frame. Returns
 * nullopt when the frame ends early, a v2 varint is overlong or not the
 * shortest encoding of its value, or the value exceeds what the field may
 * hold.
 */
std::optional<uint64_t> readWalField(
    folly::ByteRange frame,
    size_t& offset,
    WalField field,
    WalFormat format) {
  switch (format) {
    case WalFormat::Version1: {
      const auto size = walFieldSize(/*value=*/0, format, field);
      if (offset > frame.size() || frame.size() - offset < size) {
        return std::nullopt;
      }
      uint64_t value = 0;
      memcpy(&value, frame.data() + offset, size);
      offset += size;
      return value;
    }
    case WalFormat::Version2: {
      if (offset > frame.size()) {
        return std::nullopt;
      }
      auto rest = frame.subpiece(offset);
      const auto value = folly::tryDecodeVarint(rest);
      if (!value || *value > walFieldMax(field)) {
        return std::nullopt;
      }
      // The writer emits the shortest encoding, so a padded one (trailing
      // 0x80 0x00 style continuation) is corruption, not a value.
      const auto consumed = frame.size() - offset - rest.size();
      if (consumed != static_cast<size_t>(folly::encodeVarintSize(*value))) {
        return std::nullopt;
      }
      offset = frame.size() - rest.size();
      return *value;
    }
  }
  throw std::runtime_error(
      fmt::format(
          "cannot read WAL field for format {}", static_cast<int>(format)));
}
} // namespace

LoadWalResult FsFileContentStore::loadWalDelta(
    InodeNumber parent,
    CaseSensitivity caseSensitive) {
  LoadWalResult result{caseSensitive};
  auto& delta = result.delta;
  auto assignDelta = [&](std::string walName, WalDelta walDelta) {
    auto it = delta.find(walName);
    if (it != delta.end()) {
      delta.erase(it);
    }
    delta.emplace(std::move(walName), std::move(walDelta));
  };
  // The name each inode was last ADDed under. An ADD of an inode that an
  // earlier ADD placed under another name is the destination of a rename, so
  // the earlier name becomes a REMOVE, which is the record the daemon writes
  // right after that ADD.
  folly::F14FastMap<int64_t, std::string> lastAddedAs;
  auto forgetAdd = [&](const std::string& name) {
    auto it = delta.find(name);
    if (it == delta.end() || it->second.type != WalOpType::ADD) {
      return;
    }
    auto known = lastAddedAs.find(*it->second.entry.inodeNumber());
    if (known != lastAddedAs.end() && known->second == it->first) {
      lastAddedAs.erase(known);
    }
  };

  auto walPath = getWalPath(parent);
  // Mirror loadRawOverlayDir: open once, ENOENT means no WAL, anything
  // else throws. The caller (Overlay::loadOverlayDir for the hot path,
  // OverlayChecker's repair path for fsck) decides how to handle the
  // error.
  int fd =
      openat(dirFile_.fd(), walPath.c_str(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
  if (fd == -1) {
    // ENOENT just means there is no pending WAL for this inode.
    if (errno == ENOENT) {
      return result;
    }
    folly::throwSystemError(
        fmt::format("error opening WAL file for inode {}", parent));
  }
  folly::File file{fd, /* ownsFd */ true};

  std::string data;
  if (!folly::readFile(file.fd(), data)) {
    folly::throwSystemError(
        fmt::format("error reading WAL file for inode {}", parent));
  }

  if (data.empty()) {
    return result;
  }

  const auto header = readWalHeader(data, data.size(), parent);
  if (header.torn) {
    ++result.parseErrors;
    return result;
  }
  const auto format = header.format;
  size_t offset = header.recordsOffset;

  while (offset < data.size()) {
    const folly::ByteRange rest{
        reinterpret_cast<const uint8_t*>(data.data()) + offset,
        data.size() - offset};
    size_t entryStart = 0;
    const auto entryLenField =
        readWalField(rest, entryStart, WalField::EntryLen, format);
    // A missing or torn entryLen is a torn tail, as is a frame that claims
    // more bytes than remain.
    if (!entryLenField || *entryLenField == 0 ||
        entryStart + *entryLenField > rest.size()) {
      ++result.parseErrors;
      break;
    }
    const auto entryLen = static_cast<size_t>(*entryLenField);
    const uint8_t* entryData = rest.data() + entryStart;
    const folly::ByteRange frame{entryData, entryLen};
    size_t entryOffset = 0;

    if (entryOffset + sizeof(uint8_t) > entryLen) {
      ++result.parseErrors;
      break;
    }
    auto opType = static_cast<WalOpType>(entryData[entryOffset]);
    entryOffset += sizeof(uint8_t);

    auto nameLen = readWalField(frame, entryOffset, WalField::NameLen, format);
    if (!nameLen) {
      ++result.parseErrors;
      break;
    }

    if (entryOffset + *nameLen > entryLen) {
      ++result.parseErrors;
      break;
    }
    std::string name(
        reinterpret_cast<const char*>(entryData + entryOffset), *nameLen);
    entryOffset += *nameLen;

    bool valid = true;
    bool skipped = false;
    if (!isValidPathComponent(name)) {
      // The frame is intact, so skip just this entry. Letting the name
      // reach the delta map would throw from its comparator and leave the
      // directory unloadable until the WAL is removed by hand.
      ++result.parseErrors;
      XLOGF(
          WARN,
          "Skipping WAL entry for inode {} whose name is not a valid path component",
          parent);
      skipped = true;
    } else {
      switch (opType) {
        case WalOpType::ADD: {
          const auto modeField =
              readWalField(frame, entryOffset, WalField::Mode, format);
          if (!modeField) {
            valid = false;
            break;
          }
          const auto mode =
              static_cast<int32_t>(static_cast<uint32_t>(*modeField));

          const auto inodeField =
              readWalField(frame, entryOffset, WalField::InodeNumber, format);
          if (!inodeField) {
            valid = false;
            break;
          }
          const auto inodeNum = static_cast<int64_t>(*inodeField);

          if (inodeNum <= 0) {
            ++result.parseErrors;
            XLOGF(
                WARN,
                "Skipping WAL ADD entry {} for inode {} with invalid inode number {}",
                name,
                parent,
                inodeNum);
            skipped = true;
            break;
          }

          auto hashLen =
              readWalField(frame, entryOffset, WalField::ObjectIdLen, format);
          if (!hashLen) {
            valid = false;
            break;
          }

          if (entryOffset + *hashLen > entryLen) {
            valid = false;
            break;
          }

          overlay::OverlayEntry overlayEntry;
          overlayEntry.mode() = mode;
          overlayEntry.inodeNumber() = inodeNum;
          if (*hashLen > 0) {
            overlayEntry.hash() = std::string(
                reinterpret_cast<const char*>(entryData + entryOffset),
                *hashLen);
          }
          entryOffset += *hashLen;

          auto remaining = entryLen - entryOffset;
          if (remaining > 0) {
            if (remaining != kWalAclRootStateTailSize) {
              valid = false;
              break;
            }
            auto isRestricted = entryData[entryOffset] != 0;
            entryOffset += sizeof(uint8_t);

            auto aclRootState = entryData[entryOffset];
            entryOffset += sizeof(uint8_t);

            overlayEntry.isRestricted() = isRestricted;
            overlayEntry.aclRootState() = static_cast<int32_t>(aclRootState);
          }

          forgetAdd(name);
          if (auto stale = lastAddedAs.find(inodeNum); stale !=
                  lastAddedAs.end() &&
              !isPathPieceEqual(PathComponentPiece{stale->second},
                                PathComponentPiece{name},
                                caseSensitive)) {
            assignDelta(stale->second, WalDelta{WalOpType::REMOVE, {}});
          }
          lastAddedAs[inodeNum] = name;
          assignDelta(
              std::move(name),
              WalDelta{WalOpType::ADD, std::move(overlayEntry)});
          break;
        }

        case WalOpType::REMOVE: {
          if (entryOffset != entryLen) {
            valid = false;
            break;
          }
          forgetAdd(name);
          assignDelta(std::move(name), WalDelta{WalOpType::REMOVE, {}});
          break;
        }

        case WalOpType::MATERIALIZE: {
          if (entryOffset != entryLen) {
            valid = false;
            break;
          }
          auto it = delta.find(name);
          if (it == delta.end()) {
            // No prior delta for this name — record the MATERIALIZE so the
            // mutator merge can clear the hash on the base entry.
            delta.emplace(
                std::move(name), WalDelta{WalOpType::MATERIALIZE, {}});
          } else if (it->second.type == WalOpType::ADD) {
            // Materialize an ADD we already have — clear the hash in place.
            it->second.entry.hash().reset();
          }
          // REMOVE stays REMOVE; mirrors replayWal's MATERIALIZE-on-missing
          // no-op (covered by loadWalDelta_materializeAfterRemoveLeavesRemove).
          break;
        }

        default:
          // Forward-compat: an unknown opcode with a valid entryLen frame is
          // safe to skip — the entryLen prefix told us exactly how many bytes
          // this entry occupies. Older binaries reading a newer WAL log and
          // continue rather than dropping every entry past the unknown op.
          ++result.parseErrors;
          XLOGF(
              WARN,
              "Unknown WAL op {} for inode {}; skipping entry",
              static_cast<int>(opType),
              parent);
          skipped = true;
          break;
      }
    }

    if (!valid) {
      ++result.parseErrors;
      break;
    }
    if (!skipped) {
      ++result.rawEntriesParsed;
    }
    offset += entryStart + entryLen;
  }

  // This also catches a torn length prefix (one to three trailing bytes).
  // Do not double-count corruption already reported while parsing a frame.
  if (offset != data.size() && result.parseErrors == 0) {
    ++result.parseErrors;
  }
  return result;
}

LoadWalResult FsFileContentStore::replayWal(
    InodeNumber parent,
    overlay::OverlayDir& dir,
    CaseSensitivity caseSensitive) {
  auto result = loadWalDelta(parent, caseSensitive);
  auto& entries = *dir.entries_ref();
  auto findEntry = [&](const std::string& name) {
    if (caseSensitive == CaseSensitivity::Sensitive) {
      return entries.find(name);
    }
    // TODO: Build a temporary case-aware index for large cold-path WAL
    // replays so insensitive lookups do not scan the directory per delta.
    return std::find_if(entries.begin(), entries.end(), [&](const auto& item) {
      return isPathPieceEqual(
          PathComponentPiece{item.first},
          PathComponentPiece{name},
          caseSensitive);
    });
  };
  // An ADD for an inode that the base holds under another name is the
  // destination of a rename whose REMOVE never reached the WAL; that name
  // goes. loadWalDelta already reconciled the ADDs within the WAL.
  folly::F14FastMap<int64_t, const std::string*> addedAs;
  for (const auto& [name, op] : result.delta) {
    if (op.type == WalOpType::ADD) {
      addedAs[*op.entry.inodeNumber()] = &name;
    }
  }
  if (!addedAs.empty()) {
    std::vector<std::string> stale;
    for (const auto& [name, entry] : entries) {
      auto added = addedAs.find(*entry.inodeNumber());
      if (added != addedAs.end() &&
          !isPathPieceEqual(
              PathComponentPiece{name},
              PathComponentPiece{*added->second},
              caseSensitive)) {
        XLOGF(
            WARN,
            "Overlay dir {}: dropping entry '{}' for inode {}, which the WAL names again as '{}'",
            parent,
            name,
            *entry.inodeNumber(),
            *added->second);
        stale.push_back(name);
      }
    }
    for (const auto& name : stale) {
      entries.erase(name);
    }
  }
  for (auto& [name, op] : result.delta) {
    switch (op.type) {
      case WalOpType::ADD: {
        auto it = findEntry(name);
        if (it != entries.end()) {
          entries.erase(it);
        }
        entries.emplace(name, std::move(op.entry));
        break;
      }
      case WalOpType::REMOVE: {
        auto it = findEntry(name);
        if (it != entries.end()) {
          entries.erase(it);
        }
        break;
      }
      case WalOpType::MATERIALIZE: {
        auto it = findEntry(name);
        if (it != entries.end()) {
          it->second.hash().reset();
        }
        break;
      }
    }
  }
  return result;
}

void FsFileContentStore::removeWal(InodeNumber parent) {
  invalidateCachedWalFile(parent);
  auto walPath = getWalPath(parent);
  if (::unlinkat(dirFile_.fd(), walPath.c_str(), 0) != 0) {
    int err = errno;
    if (err == ENOENT) {
      return;
    }
    folly::throwSystemErrorExplicit(
        err, fmt::format("error removing WAL file for inode {}", parent));
  }
}

std::vector<InodeNumber> FsFileContentStore::scanForWalFiles() const {
  // Suffix is constexpr so the off-by-one is in one place.
  static constexpr StringPiece kWalSuffix{".wal"};
  std::vector<InodeNumber> result;
  std::array<char, kShardDirPathLength + 1> subdirPath{};
  subdirPath[kShardDirPathLength] = '\0';
  MutableStringPiece subdirPiece{subdirPath.data(), kShardDirPathLength};

  for (ShardID n = 0; n < kNumShards; ++n) {
    formatSubdirShardPath(n, subdirPiece);

    // O_NOFOLLOW hardens against shard symlinks. This is still best-effort:
    // log shard access failures and keep scanning other shards.
    int shardFd = openat(
        dirFile_.fd(),
        subdirPath.data(),
        O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (shardFd == -1) {
      XLOGF(
          WARN,
          "scanForWalFiles: failed to open shard {}: {}",
          subdirPath.data(),
          folly::errnoStr(errno));
      continue;
    }

    DIR* dir = fdopendir(shardFd);
    if (!dir) {
      XLOGF(
          WARN,
          "scanForWalFiles: fdopendir failed on shard {}: {}",
          subdirPath.data(),
          folly::errnoStr(errno));
      ::close(shardFd);
      continue;
    }
    SCOPE_EXIT {
      closedir(dir);
    };

    // readdir reports end-of-stream and errors as nullptr. Check errno after
    // the loop so mid-shard failures are at least visible in logs.
    struct dirent* entry;
    while (true) {
      errno = 0;
      entry = readdir(dir);
      if (entry == nullptr) {
        break;
      }
      StringPiece name{entry->d_name};
      if (name == "." || name == "..") {
        continue;
      }
      if (!name.endsWith(kWalSuffix)) {
        continue;
      }
      // Accept DT_UNKNOWN (some network FSes / older kernels don't
      // populate d_type); reject other non-regular types.
      if (entry->d_type != DT_REG && entry->d_type != DT_UNKNOWN) {
        XLOGF(
            WARN,
            "scanForWalFiles: ignoring non-regular WAL entry {} in shard {}",
            entry->d_name,
            subdirPath.data());
        continue;
      }
      auto inodeStr = name.subpiece(0, name.size() - kWalSuffix.size());
      auto parsed = folly::tryTo<uint64_t>(inodeStr);
      if (parsed.hasError()) {
        XLOGF(
            WARN, "Ignoring WAL file with unparsable name: {}", entry->d_name);
        continue;
      }
      auto inodeNum = parsed.value();
      // InodeNumber{0} fails the InodeNumber invariant (debug abort), so
      // a stray "0.wal" would crash startup. Reject and warn.
      if (inodeNum == 0) {
        XLOGF(
            WARN,
            "Ignoring WAL file with zero inode: {} in shard {}",
            entry->d_name,
            subdirPath.data());
        continue;
      }
      // Reject leading-zero duplicates: "5.wal" and "05.wal" parse to
      // the same inode and we'd return (and replay) it twice.
      if (inodeStr.size() > 1 && inodeStr[0] == '0') {
        XLOGF(
            WARN,
            "Ignoring WAL file with leading-zero name: {}",
            entry->d_name);
        continue;
      }
      // Wrong-shard placement: getWalPath() addresses by (inode & 0xff),
      // so subsequent replayWal/removeWal would never find this file.
      if ((inodeNum & 0xff) != n) {
        XLOGF(
            WARN,
            "Ignoring WAL file in wrong shard: {} found in shard {} (expected shard {:02x})",
            entry->d_name,
            subdirPath.data(),
            inodeNum & 0xff);
        continue;
      }
      result.emplace_back(inodeNum);
    }
    if (errno != 0) {
      XLOGF(
          WARN,
          "scanForWalFiles: readdir failed in shard {}: {}",
          subdirPath.data(),
          folly::errnoStr(errno));
    }
  }
  return result;
}

std::optional<overlay::OverlayDir> FsFileContentStore::deserializeOverlayDir(
    InodeNumber inodeNumber) {
  auto raw = loadRawOverlayDir(inodeNumber);
  if (!raw) {
    return std::nullopt;
  }
  overlay::OverlayDir dir;
  loadOverlayEntriesFromData(
      inodeNumber,
      *raw,
      [&](uint32_t, InodeCatalog::OverlayEntryIterator iterate) {
        iterate([&](const std::string& name,
                    const overlay::OverlayEntry& entry) {
          auto [it, inserted] = dir.entries()->emplace(name, entry);
          if (!inserted) {
            throw OverlayDataCorruption{fmt::format(
                "duplicate overlay name (hex \"{}\") for inode {}: first inode {}, duplicate inode {}",
                folly::hexlify(name),
                inodeNumber,
                *it->second.inodeNumber(),
                *entry.inodeNumber())};
          }
        });
      });
  return dir;
}

std::optional<std::string> FsFileContentStore::loadRawOverlayDir(
    InodeNumber inodeNumber) {
  // Open the file.  Return std::nullopt if the file does not exist.
  auto path = FsFileContentStore::getFilePath(inodeNumber);
  int fd = openat(dirFile_.fd(), path.c_str(), O_RDWR | O_CLOEXEC | O_NOFOLLOW);
  if (fd == -1) {
    int err = errno;
    if (err == ENOENT) {
      // There is no overlay here
      return std::nullopt;
    }
    folly::throwSystemErrorExplicit(
        err,
        fmt::format(
            "error opening overlay file for inode {} in {}",
            inodeNumber,
            localDir_.view()));
  }
  folly::File file{fd, /* ownsFd */ true};

  // Read the file data
  std::string serializedData;
  if (!folly::readFile(file.fd(), serializedData)) {
    int err = errno;
    if (err == ENOENT) {
      // There is no overlay here
      return std::nullopt;
    }
    folly::throwSystemErrorExplicit(
        errno, "failed to read ", RelativePathPiece{path}.view());
  }

  StringPiece contents{serializedData};
  FsFileContentStore::validateHeader(
      inodeNumber, contents, FsFileContentStore::kHeaderIdentifierDir);

  // Return just the bytes after the header
  serializedData.erase(0, FsFileContentStore::kHeaderLength);
  return serializedData;
}

std::array<uint8_t, FsFileContentStore::kHeaderLength>
FsFileContentStore::createHeader(
    folly::StringPiece identifier,
    uint32_t version) {
  std::array<uint8_t, kHeaderLength> headerStorage;
  IOBuf header{IOBuf::WRAP_BUFFER, folly::MutableByteRange{headerStorage}};
  header.clear();
  folly::io::Appender appender(&header, 0);

  appender.push(identifier);
  appender.writeBE<uint32_t>(version);
  // The overlay header used to store timestamps for inodes but that has since
  // been moved to the InodeMetadataTable. Write zeroes instead.
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // atime.tv_sec
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // atime.tv_nsec
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // ctime.tv_sec
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // ctime.tv_nsec
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // mtime.tv_sec
  appender.writeBE<uint64_t>(static_cast<uint64_t>(0)); // mtime.tv_nsec
  auto paddingSize = kHeaderLength - header.length();
  appender.ensure(paddingSize);
  memset(appender.writableData(), 0, paddingSize);
  appender.append(paddingSize);

  return headerStorage;
}

folly::File FsFileContentStore::openFile(
    InodeNumber inodeNumber,
    folly::StringPiece headerId) {
  // Open the overlay file
  auto file = openFileNoVerify(inodeNumber);

  // Read the contents
  std::string contents;
  if (!folly::readFile(file.fd(), contents, kHeaderLength)) {
    folly::throwSystemErrorExplicit(
        errno,
        fmt::format(
            "failed to read overlay file for inode {} in {}",
            inodeNumber,
            localDir_.view()));
  }

  validateHeader(inodeNumber, contents, headerId);
  return file;
}

folly::File FsFileContentStore::openFileNoVerify(InodeNumber inodeNumber) {
  auto path = getFilePath(inodeNumber);
  return openFileNoVerifyWithFlags(
      dirFile_.fd(), localDir_, inodeNumber, path, O_RDWR);
}

namespace {

// Path format: sharded_tmp/<2-char-shard>/<inodeNumber>\0
// = kShardedTmpDirName.size() + 1 + kShardDirPathLength + 1 +
// kMaxDecimalInodeNumberLength + 1
using InodeTmpPath = std::array<
    char,
    kShardedTmpDirName.size() + 1 + FsFileContentStore::kShardDirPathLength +
        1 + FsFileContentStore::kMaxDecimalInodeNumberLength + 1>;

InodeTmpPath getFileTmpPath(InodeNumber inodeNumber) {
  // Create the temporary file in a sharded subdirectory under sharded_tmp/.
  // Using per-shard tmp directories (sharded_tmp/XX/) avoids cross-directory
  // rename contention on the kernel's lock_rename mutex that occurs when all
  // threads share a single tmp/ directory, while keeping the temp file in a
  // separate directory from the final destination for clean separation.
  InodeTmpPath tmpPath;
  memcpy(tmpPath.data(), kShardedTmpDirName.data(), kShardedTmpDirName.size());
  auto pos = kShardedTmpDirName.size();
  tmpPath[pos++] = '/';
  FsFileContentStore::formatSubdirPath(
      inodeNumber,
      MutableStringPiece{
          tmpPath.data() + pos, FsFileContentStore::kShardDirPathLength});
  pos += FsFileContentStore::kShardDirPathLength;
  tmpPath[pos++] = '/';
  auto index = folly::to_ascii_decimal(
      tmpPath.data() + pos, tmpPath.end(), inodeNumber.get());
  tmpPath[pos + index] = '\0';
  return tmpPath;
}

} // namespace

folly::File FsFileContentStore::createOverlayFileImpl(
    InodeNumber inodeNumber,
    iovec* iov,
    size_t iovCount,
    bool crashSafe,
    bool removeOnFailure) {
  auto path = getFilePath(inodeNumber);

  // For the root inode, always use the crash-safe path regardless of the
  // caller's request. If root inode data is corrupt, Eden cannot remount.
  bool useTmpFile = crashSafe || inodeNumber == kRootNodeId;

  const char* openPath;
  InodeTmpPath tmpPath{};
  if (useTmpFile) {
    tmpPath = getFileTmpPath(inodeNumber);
    openPath = tmpPath.data();
  } else {
    openPath = path.c_str();
  }

  auto fd = openat(
      dirFile_.fd(),
      openPath,
      O_CREAT | O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_TRUNC,
      0600);
  folly::checkUnixError(
      fd,
      fmt::format(
          "failed to create overlay file for inode {} in {}",
          inodeNumber,
          localDir_.view()));
  folly::File file{fd, /* ownsFd */ true};
  bool success = false;
  SCOPE_EXIT {
    if (!success && (useTmpFile || removeOnFailure) &&
        unlinkat(dirFile_.fd(), openPath, 0) != 0 && errno != ENOENT) {
      XLOGF(
          WARN,
          "failed to remove overlay file {} for inode {} after its creation failed: {}",
          openPath,
          inodeNumber,
          folly::errnoStr(errno));
    }
  };

  auto sizeWritten = folly::writevFull(fd, iov, iovCount);
  folly::checkUnixError(
      sizeWritten,
      fmt::format(
          "error writing to overlay file for inode {} in {}",
          inodeNumber,
          localDir_.view()));

  if (useTmpFile) {
    // We do not use mkstemp() to create the temporary file, since there is no
    // mkstempat() equivalent that can create files relative to dirFile.  We
    // simply create the file with a fixed suffix, and do not use O_EXCL.  This
    // is not a security risk since only the current user should have permission
    // to create files inside the overlay directory, so no one else can create
    // symlinks inside the overlay directory.  We also open the temporary file
    // using O_NOFOLLOW.
    //
    // We could potentially use O_TMPFILE followed by linkat() to commit the
    // file.  However this may not be supported by all filesystems, and seems to
    // provide minimal benefits for our use case.

    // fdatasync() is required to ensure that we are really reliably and
    // atomically writing out the new file.  Without calling fdatasync() the
    // file contents may not be flushed to disk even though the rename has
    // been written.
    //
    // However, fdatasync() has a significant performance overhead.  We've
    // measured it at a nearly 300 microsecond cost, which can significantly
    // impact performance of source control update operations when many inodes
    // are affected.
    //
    // Per docs/InodeStorage.md, Eden does not claim to handle disk, kernel,
    // or power failure, so we do not call fdatasync() in the common case.
    // However, the root inode is particularly important; if its data is
    // corrupt Eden will not be able to remount the checkout.  Therefore we
    // always call fdatasync() when writing out the root inode.
    if (inodeNumber == kRootNodeId) {
      auto syncReturnCode = folly::fdatasyncNoInt(fd);
      folly::checkUnixError(
          syncReturnCode,
          fmt::format(
              "error flushing data to overlay file for inode {} in {}",
              inodeNumber,
              localDir_.view()));
    }

    auto returnCode =
        renameat(dirFile_.fd(), tmpPath.data(), dirFile_.fd(), path.c_str());
    folly::checkUnixError(
        returnCode,
        fmt::format(
            "error committing overlay file for inode {} in {}",
            inodeNumber,
            localDir_.view()));
  }

  success = true;
  return file;
}

folly::File FsFileContentStore::createOverlayFile(
    InodeNumber inodeNumber,
    ByteRange contents) {
  auto header = createHeader(kHeaderIdentifierFile, kHeaderVersion);

  std::array<struct iovec, 2> iov;
  iov[0].iov_base = header.data();
  iov[0].iov_len = header.size();
  iov[1].iov_base = const_cast<uint8_t*>(contents.data());
  iov[1].iov_len = contents.size();
  return createOverlayFileImpl(
      inodeNumber,
      iov.data(),
      iov.size(),
      /*crashSafe=*/!directFileCreate_,
      /*removeOnFailure=*/true);
}

folly::File FsFileContentStore::createOverlayFile(
    InodeNumber inodeNumber,
    const IOBuf& contents) {
  // In the common case where there is just one element in the chain, use the
  // ByteRange version of createOverlayFile() to avoid having to allocate the
  // iovec array on the heap.
  if (contents.next() == &contents) {
    return createOverlayFile(
        inodeNumber, ByteRange{contents.data(), contents.length()});
  }

  auto header = createHeader(kHeaderIdentifierFile, kHeaderVersion);

  fbvector<struct iovec> iov;
  iov.resize(1);
  iov[0].iov_base = header.data();
  iov[0].iov_len = header.size();
  contents.appendToIov(&iov);

  return createOverlayFileImpl(
      inodeNumber,
      iov.data(),
      iov.size(),
      /*crashSafe=*/!directFileCreate_,
      /*removeOnFailure=*/true);
}

void FsFileContentStore::validateHeader(
    InodeNumber inodeNumber,
    folly::StringPiece contents,
    folly::StringPiece headerId) {
  if (contents.size() < kHeaderLength) {
    // Something wrong with the file (may be corrupted)
    throw OverlayDataCorruption(
        EIO,
        EdenErrorType::POSIX_ERROR,
        "Overlay file (inode ",
        inodeNumber,
        ") is too short for header: size=",
        contents.size(),
        " expected at least ",
        kHeaderLength,
        " bytes, expected headerId hex=",
        folly::hexlify(ByteRange{headerId}));
  }

  IOBuf buf(IOBuf::WRAP_BUFFER, ByteRange{contents});
  folly::io::Cursor cursor(&buf);

  // Validate header identifier
  auto id = cursor.readFixedString(kHeaderIdentifierDir.size());
  StringPiece identifier{id};
  if (identifier.compare(headerId) != 0) {
    throw OverlayDataCorruption(
        EINVAL,
        EdenErrorType::POSIX_ERROR,
        "unexpected overlay header identifier for inode ",
        inodeNumber,
        " at offset 0: actual hex=",
        folly::hexlify(ByteRange{identifier}),
        " expected hex=",
        folly::hexlify(ByteRange{headerId}));
  }

  // Validate header version
  auto version = cursor.readBE<uint32_t>();
  if (version != kHeaderVersion) {
    throw OverlayDataCorruption(
        EINVAL,
        EdenErrorType::POSIX_ERROR,
        "unexpected overlay version for inode ",
        inodeNumber,
        " at offset ",
        kHeaderIdentifierDir.size(),
        ": actual=",
        version,
        " expected=",
        kHeaderVersion);
  }
}

void FsFileContentStore::removeOverlayFile(InodeNumber inodeNumber) {
  auto path = getFilePath(inodeNumber);
  int result = ::unlinkat(dirFile_.fd(), path.c_str(), 0);
  if (result == 0) {
    XLOGF(DBG4, "removed overlay data for inode {}", inodeNumber);
  } else if (errno != ENOENT) {
    folly::throwSystemError(
        "error unlinking overlay file: ", RelativePathPiece{path}.view());
  }
}

void FsInodeCatalog::removeOverlayDir(InodeNumber inodeNumber) {
  core_->removeOverlayFile(inodeNumber);
}

bool FsFileContentStore::hasOverlayFile(InodeNumber inodeNumber) {
  // TODO: It might be worth maintaining a memory-mapped set to rapidly
  // query whether the overlay has an entry for a particular inode.  As it is,
  // this function requires a syscall to see if the overlay has an entry.
  auto path = FsFileContentStore::getFilePath(inodeNumber);
  struct stat st;
  if (0 == fstatat(dirFile_.fd(), path.c_str(), &st, AT_SYMLINK_NOFOLLOW)) {
    return S_ISREG(st.st_mode);
  } else {
    return false;
  }
}

bool FsInodeCatalog::hasOverlayDir(InodeNumber inodeNumber) {
  return core_->hasOverlayFile(inodeNumber);
}

std::optional<fsck::InodeInfo> FsFileContentStore::loadInodeInfo(
    InodeNumber number) {
  folly::File file;
  try {
    file = openFileNoVerify(number);
  } catch (const std::exception& ex) {
    return makeInodeError(
        number, "error opening file: ", folly::exceptionStr(ex));
  }

  return readInodeInfoFromFile(number, file);
}

std::optional<fsck::InodeInfo> FsInodeCatalog::loadInodeInfo(
    InodeNumber number) {
  return core_->loadInodeInfo(number);
}

std::optional<fsck::InodeInfo> FsFileContentStore::loadInodeInfoAndEntries(
    InodeNumber number,
    InodeCatalog::OverlayEntryLoader loader) {
  folly::File file;
  try {
    auto path = getFilePath(number);
    file = openFileNoVerifyWithFlags(
        dirFile_.fd(), localDir_, number, path, O_RDONLY);
  } catch (const std::exception& ex) {
    return makeInodeError(
        number, "error opening file: ", folly::exceptionStr(ex));
  }

  auto info = readInodeInfoFromFile(number, file);
  if (info.type != fsck::InodeType::Dir) {
    return info;
  }

  std::string rawData;
  if (!folly::readFile(file.fd(), rawData)) {
    return makeInodeError(
        number, "error reading directory contents: ", folly::errnoStr(errno));
  }
  loadOverlayEntriesFromData(number, rawData, loader);
  return info;
}

std::optional<fsck::InodeInfo> FsInodeCatalog::loadInodeInfoAndEntries(
    InodeNumber number,
    OverlayEntryLoader loader) {
  return core_->loadInodeInfoAndEntries(number, loader);
}
} // namespace facebook::eden

#endif

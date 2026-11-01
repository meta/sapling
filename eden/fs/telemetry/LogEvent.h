/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#pragma once

#include <folly/portability/SysTypes.h>
#include <cstdint>
#include <optional>
#include <string>
#include <string_view>
#include <utility>

#include "eden/common/os/ProcessId.h"
#include "eden/common/telemetry/DynamicEvent.h"
#include "eden/common/telemetry/LogEvent.h"
#include "eden/common/utils/ProcessInfo.h"
#include "eden/fs/telemetry/XplatKeys.h"

namespace facebook::eden {

struct EdenFSEvent : public TypedEvent {
  // Keep populate() and getType() pure virtual to force subclasses
  // to implement them
  virtual void populate(DynamicEvent&) const override = 0;
  virtual const char* getType() const override = 0;
};

struct Fsck : public EdenFSEvent {
  double duration = 0.0;
  bool success = false;
  bool attempted_repair = false;

  Fsck(double duration, bool success, bool attempted_repair)
      : duration(duration),
        success(success),
        attempted_repair(attempted_repair) {}

  void populate(DynamicEvent& event) const override {
    event.addDouble("duration", duration);
    event.addBool("success", success);
    event.addBool("attempted_repair", attempted_repair);
  }

  const char* getType() const override {
    return "fsck";
  }
};

struct StarGlob : public EdenFSEvent {
  std::string glob_request;
  std::string client_cmdline;

  StarGlob(std::string glob_request, std::string client_cmdline)
      : glob_request(std::move(glob_request)),
        client_cmdline(std::move(client_cmdline)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("glob_request", glob_request);
    event.addString("client_cmdline", client_cmdline);
  }

  const char* getType() const override {
    return "star_glob";
  }
};

struct ExpensiveGlob : public EdenFSEvent {
  double duration = 0.0;
  std::string glob_request;
  std::string client_cmdline;
  bool is_local;

  ExpensiveGlob(
      double duration,
      std::string glob_request,
      std::string client_cmdline,
      bool is_local)
      : duration(duration),
        glob_request(std::move(glob_request)),
        client_cmdline(std::move(client_cmdline)),
        is_local(is_local) {}

  void populate(DynamicEvent& event) const override {
    event.addDouble("duration", duration);
    event.addString("glob_request", glob_request);
    event.addString("client_cmdline", client_cmdline);
    event.addBool("is_local", is_local);
  }

  const char* getType() const override {
    return "expensive_glob";
  }
};

struct MissingProxyHash : public EdenFSEvent {
  void populate(DynamicEvent&) const override {}

  const char* getType() const override {
    return "missing_proxy_hash";
  }
};

struct FetchHeavy : public EdenFSEvent {
  std::string client_cmdline;
  ProcessId pid;
  uint64_t fetch_count;
  std::optional<uint64_t> loaded_inodes;
  std::optional<ProcessAttribution> client_attribution;

  FetchHeavy(
      std::string client_cmdline,
      ProcessId pid,
      uint64_t fetch_count,
      std::optional<uint64_t> loaded_inodes,
      std::optional<ProcessAttribution> client_attribution = std::nullopt)
      : client_cmdline(std::move(client_cmdline)),
        pid(std::move(pid)),
        fetch_count(fetch_count),
        loaded_inodes(loaded_inodes),
        client_attribution(std::move(client_attribution)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("client_cmdline", client_cmdline);
    event.addInt("client_pid", pid.get());
    event.addInt("fetch_count", fetch_count);
    if (loaded_inodes.has_value()) {
      event.addTruncatedInt("loaded_inodes", loaded_inodes.value(), 8U);
    }
    if (client_attribution.has_value()) {
      for (const auto& [name, value] : *client_attribution) {
        event.addString(name, value);
      }
    }
  }

  const char* getType() const override {
    return "fetch_heavy";
  }
};

struct ParentMismatch : public EdenFSEvent {
  std::string mercurial_parent;
  std::string eden_parent;

  ParentMismatch(std::string mercurial_parent, std::string eden_parent)
      : mercurial_parent(std::move(mercurial_parent)),
        eden_parent(std::move(eden_parent)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("mercurial_parent", mercurial_parent);
    event.addString("eden_parent", eden_parent);
  }

  const char* getType() const override {
    return "parent_mismatch";
  }
};

struct DaemonStart : public EdenFSEvent {
  double duration = 0.0;
  bool is_takeover = false;
  bool success = false;
  std::optional<uint64_t> daemon_mount_namespace;
  std::optional<uint64_t> daemon_pid_namespace;
  std::optional<uint64_t> privhelper_mount_namespace;
  std::optional<uint64_t> privhelper_pid_namespace;
  std::optional<bool> is_daemon_in_root_mount_namespace;
  std::optional<bool> is_privhelper_in_root_mount_namespace;
  std::optional<std::string> cgroup;
  // Restarts already spent in the current backoff window. Set only when this
  // daemon was relaunched by the privhelper, so its presence is itself the
  // "this was an auto-restart" signal.
  std::optional<uint64_t> num_restarts;

  DaemonStart(
      double duration,
      bool is_takeover,
      bool success,
      std::optional<uint64_t> daemon_mount_namespace = std::nullopt,
      std::optional<uint64_t> daemon_pid_namespace = std::nullopt,
      std::optional<uint64_t> privhelper_mount_namespace = std::nullopt,
      std::optional<uint64_t> privhelper_pid_namespace = std::nullopt,
      std::optional<bool> is_daemon_in_root_mount_namespace = std::nullopt,
      std::optional<bool> is_privhelper_in_root_mount_namespace = std::nullopt,
      std::optional<std::string> cgroup = std::nullopt,
      std::optional<uint64_t> num_restarts = std::nullopt)
      : duration(duration),
        is_takeover(is_takeover),
        success(success),
        daemon_mount_namespace(daemon_mount_namespace),
        daemon_pid_namespace(daemon_pid_namespace),
        privhelper_mount_namespace(privhelper_mount_namespace),
        privhelper_pid_namespace(privhelper_pid_namespace),
        is_daemon_in_root_mount_namespace(is_daemon_in_root_mount_namespace),
        is_privhelper_in_root_mount_namespace(
            is_privhelper_in_root_mount_namespace),
        cgroup(std::move(cgroup)),
        num_restarts(num_restarts) {}

  void populate(DynamicEvent& event) const override {
    event.addDouble("duration", duration);
    event.addBool("is_takeover", is_takeover);
    event.addBool("success", success);
    if (daemon_mount_namespace.has_value()) {
      event.addInt(
          "daemon_mount_namespace",
          static_cast<int64_t>(*daemon_mount_namespace));
    }
    if (daemon_pid_namespace.has_value()) {
      event.addInt(
          "daemon_pid_namespace", static_cast<int64_t>(*daemon_pid_namespace));
    }
    if (privhelper_mount_namespace.has_value()) {
      event.addInt(
          "privhelper_mount_namespace",
          static_cast<int64_t>(*privhelper_mount_namespace));
    }
    if (privhelper_pid_namespace.has_value()) {
      event.addInt(
          "privhelper_pid_namespace",
          static_cast<int64_t>(*privhelper_pid_namespace));
    }
    if (is_daemon_in_root_mount_namespace.has_value()) {
      event.addBool(
          "is_daemon_in_root_mount_namespace",
          *is_daemon_in_root_mount_namespace);
    }
    if (is_privhelper_in_root_mount_namespace.has_value()) {
      event.addBool(
          "is_privhelper_in_root_mount_namespace",
          *is_privhelper_in_root_mount_namespace);
    }
    if (cgroup.has_value()) {
      event.addString("cgroup", *cgroup);
    }
    if (num_restarts.has_value()) {
      event.addInt("num_restarts", static_cast<int64_t>(*num_restarts));
    }
  }

  const char* getType() const override {
    return "daemon_start";
  }
};

struct DaemonStop : public EdenFSEvent {
  double duration = 0.0;
  bool is_takeover = false;
  bool success = false;

  DaemonStop(double duration, bool is_takeover, bool success)
      : duration(duration), is_takeover(is_takeover), success(success) {}

  void populate(DynamicEvent& event) const override {
    event.addDouble("duration", duration);
    event.addBool("is_takeover", is_takeover);
    event.addBool("success", success);
  }

  const char* getType() const override {
    return "daemon_stop";
  }
};

/**
 * A graceful restart was refused because Thrift requests were still running
 * after core:takeover-request-grace. `requests` names them.
 */
struct TakeoverRefused : public EdenFSEvent {
  std::string requests;
  double oldest_request_age_s = 0.0;

  TakeoverRefused(std::string requests, double oldest_request_age_s)
      : requests(std::move(requests)),
        oldest_request_age_s(oldest_request_age_s) {}

  void populate(DynamicEvent& event) const override {
    event.addString("reason", requests);
    event.addDouble("duration", oldest_request_age_s);
  }

  const char* getType() const override {
    return "takeover_refused";
  }
};

struct PrivhelperShutdown : public EdenFSEvent {
  int64_t exit_code = 0;
  int64_t exit_signal = 0;

  PrivhelperShutdown(int64_t exit_code, int64_t exit_signal)
      : exit_code(exit_code), exit_signal(exit_signal) {}

  void populate(DynamicEvent& event) const override {
    event.addInt("exit_code", exit_code);
    event.addInt("exit_signal", exit_signal);
  }

  const char* getType() const override {
    return "privhelper_shutdown";
  }
};

struct PrivhelperRequestStall : public EdenFSEvent {
  std::string method;
  double duration = 0.0; // seconds

  PrivhelperRequestStall(std::string method, double duration)
      : method(std::move(method)), duration(duration) {}

  void populate(DynamicEvent& event) const override {
    event.addString("method", method);
    event.addDouble("duration", duration);
  }

  const char* getType() const override {
    return "privhelper_request_stall";
  }
};

struct FinishedCheckout : public EdenFSEvent {
  std::string mode;
  double duration = 0.0;
  bool success = false;
  uint64_t fetchedTrees = 0;
  uint64_t fetchedBlobs = 0;
  uint64_t fetchedBlobsAuxData = 0;
  uint64_t accessedTrees = 0;
  uint64_t accessedBlobs = 0;
  uint64_t accessedBlobsAuxData = 0;
  uint64_t numConflicts = 0;
  uint64_t numErrors = 0;
  std::string error;
  uint64_t numLoadedInodes = 0;
  uint64_t numUnloadedInodes = 0;
  uint64_t numPeriodicLinkedUnloadedInodes = 0;
  uint64_t numPeriodicUnlinkedUnloadedInodes = 0;
  // durations of checkout phases
  std::optional<uint64_t> durationLookupTrees = std::nullopt;
  std::optional<uint64_t> durationDiff = std::nullopt;
  std::optional<uint64_t> durationAcquireRenameLock = std::nullopt;
  std::optional<uint64_t> durationCheckout = std::nullopt;
  std::optional<uint64_t> durationFinish = std::nullopt;

  FinishedCheckout(
      std::string mode,
      double duration,
      bool success,
      uint64_t fetchedTrees,
      uint64_t fetchedBlobs,
      uint64_t fetchedBlobsAuxData,
      uint64_t accessedTrees,
      uint64_t accessedBlobs,
      uint64_t accessedBlobsAuxData,
      uint64_t numConflicts,
      uint64_t numLoadedInodes,
      uint64_t numUnloadedInodes,
      uint64_t numPeriodicLinkedUnloadedInodes,
      uint64_t numPeriodicUnlinkedUnloadedInodes)
      : mode(std::move(mode)),
        duration(duration),
        success(success),
        fetchedTrees(fetchedTrees),
        fetchedBlobs(fetchedBlobs),
        fetchedBlobsAuxData(fetchedBlobsAuxData),
        accessedTrees(accessedTrees),
        accessedBlobs(accessedBlobs),
        accessedBlobsAuxData(accessedBlobsAuxData),
        numConflicts(numConflicts),
        numLoadedInodes(numLoadedInodes),
        numUnloadedInodes(numUnloadedInodes),
        numPeriodicLinkedUnloadedInodes(numPeriodicLinkedUnloadedInodes),
        numPeriodicUnlinkedUnloadedInodes(numPeriodicUnlinkedUnloadedInodes) {}

  void populateCheckoutDurations(
      uint64_t durationLookupTrees_,
      uint64_t durationDiff_,
      uint64_t durationAcquireRenameLock_,
      uint64_t durationCheckout_,
      uint64_t durationFinish_) {
    durationLookupTrees = durationLookupTrees_;
    durationDiff = durationDiff_;
    durationAcquireRenameLock = durationAcquireRenameLock_;
    durationCheckout = durationCheckout_;
    durationFinish = durationFinish_;
  }

  void populateError(uint64_t numErrors_, std::string error_) {
    numErrors = numErrors_;
    error = std::move(error_);
  }

  void populate(DynamicEvent& event) const override {
    event.addString("mode", mode);
    event.addDouble("duration", duration);
    event.addBool("success", success);
    event.addInt("fetched_trees", fetchedTrees);
    event.addInt("fetched_blobs", fetchedBlobs);
    event.addInt("fetched_blobs_metadata", fetchedBlobsAuxData);
    event.addInt("accessed_trees", accessedTrees);
    event.addInt("accessed_blobs", accessedBlobs);
    event.addInt("accessed_blobs_metadata", accessedBlobsAuxData);
    event.addInt("num_conflicts", numConflicts);
    event.addInt("num_errors", numErrors);
    if (!error.empty()) {
      event.addString("error", error);
    }
    event.addInt("loaded_inodes", numLoadedInodes);
    event.addInt("unloaded_inodes", numUnloadedInodes);
    event.addInt("linked_unloaded_inodes", numPeriodicLinkedUnloadedInodes);
    event.addInt("unlinked_unloaded_inodes", numPeriodicUnlinkedUnloadedInodes);
    if (durationLookupTrees) {
      event.addInt("duration_lookup_trees", durationLookupTrees.value());
    }
    if (durationDiff) {
      event.addInt("duration_diff", durationDiff.value());
    }
    if (durationAcquireRenameLock) {
      event.addInt(
          "duration_acquire_rename_lock", durationAcquireRenameLock.value());
    }
    if (durationCheckout) {
      event.addInt("duration_checkout", durationCheckout.value());
    }
    if (durationFinish) {
      event.addInt("duration_finish", durationFinish.value());
    }
  }

  const char* getType() const override {
    return "checkout";
  }
};

struct ThriftCancellation : public EdenFSEvent {
  std::string endpoint;
  bool success = false;
  double duration = 0.0;

  ThriftCancellation(std::string endpoint, bool success, double duration)
      : endpoint(std::move(endpoint)), success(success), duration(duration) {}

  void populate(DynamicEvent& event) const override {
    event.addString("endpoint", endpoint);
    event.addBool("success", success);
    event.addDouble("duration", duration);
  }

  const char* getType() const override {
    return "thrift_cancellation";
  }
};

struct NFSStaleError : public EdenFSEvent {
  void populate(DynamicEvent& /*event*/) const override {}

  const char* getType() const override {
    return "nfs_stale_error";
  }
};

struct FinishedMount : public EdenFSEvent {
  std::string backing_store_type;
  std::string repo_type;
  std::string repo_source;
  std::string fs_channel_type;
  std::optional<std::string> fuse_transport;
  bool is_takeover = false;
  double duration = 0.0;
  bool success = false;
  bool clean = false;
  int64_t inode_catalog_type = -1;

  FinishedMount(
      std::string backing_store_type,
      std::string repo_type,
      std::string repo_source,
      std::string fs_channel_type,
      std::optional<std::string> fuse_transport,
      bool is_takeover,
      double duration,
      bool success,
      bool clean,
      int64_t inode_catalog_type)
      : backing_store_type(std::move(backing_store_type)),
        repo_type(std::move(repo_type)),
        repo_source(std::move(repo_source)),
        fs_channel_type(std::move(fs_channel_type)),
        fuse_transport(std::move(fuse_transport)),
        is_takeover(is_takeover),
        duration(duration),
        success(success),
        clean(clean),
        inode_catalog_type(inode_catalog_type) {}

  void populate(DynamicEvent& event) const override {
    event.addString("backing_store_type", backing_store_type);
    event.addString("repo_type", repo_type);
    event.addString("repo_source", repo_source);
    event.addString("fs_channel_type", fs_channel_type);
    if (fuse_transport.has_value()) {
      event.addString("fuse_transport", *fuse_transport);
    }
    event.addBool("is_takeover", is_takeover);
    event.addDouble("duration", duration);
    event.addBool("success", success);
    event.addBool("clean", clean);
    event.addInt("overlay_type", inode_catalog_type);
  }

  const char* getType() const override {
    return "mount";
  }
};

struct ServerDataFetch : public EdenFSEvent {
  std::string cause;
  OptionalProcessId client_pid;
  std::optional<std::string> client_cmdline;
  std::string fetched_path;
  std::string fetched_object_type;

  ServerDataFetch(
      std::string cause,
      OptionalProcessId client_pid,
      std::optional<std::string> client_cmdline,
      std::string fetched_path,
      std::string fetched_object_type)
      : cause(std::move(cause)),
        client_pid(std::move(client_pid)),
        client_cmdline(std::move(client_cmdline)),
        fetched_path(std::move(fetched_path)),
        fetched_object_type(std::move(fetched_object_type)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("interface", cause);
    if (client_pid) {
      event.addInt("client_pid", client_pid.value().get());
    }
    if (client_cmdline) {
      event.addString("client_cmdline", client_cmdline.value());
    }
    event.addString("fetched_path", fetched_path);
    event.addString("fetched_object_type", fetched_object_type);
  }

  const char* getType() const override {
    return "server_data_fetch";
  }
};

struct NfsParsingError : public EdenFSEvent {
  std::string proc;
  std::string reason;

  NfsParsingError(std::string proc, std::string reason)
      : proc(std::move(proc)), reason(std::move(reason)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("interface", proc);
    event.addString("reason", reason);
  }

  const char* getType() const override {
    return "nfs_parsing_error";
  }
};

struct TccInvalidationDenied : public EdenFSEvent {
  int err;
  std::string path;

  TccInvalidationDenied(int err, std::string path)
      : err(err), path(std::move(path)) {}

  void populate(DynamicEvent& event) const override {
    event.addInt("errno", err);
    event.addString("path", path);
  }

  const char* getType() const override {
    return "tcc_invalidation_denied";
  }
};

/**
 * edenfs did not disclaim TCC responsibility for `process` ("daemon" or
 * "privhelper") because its code signature carries a real team identifier
 * that is not the expected one: a development certificate, or a rotated
 * release team. Ad-hoc builds carry no team and are not reported.
 */
struct TccDisclaimSkipped : public EdenFSEvent {
  std::string process;
  std::string observed_team;
  std::string expected_team;

  TccDisclaimSkipped(
      std::string process,
      std::string observed_team,
      std::string expected_team)
      : process(std::move(process)),
        observed_team(std::move(observed_team)),
        expected_team(std::move(expected_team)) {}

  void populate(DynamicEvent& event) const override {
    event.addString(std::string{xplat_keys::kTccDisclaimProcess}, process);
    event.addString(std::string{xplat_keys::kTccObservedTeam}, observed_team);
    event.addString(std::string{xplat_keys::kTccExpectedTeam}, expected_team);
  }

  const char* getType() const override {
    return "tcc_disclaim_skipped";
  }
};

struct TooManyNfsClients : public EdenFSEvent {
  void populate(DynamicEvent& /*event*/) const override {}

  const char* getType() const override {
    return "too_many_clients";
  }
};

struct MetadataSizeMismatch : public EdenFSEvent {
  std::string mount_protocol;
  std::string method;

  MetadataSizeMismatch(std::string mount_protocol, std::string method)
      : mount_protocol(std::move(mount_protocol)), method(std::move(method)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("mount_protocol", mount_protocol);
    event.addString("method", method);
  }

  const char* getType() const override {
    return "metadata_size_mismatch";
  }
};

struct InodeMetadataMismatch : public EdenFSEvent {
  uint64_t mode;
  uint64_t gid;
  uint64_t uid;
  uint64_t atime;
  uint64_t ctime;
  uint64_t mtime;

  InodeMetadataMismatch(
      uint64_t mode,
      uint64_t gid,
      uint64_t uid,
      uint64_t atime,
      uint64_t ctime,
      uint64_t mtime)
      : mode(mode),
        gid(gid),
        uid(uid),
        atime(atime),
        ctime(ctime),
        mtime(mtime) {}

  void populate(DynamicEvent& event) const override {
    event.addInt("st_mode", mode);
    event.addInt("gid", gid);
    event.addInt("uid", uid);
    event.addInt("atime", atime);
    event.addInt("ctime", ctime);
    event.addInt("mtime", mtime);
  }

  const char* getType() const override {
    return "inode_metadata_mismatch";
  }
};

struct EMenuStartupFailure : public EdenFSEvent {
  std::string reason;

  explicit EMenuStartupFailure(std::string reason)
      : reason(std::move(reason)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("reason", reason);
  }

  const char* getType() const override {
    return "emenu_startup_failure";
  }
};

struct PrjFSFileNotificationFailure : public EdenFSEvent {
  std::string reason;
  std::string path;

  PrjFSFileNotificationFailure(std::string reason, std::string path)
      : reason(std::move(reason)), path(std::move(path)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("reason", reason);
    event.addString("path", path);
  }

  const char* getType() const override {
    return "prjfs_file_notification_failure";
  }
};

struct PrjFSCheckoutReadRace : public EdenFSEvent {
  std::string client_cmdline;

  explicit PrjFSCheckoutReadRace(std::string client_cmdline)
      : client_cmdline(std::move(client_cmdline)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("client_cmdline", client_cmdline);
  }

  const char* getType() const override {
    return "prjfs_checkout_read_race";
  }
};

struct WorkingCopyGc : public EdenFSEvent {
  double duration = 0.0;
  int64_t numInvalidated = 0;
  bool success = false;
  int64_t numOfDeletedInodes = 0;

  WorkingCopyGc(
      double duration,
      int64_t numInvalidated,
      bool success,
      int64_t numOfDeletedInodes)
      : duration(duration),
        numInvalidated(numInvalidated),
        success(success),
        numOfDeletedInodes(numOfDeletedInodes) {}

  void populate(DynamicEvent& event) const override {
    event.addDouble("duration", duration);
    event.addInt("num_invalidated", numInvalidated);
    event.addBool("success", success);
    event.addInt("num_deleted_inodes", numOfDeletedInodes);
  }

  const char* getType() const override {
    return "working_copy_gc";
  }
};

/**
 * A pin scan (`edenfs_privhelper --scan-pins`) that produced no usable
 * report, so the GC run that asked for it treated pins as unknown. `reason`
 * is one of spawn_error, poll_error, read_error, timeout, output_too_large,
 * exit_status and malformed_output from running the helper, or
 * mounts_unreadable and mount_not_covered from mapping its report to this
 * daemon's mounts.
 */
struct PinScanFailure : public EdenFSEvent {
  std::string reason;
  // Errno text, exit status, or the mount the scan did not cover.
  std::string detail;
  // The start of the helper's report and the end of its progress trail.
  std::string stdoutPrefix;
  std::string stderrTail;
  int64_t durationMs = 0;

  PinScanFailure(std::string reason, std::string detail)
      : reason(std::move(reason)), detail(std::move(detail)) {}

  // Only columns the edenfs_events logger schema declares reach Scuba, so
  // the fields ride in existing ones: `error` holds the detail, `causeDetail`
  // the helper's stderr followed by its stdout when there is any, and
  // `duration` is in seconds like the other GC events.
  void populate(DynamicEvent& event) const override {
    event.addString(std::string{xplat_keys::kReason}, reason);
    event.addString(
        std::string{xplat_keys::kError}, truncated(detail, kMaxDetail));
    event.addString(std::string{xplat_keys::kCauseDetail}, helperOutput());
    event.addDouble(std::string{xplat_keys::kDuration}, durationMs / 1000.0);
  }

  static constexpr size_t kMaxDetail = 256;
  static constexpr size_t kMaxOutput = 1024;

  static std::string truncated(const std::string& s, size_t max) {
    return s.size() <= max ? s : s.substr(0, max);
  }

  // The end of stderr is the helper's last progress line, which is what a
  // timed-out scan is judged by, so stderr keeps its tail and stdout, the
  // report, its head.
  std::string helperOutput() const {
    std::string out = stderrTail.size() <= kMaxOutput
        ? stderrTail
        : stderrTail.substr(stderrTail.size() - kMaxOutput);
    if (!stdoutPrefix.empty() && out.size() < kMaxOutput) {
      if (!out.empty() && out.back() != '\n') {
        out += '\n';
      }
      out += "[stdout] ";
      out += stdoutPrefix;
    }
    return truncated(out, kMaxOutput);
  }

  const char* getType() const override {
    return "pin_scan_failure";
  }
};

struct SilentDaemonExit : public EdenFSEvent {
  uint64_t last_daemon_heartbeat = 0;
  uint8_t daemon_exit_signal = 0;
  uint64_t system_boot_timestamp = 0;
  std::optional<bool> is_memory_pressure_kill = std::nullopt;
  std::string system_log_check_error_str;
  std::optional<uint64_t> daemon_downtime_s = std::nullopt;

  SilentDaemonExit(
      uint64_t last_daemon_heartbeat,
      uint8_t daemon_exit_signal,
      uint64_t system_boot_timestamp = 0,
      std::optional<bool> is_memory_pressure_kill = std::nullopt,
      std::string system_log_check_error_str = "",
      std::optional<uint64_t> daemon_downtime_s = std::nullopt)
      : last_daemon_heartbeat(last_daemon_heartbeat),
        daemon_exit_signal(daemon_exit_signal),
        system_boot_timestamp(system_boot_timestamp),
        is_memory_pressure_kill(is_memory_pressure_kill),
        system_log_check_error_str(std::move(system_log_check_error_str)),
        daemon_downtime_s(daemon_downtime_s) {}

  void populate(DynamicEvent& event) const override {
    event.addInt("last_daemon_heartbeat", last_daemon_heartbeat);
    event.addInt("exit_signal", daemon_exit_signal);
    event.addInt("system_boot_timestamp", system_boot_timestamp);
    if (is_memory_pressure_kill.has_value()) {
      event.addBool("is_memory_pressure_kill", is_memory_pressure_kill.value());
    }
    if (!system_log_check_error_str.empty()) {
      event.addString("system_log_check_error", system_log_check_error_str);
    }
    if (daemon_downtime_s.has_value()) {
      event.addInt("daemon_downtime_s", daemon_downtime_s.value());
    }
  }

  const char* getType() const override {
    return "silent_daemon_exit";
  }
};

struct PrivHelperExit : public EdenFSEvent {
  std::string reason;

  explicit PrivHelperExit(std::string reason) : reason(std::move(reason)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("reason", reason);
  }

  const char* getType() const override {
    return "privhelper_exit";
  }
};

// What started a checkout recovery attempt. Its name is logged in the
// `source` column of accidental_unmount_recovery events.
enum class CheckoutRecoveryTrigger : uint8_t {
  PeriodicTask,
};

constexpr std::string_view checkoutRecoveryTriggerName(
    CheckoutRecoveryTrigger trigger) {
  switch (trigger) {
    case CheckoutRecoveryTrigger::PeriodicTask:
      return "periodic_task";
  }
  return "unknown";
}

struct AccidentalUnmountRecovery : public EdenFSEvent {
  std::string error;
  bool success = false;
  std::string repo_source;
  std::string mount_path;
  CheckoutRecoveryTrigger trigger;
  // 1 for the first attempt.
  int64_t attempt = 0;
  double duration = 0.0;
  std::string mount_protocol;
  // Set on the failed attempt after which recovery gave up on the checkout.
  bool gave_up = false;
  // Why the daemon stopped serving the checkout.
  std::string stop_reason;
  // Time from that stop until this attempt ended. Unset when this daemon never
  // served the checkout.
  std::optional<int64_t> outage_age_ms;

  AccidentalUnmountRecovery(
      std::string error,
      bool success,
      std::string repo_source,
      std::string mount_path,
      CheckoutRecoveryTrigger trigger,
      int64_t attempt,
      double duration,
      std::string mount_protocol,
      std::string stop_reason)
      : error(std::move(error)),
        success(success),
        repo_source(std::move(repo_source)),
        mount_path(std::move(mount_path)),
        trigger(trigger),
        attempt(attempt),
        duration(duration),
        mount_protocol(std::move(mount_protocol)),
        stop_reason(std::move(stop_reason)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("remount_error", error);
    event.addBool("success", success);
    event.addString("repo_source", repo_source);
    event.addString("mount_path", mount_path);
    event.addString(
        "source", std::string{checkoutRecoveryTriggerName(trigger)});
    event.addInt("retry", attempt);
    event.addDouble("duration", duration);
    event.addString("mount_protocol", mount_protocol);
    event.addString("reason", stop_reason);
    event.addString(
        "causeDetail",
        outage_age_ms ? fmt::format(
                            "{{\"gave_up\":{},\"outage_age_ms\":{}}}",
                            gave_up,
                            *outage_age_ms)
                      : fmt::format("{{\"gave_up\":{}}}", gave_up));
  }

  const char* getType() const override {
    return "accidental_unmount_recovery";
  }
};

struct EdenMountHealthIssue : public EdenFSEvent {
  std::string source;
  std::string reason;
  std::string mount_path;
  std::string checkout_path;
  std::string path_type;
  std::string repo_source;
  std::string error;
  bool attempted_repair = false;
  bool success = false;

  EdenMountHealthIssue(
      std::string source,
      std::string reason,
      std::string mount_path,
      std::string checkout_path,
      std::string path_type,
      std::string repo_source = "",
      std::string error = "",
      bool attempted_repair = false,
      bool success = false)
      : source(std::move(source)),
        reason(std::move(reason)),
        mount_path(std::move(mount_path)),
        checkout_path(std::move(checkout_path)),
        path_type(std::move(path_type)),
        repo_source(std::move(repo_source)),
        error(std::move(error)),
        attempted_repair(attempted_repair),
        success(success) {}

  void populate(DynamicEvent& event) const override {
    event.addString("source", source);
    event.addString("reason", reason);
    event.addString("mount_path", mount_path);
    event.addString("checkout_path", checkout_path);
    event.addString("path_type", path_type);
    if (!repo_source.empty()) {
      event.addString("repo_source", repo_source);
    }
    if (!error.empty()) {
      event.addString("error", error);
    }
    event.addBool("attempted_repair", attempted_repair);
    event.addBool("success", success);
  }

  const char* getType() const override {
    return "eden_mount_health_issue";
  }
};

struct FetchMiss : public EdenFSEvent {
  enum MissType : uint8_t {
    Tree = 0,
    Blob = 1,
    BlobAuxData = 2,
    TreeAuxData = 3
  };

  std::string_view missTypeToString(MissType miss) const {
    switch (miss) {
      case Tree:
        return "Tree";
      case Blob:
        return "Blob";
      case BlobAuxData:
        return "BlobAuxData";
      case TreeAuxData:
        return "TreeAuxData";
      default:
        return "Unknown";
    }
  }

  std::string_view repo_source;
  MissType miss_type;
  std::string reason;
  bool retry;
  bool dogfooding_host;

  FetchMiss(
      std::string_view repo_source,
      MissType miss_type,
      std::string reason,
      bool retry,
      bool dogfooding_host)
      : repo_source(repo_source),
        miss_type(miss_type),
        reason(std::move(reason)),
        retry(retry),
        dogfooding_host(dogfooding_host) {}

  void populate(DynamicEvent& event) const override {
    event.addString("repo_source", std::string(repo_source));
    if (miss_type == Tree) {
      event.addString("miss_type", "tree");
    } else if (miss_type == Blob) {
      event.addString("miss_type", "blob");
    } else if (miss_type == BlobAuxData) {
      event.addString("miss_type", "blob_aux");
    } else if (miss_type == TreeAuxData) {
      event.addString("miss_type", "tree_aux");
    } else {
      throw std::range_error(
          fmt::format("Unknown miss type: {}", missTypeToString(miss_type)));
    }
    event.addString("reason", reason);
    event.addBool("retry", retry);
    event.addBool("dogfooding_host", dogfooding_host);
  }

  const char* getType() const override {
    return "fetch_miss";
  }
};

/**
 * So that we know how many hosts have EdenFS handling high numbers of fuse
 * requests at once as we rollout rate limiting.
 *
 * This honestly could be an ODS counter, but we don't have ODS on some
 * platforms (CI), so logging it to scuba so that we have this available to
 * monitor on all platforms.
 */
struct ManyLiveFsChannelRequests : public EdenFSEvent {
  void populate(DynamicEvent& /*event*/) const override {}

  const char* getType() const override {
    return "high_fschannel_requests";
  }
};

/**
 * Used to log user actions on e-Menu
 */
struct EMenuActionEvent : public EdenFSEvent {
  enum ActionType : uint8_t { EMenuClick = 0 };

  ActionType action_type;

  explicit EMenuActionEvent(ActionType action_type)
      : action_type(action_type) {}

  void populate(DynamicEvent& event) const override {
    if (action_type == EMenuClick) {
      event.addString("action_type", "EMenuClick");
    }
  }

  const char* getType() const override {
    return "e_menu_action_events";
  }
};

/**
 * Indicates that a FS request (through FUSE, NFS, PrjFS) took longer than a set
 * threshold.
 */
struct LongRunningFSRequest : public EdenFSEvent {
  double duration = 0.0;
  std::string causeDetail;
  double acceptedNs = -1.0;
  double queueWaitNs = -1.0;
  double processingNs = -1.0;
  double writeWaitNs = -1.0;

  LongRunningFSRequest(double duration, std::string_view detail)
      : duration(duration), causeDetail(detail) {}

  LongRunningFSRequest(
      double duration,
      std::string_view detail,
      double accepted,
      double queueWait,
      double processing,
      double writeWait)
      : duration(duration),
        causeDetail(detail),
        acceptedNs(accepted),
        queueWaitNs(queueWait),
        processingNs(processing),
        writeWaitNs(writeWait) {}

  void populate(DynamicEvent& event) const override {
    // Duration in nanoseconds
    event.addDouble("duration", duration);
    event.addString("causeDetail", causeDetail);
    if (acceptedNs >= 0) {
      event.addDouble("acceptedNs", acceptedNs);
    }
    if (queueWaitNs >= 0) {
      event.addDouble("queueWaitNs", queueWaitNs);
    }
    if (processingNs >= 0) {
      event.addDouble("processingNs", processingNs);
    }
    if (writeWaitNs >= 0) {
      event.addDouble("writeWaitNs", writeWaitNs);
    }
  }

  const char* getType() const override {
    return "long_running_fs_request";
  }
};

struct CheckoutUpdateError : public EdenFSEvent {
  std::string path;
  std::string reason;

  CheckoutUpdateError(std::string path, std::string reason)
      : path(std::move(path)), reason(std::move(reason)) {}

  void populate(DynamicEvent& event) const override {
    event.addString("path", path);
    event.addString("reason", reason);
  }

  const char* getType() const override {
    return "checkout_update_error";
  }
};

struct ChangesSince : public EdenFSEvent {
  std::string client_cmdline;
  std::string position;
  std::string mount;
  std::string root;
  std::vector<std::string> included_roots;
  std::vector<std::string> excluded_roots;
  std::vector<std::string> included_suffixes;
  std::vector<std::string> excluded_suffixes;
  bool include_vcs;
  uint64_t num_small_changes;
  uint64_t num_state_changes;
  uint64_t num_renamed_directories;
  uint64_t num_commit_transitions;
  std::optional<uint64_t> lost_changes;
  uint64_t num_filtered_changes;

  ChangesSince(
      std::string client_cmdline,
      std::string position,
      std::string mount,
      std::string root,
      std::vector<std::string> included_roots,
      std::vector<std::string> excluded_roots,
      std::vector<std::string> included_suffixes,
      std::vector<std::string> excluded_suffixes,
      bool include_vcs,
      uint64_t num_small_changes,
      uint64_t num_state_changes,
      uint64_t num_renamed_directories,
      uint64_t num_commit_transitions,
      std::optional<uint64_t> lost_changes,
      uint64_t num_filtered_changes)
      : client_cmdline(std::move(client_cmdline)),
        position(std::move(position)),
        mount(std::move(mount)),
        root(std::move(root)),
        included_roots(std::move(included_roots)),
        excluded_roots(std::move(excluded_roots)),
        included_suffixes(std::move(included_suffixes)),
        excluded_suffixes(std::move(excluded_suffixes)),
        include_vcs(include_vcs),
        num_small_changes(num_small_changes),
        num_state_changes(num_state_changes),
        num_renamed_directories(num_renamed_directories),
        num_commit_transitions(num_commit_transitions),
        lost_changes(lost_changes),
        num_filtered_changes(num_filtered_changes) {}

  void populate(DynamicEvent& event) const override {
    event.addString("client_cmdline", client_cmdline);
    event.addString("position", position);
    event.addString("mount", mount);
    event.addString("root", root);
    event.addStringVec("included_roots", included_roots);
    event.addStringVec("excluded_roots", excluded_roots);
    event.addStringVec("included_suffixes", included_suffixes);
    event.addStringVec("excluded_suffixes", excluded_suffixes);
    event.addBool("include_vcs", include_vcs);
    event.addInt("num_small_changes", num_small_changes);
    event.addInt("num_state_changes", num_state_changes);
    event.addInt("num_renamed_directory", num_renamed_directories);
    event.addInt("num_commit_transition", num_commit_transitions);
    if (lost_changes.has_value()) {
      event.addInt("lost_changes", lost_changes.value());
    }
    event.addInt("num_filtered_changes", num_filtered_changes);
  }

  const char* getType() const override {
    return "changes_since";
  }
};

struct StaleRedirectionCleanup : public EdenFSEvent {
  std::string checkout_path;
  int64_t stale_redirections_found;
  int64_t stale_redirections_succeeded;
  int64_t stale_redirections_failed;
  bool stale_checkout_mount_unmounted;
  // Derived: true when stale_redirections_failed == 0. Does not account for
  // stale_checkout_mount_unmounted, which tracks a separate cleanup step.
  bool success;

  StaleRedirectionCleanup(
      std::string checkout_path,
      int64_t found,
      int64_t succeeded,
      int64_t failed,
      bool checkoutUnmounted)
      : checkout_path(std::move(checkout_path)),
        stale_redirections_found(found),
        stale_redirections_succeeded(succeeded),
        stale_redirections_failed(failed),
        stale_checkout_mount_unmounted(checkoutUnmounted),
        success(failed == 0) {}

  void populate(DynamicEvent& event) const override {
    event.addString("checkout_path", checkout_path);
    event.addInt("stale_redirections_found", stale_redirections_found);
    event.addInt("stale_redirections_succeeded", stale_redirections_succeeded);
    event.addInt("stale_redirections_failed", stale_redirections_failed);
    event.addBool(
        "stale_checkout_mount_unmounted", stale_checkout_mount_unmounted);
    event.addBool("success", success);
  }

  const char* getType() const override {
    return "stale_redirection_cleanup";
  }
};

} // namespace facebook::eden

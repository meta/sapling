/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#include "eden/fs/config/TomlFileConfigSource.h"

#include <cpptoml.h>

#include <folly/File.h>
#include <folly/FileUtil.h>
#include <folly/MapUtil.h>

#include <map>
#include <string_view>
#include <vector>

#include "eden/fs/config/ConfigSetting.h"
#include "eden/fs/config/DeadConfigKeys.h"

namespace facebook::eden {

TomlFileConfigSource::TomlFileConfigSource(
    AbsolutePath path,
    ConfigSourceType sourceType,
    std::optional<EdenVersion> buildVersion)
    : path_{std::move(path)},
      sourceType_{sourceType},
      buildVersion_{buildVersion} {}

FileChangeReason TomlFileConfigSource::shouldReload() {
  std::optional<FileStat> currentStat;

  // It's okay to stat() and then perhaps open(). There's no TOCTOU, because
  // stat() is only used to determine whether opening again makes sense, and the
  // configuration will converge either way.
  auto stat = getFileStat(path_.c_str());
  if (stat.hasError()) {
    // Treat config file as if not present on error.
    // Log error if not ENOENT as they are unexpected and useful for debugging.
    if (stat.error() != ENOENT) {
      XLOGF(
          WARN,
          "error accessing config file {}: {}",
          path_,
          folly::errnoStr(stat.error()));
    }
  } else {
    currentStat = stat.value();
  }

  if (lastStat_ && currentStat) {
    return hasFileChanged(*lastStat_, *currentStat);
  } else if (lastStat_ || currentStat) {
    // Treat existing -> missing and missing -> existing as the size changing.
    return FileChangeReason::SIZE;
  } else {
    return FileChangeReason::NONE;
  }
}

void TomlFileConfigSource::reload(
    const ConfigVariables& substitutions,
    ConfigSettingMap& map) {
  auto fd = folly::fileops::open(path_.c_str(), O_RDONLY);
  if (fd == -1) {
    auto err = errno;
    if (err != ENOENT) {
      XLOGF(
          WARN,
          "error opening config file {}: {}",
          path_,
          folly::errnoStr(err));
    }
    // TODO: If a config is deleted from underneath, should we clear configs
    // sourced from that file? For now, we intentionally choose not to.
    lastStat_ = std::nullopt;
    return;
  }
  folly::File configFile(fd, /*ownsFd=*/true);

  auto result = getFileStat(fd);
  if (result.hasError()) {
    XLOGF(
        WARN,
        "error stat()ing config file {}: {}",
        path_,
        folly::errnoStr(result.error()));
    lastStat_ = std::nullopt;
  } else {
    lastStat_ = result.value();
  }

  parseAndApply(configFile.fd(), substitutions, map);
}

namespace {
// This is a bit gross.  We have enough type information in the toml
// file to know when an option is a boolean or array, but at the moment our
// intermediate layer stringly-types all the data.  When the upper
// layers want to consume a bool or array, they expect to do so by consuming
// the string representation of it.
// This helper performs the reverse transformation so that we allow
// users to specify their configuration as a true boolean or array type.
std::optional<std::string> valueAsString(const cpptoml::base& value) {
  if (auto valueStr = value.as<std::string>()) {
    return valueStr->get();
  }

  if (auto valueBool = value.as<bool>()) {
    return valueBool->get() ? "true" : "false";
  }

  if (value.is_array()) {
    // reserialize using cpptoml
    // re-serialize using cpp-toml
    std::ostringstream stringifiedValue;
    stringifiedValue << *value.clone()->as_array();
    return stringifiedValue.str();
  }

  return std::nullopt;
}

bool isDeadSection(
    std::string_view sectionName,
    const std::shared_ptr<cpptoml::base>& section) {
  auto table = section->as_table();
  if (!table) {
    return false;
  }
  std::vector<std::string_view> keys;
  for (const auto& [key, value] : *table) {
    keys.push_back(key);
  }
  return isDeadConfigSection(sectionName, keys);
}
} // namespace

void TomlFileConfigSource::parseAndApply(
    int configFd,
    const ConfigVariables& substitutions,
    ConfigSettingMap& map) {
  std::shared_ptr<cpptoml::table> configRoot;

  try {
    std::string fileContents;
    if (!folly::readFile(configFd, fileContents)) {
      XLOGF(WARNING, "Failed to read config file: {}", path_);
      return;
    }
    std::istringstream is{fileContents};
    cpptoml::parser p{is};
    configRoot = p.parse();
  } catch (const cpptoml::parse_exception& ex) {
    XLOGF(
        WARNING,
        "Failed to parse config file: {}. Skipping, error: ",
        path_,
        ex.what());
    return;
  }

  // Report unknown sections
  for (const auto& [sectionName, section] : *configRoot) {
    auto* configMapEntry = folly::get_ptr(map, sectionName);
    if (!configMapEntry) {
      if (!isDeadSection(sectionName, section)) {
        XLOGF(
            WARNING,
            "Ignoring unknown section in eden config: {}, key: {}",
            path_,
            sectionName);
      }
      continue;
    }
    // Load section
    auto sectionTable = section->as_table();
    if (!sectionTable) {
      // If it's some other type, ignore it.
      continue;
    }

    // Resolve version gates before applying anything. A satisfied gate beats
    // the plain entry and a higher satisfied gate beats a lower one, and table
    // iteration order cannot be relied on to express that.
    struct ChosenEntry {
      std::optional<EdenVersion> gate;
      std::shared_ptr<cpptoml::base> value;
    };
    std::map<std::string, ChosenEntry> chosenEntries;
    for (const auto& [rawKey, entryValue] : *sectionTable) {
      std::string entryKey = rawKey;
      std::optional<EdenVersion> gate;
      if (auto gated = parseMinVersionGatedKey(rawKey)) {
        gate = parseEdenVersion(gated->minVersion);
        if (!gate) {
          XLOGF(
              WARNING,
              "Ignoring config entry with malformed min-version {} {}:{}",
              path_,
              sectionName,
              rawKey);
          continue;
        }
        if (buildVersion_ && *buildVersion_ < *gate) {
          XLOGF(
              DBG2,
              "Skipping config entry {} {}:{}: requires a newer EdenFS",
              path_,
              sectionName,
              rawKey);
          continue;
        }
        entryKey = std::string{gated->name};
      }
      auto& chosen = chosenEntries[entryKey];
      bool gateBeatsChosen = gate && (!chosen.gate || *chosen.gate < *gate);
      if (!chosen.value || gateBeatsChosen) {
        chosen = ChosenEntry{gate, entryValue};
      }
    }

    // Report unknown config settings.
    for (const auto& [entryKey, chosen] : chosenEntries) {
      const auto& entryValue = chosen.value;
      auto* configMapKeyEntry = folly::get_ptr(*configMapEntry, entryKey);
      if (!configMapKeyEntry) {
        if (!isDeadConfigKey(sectionName, entryKey)) {
          XLOGF(
              WARNING,
              "Ignoring unknown key in eden config: {}, {}:{}",
              path_,
              sectionName,
              entryKey);
        }
        continue;
      }
      if (auto valueStr = valueAsString(*entryValue)) {
        auto rslt = (*configMapKeyEntry)
                        ->setStringValue(*valueStr, substitutions, sourceType_);
        if (rslt.hasError()) {
          XLOGF(
              WARNING,
              "Ignoring invalid config entry {} {}:{}, value '{}' {}",
              path_,
              sectionName,
              entryKey,
              *valueStr,
              rslt.error());
        }
      } else {
        XLOGF(
            WARNING,
            "Ignoring invalid config entry {} {}:{}, is not a string, boolean, or array",
            path_,
            sectionName,
            entryKey);
      }
    }
  }
}

} // namespace facebook::eden

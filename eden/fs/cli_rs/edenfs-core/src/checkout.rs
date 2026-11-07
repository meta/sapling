/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;
use std::fmt::Write as _;
#[cfg(unix)]
use std::fs;
use std::fs::File;
use std::io::BufReader;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Write;
use std::io::prelude::*;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::vec;

use anyhow::Context;
use anyhow::anyhow;
use atomicfile::atomic_write;
use byteorder::BigEndian;
use byteorder::ReadBytesExt;
use edenfs_config::EdenFsConfig;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use edenfs_error::ResultExt;
use edenfs_utils::path_from_bytes;
use edenfs_utils::varint::decode_varint;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::ser::SerializeMap;
use strum::EnumString;
use strum::VariantNames;
use thrift_types::edenfs::MountInfo;
use thrift_types::edenfs::MountState;
use toml::value::Value;
use uuid::Uuid;

use crate::instance::EdenFsInstance;
use crate::redirect::REPO_SOURCE;
use crate::redirect::Redirection;
use crate::redirect::RedirectionType;
use crate::redirect::deserialize_redirections;

// files in the client directory (aka data_dir aka state_dir)
const MOUNT_CONFIG: &str = "config.toml";
const SNAPSHOT: &str = "SNAPSHOT";

// Magical snapshot strings
const SNAPSHOT_MAGIC_1: &[u8] = b"eden\x00\x00\x00\x01";
const SNAPSHOT_MAGIC_2: &[u8] = b"eden\x00\x00\x00\x02";
const SNAPSHOT_MAGIC_3: &[u8] = b"eden\x00\x00\x00\x03";
const SNAPSHOT_MAGIC_4: &[u8] = b"eden\x00\x00\x00\x04";

// List of supported repository types. This should stay in sync with the list
// in the Python CLI at fs/cli_rs/edenfs-client/src/checkout.rs and the list in
// the Daemon's CheckoutConfig at fs/config/CheckoutConfig.h.
#[derive(Deserialize, Serialize, Debug, PartialEq, VariantNames, EnumString)]
#[serde(rename_all = "lowercase")]
enum RepositoryType {
    // The daemon no longer mounts "git" or "recas" checkouts; the CLI keeps
    // recognising the types so existing ones can still be listed and removed.
    #[strum(serialize = "git")]
    Git,
    #[strum(serialize = "recas")]
    Recas,
    #[strum(serialize = "hg")]
    Hg,
    #[strum(serialize = "filteredhg")]
    FilteredHg,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, VariantNames, EnumString)]
#[serde(rename_all = "lowercase")]
enum MountProtocol {
    #[strum(serialize = "fuse")]
    Fuse,
    #[strum(serialize = "nfs")]
    Nfs,
    #[strum(serialize = "prjfs")]
    Prjfs,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, VariantNames, EnumString)]
#[serde(rename_all = "lowercase")]
enum InodeCatalogType {
    #[strum(serialize = "legacy")]
    Legacy,
    #[strum(serialize = "inmemory")]
    InMemory,
    #[strum(serialize = "legacyephemeral")]
    LegacyEphemeral,
}

#[derive(Debug)]
pub enum PrefetchProfilesResult {
    Prefetched,
    Skipped(String),
}

#[derive(Deserialize, Serialize, Debug)]
struct Repository {
    path: PathBuf,

    #[serde(rename = "type", deserialize_with = "deserialize_repo_type")]
    repo_type: RepositoryType,

    #[serde(default = "default_guid")]
    guid: Uuid,

    #[serde(
        deserialize_with = "deserialize_protocol",
        default = "default_protocol"
    )]
    protocol: MountProtocol,

    #[serde(rename = "case-sensitive", default = "default_case_sensitive")]
    case_sensitive: bool,

    #[serde(rename = "require-utf8-path", default = "default_require_utf8_path")]
    require_utf8_path: bool,

    #[serde(rename = "use-write-back-cache", default)]
    use_write_back_cache: bool,

    #[serde(
        rename = "inode-catalog-type",
        default = "default_inode_catalog_type",
        deserialize_with = "deserialize_inode_catalog_type"
    )]
    inode_catalog_type: Option<InodeCatalogType>,

    #[serde(rename = "off-mount-repo-dir", default)]
    off_mount_repo_dir: bool,
}

fn deserialize_repo_type<'de, D>(deserializer: D) -> Result<RepositoryType, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;

    match RepositoryType::from_str(&s) {
        Ok(t) => Ok(t),
        Err(_) => Err(serde::de::Error::custom(format!(
            "Unsupported value: `{}`. Must be one of: {}",
            s,
            RepositoryType::VARIANTS.join(", ")
        ))),
    }
}

fn default_inode_catalog_type() -> Option<InodeCatalogType> {
    None
}

fn deserialize_inode_catalog_type<'de, D>(
    deserializer: D,
) -> Result<Option<InodeCatalogType>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = Option::<String>::deserialize(deserializer)?;

    match s {
        None => Ok(None),
        Some(s) => match InodeCatalogType::from_str(&s) {
            Ok(t) => Ok(Some(t)),
            Err(_) => Err(serde::de::Error::custom(format!(
                "Unsupported value: `{}`. Must be one of: {}",
                s,
                InodeCatalogType::VARIANTS.join(", ")
            ))),
        },
    }
}

fn deserialize_protocol<'de, D>(deserializer: D) -> Result<MountProtocol, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;

    match MountProtocol::from_str(&s) {
        Ok(m) => Ok(m),
        Err(_) => Err(serde::de::Error::custom(format!(
            "Unsupported value: `{}`. Must be one of: {}",
            s,
            MountProtocol::VARIANTS.join(", ")
        ))),
    }
}

fn default_protocol() -> MountProtocol {
    if cfg!(windows) {
        MountProtocol::Prjfs
    } else {
        MountProtocol::Fuse
    }
}

fn default_guid() -> Uuid {
    Uuid::new_v4()
}

fn default_case_sensitive() -> bool {
    cfg!(target_os = "linux")
}

fn default_require_utf8_path() -> bool {
    // Existing repositories may have non-utf8 files, thus allow them by default
    true
}

#[derive(Deserialize, Serialize, Debug)]
struct PrefetchProfiles {
    #[serde(deserialize_with = "deserialize_active", default)]
    pub active: Vec<String>,
}

impl PrefetchProfiles {
    fn push(&mut self, profile: &str) {
        self.active.push(profile.into());
    }
}

fn deserialize_active<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let unvalidated_arr: Vec<Value> = Vec::deserialize(deserializer)?;
    let mut arr = Vec::new();
    for val in unvalidated_arr {
        if let Some(s) = val.as_str() {
            arr.push(s.to_string());
        } else {
            return Err(serde::de::Error::custom(format!(
                "Unsupported [profiles] active type {val}. Must be string."
            )));
        }
    }

    Ok(arr)
}

#[derive(Deserialize, Serialize, Debug)]
pub struct CheckoutConfig {
    repository: Repository,

    #[serde(
        deserialize_with = "deserialize_redirections",
        serialize_with = "serialize_path_map"
    )]
    redirections: BTreeMap<PathBuf, RedirectionType>,

    #[serde(
        rename = "redirection-targets",
        deserialize_with = "deserialize_redirection_targets",
        serialize_with = "serialize_path_map",
        default = "default_redirection_targets"
    )]
    redirection_targets: BTreeMap<PathBuf, PathBuf>,

    profiles: Option<PrefetchProfiles>,
}

// Initialize it to empty map to ensure backward compatibility
fn default_redirection_targets() -> BTreeMap<PathBuf, PathBuf> {
    BTreeMap::new()
}

fn serialize_path_map<V, S>(map: &BTreeMap<PathBuf, V>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    V: Serialize,
{
    let mut map_serializer = serializer.serialize_map(Some(map.len()))?;
    for (key, value) in map {
        let serialized_key = if cfg!(windows) {
            // On Windows, we need to escape backslashes in the path, since
            // TOML uses backslashes as an escape character.
            // temp_key is used to make sure that the path is not already escaped
            // from previous serialization, since we don't want to double-escape it.
            let temp_key = key.display().to_string().replace("\\\\", "\\");
            temp_key.replace('\\', "\\\\")
        } else {
            key.to_string_lossy().into_owned()
        };
        match map_serializer.serialize_entry(&serialized_key, value) {
            Ok(_) => continue,
            Err(e) => {
                return Err(serde::ser::Error::custom(format!(
                    "Unsupported redirection. Target must be string. Error: {e}"
                )));
            }
        }
    }
    map_serializer.end()
}

fn deserialize_redirection_targets<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<PathBuf, PathBuf>, D::Error>
where
    D: Deserializer<'de>,
{
    let unvalidated_map: BTreeMap<String, Value> = BTreeMap::deserialize(deserializer)?;
    let mut map = BTreeMap::new();
    for (key, value) in unvalidated_map {
        if let Some(s) = value.as_str() {
            map.insert(
                PathBuf::from(
                    // Convert path separator to backslash on Windows
                    if cfg!(windows) {
                        key.replace("\\\\", "\\").replace('/', "\\")
                    } else {
                        key
                    },
                ),
                PathBuf::from_str(s).map_err(serde::de::Error::custom)?,
            );
        } else {
            return Err(serde::de::Error::custom(format!(
                "Unsupported redirection target {value}. Must be string."
            )));
        }
    }

    Ok(map)
}

impl CheckoutConfig {
    /// Reads checkout config information from config.toml and
    /// returns an Err if it is not properly formatted or does not exist.
    pub fn parse_config(state_dir: &Path) -> Result<CheckoutConfig> {
        let config_path = state_dir.join(MOUNT_CONFIG);
        if !config_path.try_exists().from_err()? {
            return Err(EdenFsError::IOError(std::io::Error::new(
                ErrorKind::NotFound,
                format!("checkout config {} does not exist", config_path.display()),
            )));
        }

        let content = String::from_utf8(std::fs::read(config_path).from_err()?).from_err()?;
        let config: CheckoutConfig = toml::from_str(&content).from_err()?;
        Ok(config)
    }

    pub fn get_prefetch_profiles(&self) -> Result<&Vec<String>> {
        if let Some(profiles) = &self.profiles {
            Ok(&profiles.active)
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Cannot get active prefetch profiles for {}",
                self.repository.path.display()
            )))
        }
    }

    pub fn contains_prefetch_profile(&self, profile: &str) -> bool {
        if let Some(profiles) = &self.profiles {
            profiles.active.iter().any(|x| x == profile)
        } else {
            false
        }
    }

    pub fn remove_prefetch_profile(&mut self, profile: &str, config_dir: PathBuf) -> Result<()> {
        if let Some(profiles) = &mut self.profiles {
            if profiles.active.iter().any(|x| x == profile) {
                profiles.active.retain(|x| *x != *profile);
                self.save_config(config_dir.clone()).with_context(|| {
                    anyhow!(
                        "failed to save config in the given config_dir: {}",
                        config_dir.display()
                    )
                })?;
            }
        };
        Ok(())
    }

    pub fn update_redirections(
        &mut self,
        config_dir: &Path,
        redirs: &BTreeMap<PathBuf, Redirection>,
    ) -> Result<()> {
        self.redirections.clear();
        self.redirection_targets.clear();
        for redir in redirs.values() {
            if redir.source != REPO_SOURCE {
                self.redirections
                    .insert(redir.repo_path(), redir.redir_type);
                self.redirection_targets.insert(
                    redir.repo_path(),
                    redir.target.clone().unwrap_or_else(|| PathBuf::from("")),
                );
            }
        }
        self.save_config(config_dir.into())?;
        Ok(())
    }

    pub fn remove_redirection_target(&mut self, config_dir: &Path, repo_path: &Path) -> Result<()> {
        self.redirection_targets.remove_entry(repo_path);
        self.save_config(config_dir.into())?;
        Ok(())
    }

    pub fn remove_redirection_targets(&mut self, config_dir: &Path) -> Result<()> {
        self.redirection_targets.clear();
        self.save_config(config_dir.into())?;
        Ok(())
    }

    /// Store information about the mount in the config.toml file.
    pub fn save_config(&mut self, state_dir: PathBuf) -> Result<()> {
        let toml_out = &toml::to_string(&self).with_context(|| {
            anyhow!(
                "could not toml-ize checkout config for repo '{}'",
                self.repository.path.display()
            )
        })?;
        let config_path = state_dir.join(MOUNT_CONFIG);
        // set default permissions to 0o644 (420 in decimal)
        #[cfg(windows)]
        let perm = 0o664;

        #[cfg(not(windows))]
        let perm = std::fs::metadata(&config_path)
            .map(|meta| meta.permissions().mode())
            .unwrap_or(0o664);

        atomic_write(config_path.as_path(), perm, true, |f| {
            f.write_all(toml_out.as_bytes())?;
            Ok(())
        })
        .from_err()?;
        Ok(())
    }

    /// Add a profile to the config (read the config file and write it back
    /// with profile added).
    pub fn activate_profile(&mut self, profile: &str, config_dir: PathBuf) -> Result<()> {
        if let Some(profiles) = &mut self.profiles {
            if profiles.active.iter().any(|x| x == profile) {
                // The profile is already activated so we don't need to update the profile list
                eprintln!("{profile} is already an active prefetch profile");
                return Ok(());
            }

            profiles.push(profile);
            self.save_config(config_dir.clone()).with_context(|| {
                anyhow!(
                    "failed to save config in the given config_dir: {}",
                    config_dir.display()
                )
            })?;
            Ok(())
        } else {
            Err(EdenFsError::Other(anyhow!(
                "failed to activate prefetch profile '{profile}'; could not find active profile list"
            )))
        }
    }

    /// Remove a profile to the config (read the config file and write it back
    /// with profile added).
    pub fn deactivate_profile(&mut self, profile: &str, config_dir: PathBuf) -> Result<()> {
        if let Some(profiles) = &mut self.profiles {
            if !profiles.active.iter().any(|x| x == profile) {
                return Err(EdenFsError::Other(anyhow!(
                    "Profile {profile} was not deactivated since it wasn't active."
                )));
            }
            profiles.active.retain(|x| *x != *profile);
            self.save_config(config_dir.clone()).with_context(|| {
                anyhow!(
                    "failed to save config in the given config_dir: {}",
                    config_dir.display()
                )
            })?;
        };
        Ok(())
    }
}

pub struct SnapshotState {
    pub working_copy_parent: String,
    pub last_checkout_hash: String,
    pub parent_filter_id: Option<Vec<u8>>,
    pub last_filter_id: Option<Vec<u8>>,
}

impl SnapshotState {
    fn new(
        working_copy_parent: String,
        last_checkout_hash: String,
        parent_filter_id: Option<Vec<u8>>,
        last_filter_id: Option<Vec<u8>>,
    ) -> Self {
        Self {
            working_copy_parent,
            last_checkout_hash,
            parent_filter_id,
            last_filter_id,
        }
    }
}

/// Represents an edenfs checkout with mount information as well as information from configuration
#[derive(Serialize)]
pub struct EdenFsCheckout {
    /// E.g., /data/sandcastle/boxes/fbsource
    #[serde(skip)]
    path: PathBuf,
    /// E.g., /home/unixname/local/.eden/clients/fbsource
    data_dir: PathBuf,
    /// This is None when it's just configured but not actively mounted in eden
    #[serde(serialize_with = "serialize_state")]
    state: Option<MountState>,
    /// If this is false, that means this model is only populated with mount info from edenfs
    /// As opposed to being populated with information from the configuration & live mount info.
    configured: bool,
    backing_repo: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fs_channel_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fuse_transport: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    visible_in_daemon_namespace: Option<bool>,
    #[serde(skip)]
    pub(crate) redirections: Option<BTreeMap<PathBuf, RedirectionType>>,
    #[serde(skip)]
    pub(crate) redirection_targets: Option<BTreeMap<PathBuf, PathBuf>>,
}

impl EdenFsCheckout {
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    pub fn display(&self, verbose: bool) -> String {
        let suffix = if self.configured {
            ""
        } else {
            " (unconfigured)"
        };

        let state_str = match self.state {
            Some(state) => {
                if state == MountState::RUNNING {
                    String::new()
                } else {
                    format!(" ({state})")
                }
            }
            None => " (not mounted)".to_string(),
        };

        let transport_str = if verbose {
            match (&self.fs_channel_type, &self.fuse_transport) {
                (Some(channel), Some(transport)) => format!(" ({channel}, {transport})"),
                (Some(channel), None) => format!(" ({channel})"),
                _ => String::new(),
            }
        } else {
            String::new()
        };

        let visibility_str = if self.visible_in_daemon_namespace == Some(false) {
            " (not visible in daemon namespace)"
        } else {
            ""
        };

        format!(
            "{}{}{}{}{}",
            self.path.display(),
            state_str,
            visibility_str,
            transport_str,
            suffix
        )
    }

    pub fn fsck_dir(&self) -> PathBuf {
        self.data_dir.join("fsck")
    }

    fn encode_hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            write!(&mut s, "{b:02x}").unwrap();
        }
        s
    }

    /// Determines the hash and filter id for a given Snapshot component.
    pub fn parse_snapshot_component(
        &self,
        component_buf: &Vec<u8>,
    ) -> Result<(String, Option<Vec<u8>>)> {
        let checkout_config = CheckoutConfig::parse_config(&self.data_dir)?;

        if checkout_config.repository.repo_type == RepositoryType::FilteredHg {
            // FilteredRootIds are in the form: <VarInt><RootId><FilterId>. We first parse out the
            // VarInt to determine where the RootId ends.
            let cursor = std::io::Cursor::new(component_buf);
            let (component_hash_len, varint_len) = decode_varint(&mut BufReader::new(cursor))
                .context("Could not decode varint in Snapshot file")?;
            let filter_offset = varint_len + (component_hash_len as usize);

            // We can then parse out the RootId and convert it into a string. The FilterId is not
            // guaranteed to be valid UTF-8, so we return the raw bytes.
            let decoded_hash = std::str::from_utf8(&component_buf[varint_len..filter_offset])
                .from_err()?
                .to_string();
            Ok((
                decoded_hash,
                Some(Vec::from(&component_buf[filter_offset..])),
            ))
        } else {
            // The entire buffer corresponds to the hash. There is no filter id present.
            let decoded_hash = std::str::from_utf8(component_buf).from_err()?.to_string();
            Ok((decoded_hash, None))
        }
    }

    /// Returns a SnapshotState representing EdenFS working copy parent as well as the last checked
    /// out revision.
    pub fn get_snapshot(&self) -> Result<SnapshotState> {
        let snapshot_path = self.data_dir.join(SNAPSHOT);
        let mut f = File::open(snapshot_path).from_err()?;
        let mut header = [0u8; 8];
        f.read(&mut header).from_err()?;

        if header == SNAPSHOT_MAGIC_1 {
            let mut snapshot = [0u8; 20];
            f.read(&mut snapshot).from_err()?;
            let decoded = EdenFsCheckout::encode_hex(&snapshot);
            Ok(SnapshotState::new(decoded.clone(), decoded, None, None))
        } else if header == SNAPSHOT_MAGIC_2 {
            // The first byte of the snapshot file is the length of the working copy parent.
            let body_length = f.read_u32::<BigEndian>().from_err()?;
            let mut buf = vec![0u8; body_length as usize];
            f.read_exact(&mut buf).from_err()?;

            // We must parse out the working copy parent hash. For Filtered repos, we also have to
            // parse out the active filter id.
            let (decoded_hash, filter_bytes) = self
                .parse_snapshot_component(&buf)
                .context("Could not parse snapshot component")?;
            Ok(SnapshotState::new(
                decoded_hash.clone(),
                decoded_hash,
                filter_bytes.clone(),
                filter_bytes,
            ))
        } else if header == SNAPSHOT_MAGIC_3 {
            let _pid = f.read_u32::<BigEndian>().from_err()?;

            let from_length = f.read_u32::<BigEndian>().from_err()?;
            let mut from_buf = vec![0u8; from_length as usize];
            f.read_exact(&mut from_buf).from_err()?;

            let to_length = f.read_u32::<BigEndian>().from_err()?;
            let mut to_buf = vec![0u8; to_length as usize];
            f.read_exact(&mut to_buf).from_err()?;

            let (from_hash, from_filter) = self
                .parse_snapshot_component(&from_buf)
                .context("Could not parse snapshot component")?;

            let (to_hash, to_filter) = self
                .parse_snapshot_component(&to_buf)
                .context("Could not parse snapshot component")?;

            // TODO(xavierd): return a proper object that the caller could use.
            Err(EdenFsError::Other(anyhow!(
                "A checkout operation is ongoing from {from_hash} (filter: {from_filter:?}) to {to_hash} (filter: {to_filter:?})",
            )))
        } else if header == SNAPSHOT_MAGIC_4 {
            let working_copy_parent_length = f.read_u32::<BigEndian>().from_err()?;
            let mut working_copy_parent_buf = vec![0u8; working_copy_parent_length as usize];
            f.read_exact(&mut working_copy_parent_buf).from_err()?;

            let checked_out_length = f.read_u32::<BigEndian>().from_err()?;
            let mut checked_out_buf = vec![0u8; checked_out_length as usize];
            f.read_exact(&mut checked_out_buf).from_err()?;

            let (parent_hash, parent_filter) = self
                .parse_snapshot_component(&working_copy_parent_buf)
                .context("Could not parse snapshot component")?;

            let (checked_out_hash, checked_out_filter) = self
                .parse_snapshot_component(&checked_out_buf)
                .context("Could not parse snapshot component")?;

            Ok(SnapshotState::new(
                parent_hash,
                checked_out_hash,
                parent_filter,
                checked_out_filter,
            ))
        } else {
            Err(EdenFsError::Other(anyhow!(
                "SNAPSHOT file has invalid header"
            )))
        }
    }

    pub fn backing_repo(&self) -> Option<PathBuf> {
        self.backing_repo.clone()
    }

    fn from_mount_info(path: PathBuf, thrift_mount: MountInfo) -> Result<EdenFsCheckout> {
        Ok(EdenFsCheckout {
            path,
            data_dir: path_from_bytes(&thrift_mount.edenClientPath)?,
            state: Some(thrift_mount.state),
            configured: false,
            backing_repo: match thrift_mount.backingRepoPath {
                Some(path_string) => Some(path_from_bytes(&path_string)?),
                None => None,
            },
            fs_channel_type: thrift_mount.fsChannelType,
            fuse_transport: thrift_mount.fuseTransport,
            visible_in_daemon_namespace: thrift_mount.visibleInDaemonNamespace,
            redirections: None,
            redirection_targets: None,
        })
    }

    /// A configured checkout at `path` whose client state is in `data_dir`.
    pub fn from_config(path: PathBuf, data_dir: PathBuf, config: CheckoutConfig) -> EdenFsCheckout {
        EdenFsCheckout {
            path,
            data_dir,
            state: None,
            configured: true,
            backing_repo: Some(config.repository.path.clone()),
            fs_channel_type: None,
            fuse_transport: None,
            visible_in_daemon_namespace: None,
            redirections: Some(config.redirections),
            redirection_targets: Some(config.redirection_targets),
        }
    }

    fn update_with_config(&mut self, config: CheckoutConfig) {
        if self.backing_repo.is_none() {
            self.backing_repo = Some(config.repository.path.clone());
        }
        self.configured = true;
    }

    pub fn get_contents_for_profile(
        &self,
        profile: &String,
        silent: bool,
    ) -> Result<HashSet<String>> {
        const RELATIVE_PROFILES_LOCATION: &str = "xplat/scm/prefetch_profiles/profiles";
        let profile_path = self.path.join(RELATIVE_PROFILES_LOCATION).join(profile);

        if !profile_path.exists() {
            if !silent {
                eprintln!(
                    "Profile '{}' not found for checkout {}.",
                    profile,
                    self.path().display()
                );
            }
            return Ok(HashSet::new());
        }

        let file = File::open(&profile_path).with_context(|| {
            anyhow!("Sparse profile '{}' does not exist", profile_path.display())
        })?;
        Ok(BufReader::new(file)
            .lines()
            .collect::<std::io::Result<HashSet<_>>>()
            .with_context(|| {
                anyhow!(
                    "Cannot read contents for prefetch profile '{}'",
                    profile_path.display()
                )
            })?)
    }

    pub fn should_prefetch_profiles(config: &EdenFsConfig) -> bool {
        config.prefetch_profiles.prefetching_enabled.unwrap_or(true)
    }
}

fn serialize_state<S>(field: &Option<MountState>, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    s.serialize_str(&match *field {
        Some(state) => {
            format!("{state}")
        }
        None => "NOT_RUNNING".to_string(),
    })
}

impl fmt::Display for EdenFsCheckout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display(false))
    }
}

pub fn has_missing_checkout_state(error: &EdenFsError) -> bool {
    match error {
        EdenFsError::IOError(err) => err.kind() == ErrorKind::NotFound,
        EdenFsError::Other(err) => err
            .downcast_ref::<std::io::Error>()
            .is_some_and(|err| err.kind() == ErrorKind::NotFound),
        _ => false,
    }
}

fn load_configured_checkout_configs(
    configured_mounts: BTreeMap<PathBuf, String>,
    clients_dir: &Path,
) -> Result<Vec<(PathBuf, PathBuf, CheckoutConfig)>> {
    let mut configs = Vec::new();

    for (mount_path, client_name) in configured_mounts {
        let data_dir = clients_dir.join(&client_name);
        match CheckoutConfig::parse_config(&data_dir) {
            Ok(config) => configs.push((mount_path, data_dir, config)),
            Err(err) if has_missing_checkout_state(&err) => {
                // parse_config() normalizes missing config files into ENOENT, and a
                // concurrent deletion can still race with its read().
                tracing::warn!(
                    mount_path = %mount_path.display(),
                    client_name,
                    client_dir = %data_dir.display(),
                    "skipping configured mount with missing client state"
                );
            }
            Err(err) => return Err(err),
        }
    }

    Ok(configs)
}

/// Return information about all checkouts defined in EdenFS's configuration files
/// and all information about mounted checkouts from the eden daemon
pub async fn get_mounts(instance: &EdenFsInstance) -> Result<BTreeMap<PathBuf, EdenFsCheckout>> {
    // Get all configured checkout info (including not mounted / not active ones) from configs
    let configs = load_configured_checkout_configs(
        instance.get_configured_mounts_map()?,
        &instance.clients_dir(),
    )?;

    // Get active mounted checkouts info from eden daemon. Error from listMounts indicates that the
    // Eden daemon is not running or is unhealthy.
    let mounted_checkouts = instance
        .daemon()
        .list_mounts(Some(Duration::from_secs(3)))
        .await
        .ok();

    // Combine mount info from active mounts and mount info from config files
    let mut mount_points = BTreeMap::new();
    if let Some(mounts) = mounted_checkouts {
        for thrift_mount in mounts {
            let path = path_from_bytes(&thrift_mount.mountPoint)?;
            mount_points.insert(
                path.clone(),
                EdenFsCheckout::from_mount_info(path.clone(), thrift_mount)?,
            );
        }
    }

    for (path, data_dir, config) in configs {
        match mount_points.get_mut(&path) {
            Some(mount_info) => {
                mount_info.update_with_config(config);
            }
            None => {
                mount_points.insert(
                    path.clone(),
                    EdenFsCheckout::from_config(path.clone(), data_dir, config),
                );
            }
        };
    }

    Ok(mount_points)
}

#[cfg(windows)]
#[derive(Deserialize)]
struct WindowsEdenConfigInner {
    root: PathBuf,
    client: PathBuf,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct WindowsEdenConfig {
    #[serde(rename = "Config")]
    config: WindowsEdenConfigInner,
}

/// The client directory a mount's `.eden` metadata names: the `.eden/client` link on
/// Unix, the `client` key of `.eden/config` on Windows. `None` when the metadata is
/// absent or unreadable.
pub fn dot_eden_client_dir(path: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let config = std::fs::read_to_string(path.join(".eden").join("config")).ok()?;
        Some(
            toml::from_str::<WindowsEdenConfig>(&config)
                .ok()?
                .config
                .client,
        )
    }
    #[cfg(not(windows))]
    {
        fs::read_link(path.join(".eden").join("client")).ok()
    }
}

#[cfg(windows)]
fn get_checkout_root_state(path: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    let mut checkout_root = None;
    let mut checkout_state_dir = None;

    // On Windows, walk backwards through the path until you find the `.eden` folder
    let mut curr_dir = Some(path);
    while let Some(candidate_dir) = curr_dir {
        if candidate_dir.join(".eden").exists() {
            let config_file = candidate_dir.join(".eden").join("config");
            let config = std::fs::read_to_string(config_file).from_err()?;
            let config = toml::from_str::<WindowsEdenConfig>(&config).from_err()?;
            checkout_root = Some(config.config.root);
            checkout_state_dir = Some(config.config.client);
            break;
        } else {
            curr_dir = candidate_dir.parent();
        }
    }
    Ok((checkout_root, checkout_state_dir))
}

#[cfg(not(windows))]
fn get_checkout_root_state(path: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    // We will get an error if any of these symlinks do not exist
    let eden_socket_path = fs::read_link(path.join(".eden").join("socket"));
    if eden_socket_path.is_ok() {
        let checkout_root = fs::read_link(path.join(".eden").join("root")).ok();
        let checkout_state_dir = fs::read_link(path.join(".eden").join("client")).ok();
        Ok((checkout_root, checkout_state_dir))
    } else {
        Ok((None, None))
    }
}

/// If the path provided is an eden checkout, this returns an object representing that checkout.
/// Otherwise, if the path provided is not an eden checkout, this returns an EdenFsError.
pub fn find_checkout(instance: &EdenFsInstance, path: &Path) -> Result<EdenFsCheckout> {
    // Resolve symlinks and get absolute path
    let path = path.canonicalize().from_err()?;

    // Check if path is a mounted checkout
    let (checkout_root, checkout_state_dir) = match get_checkout_root_state(&path)? {
        (Some(checkout_root), Some(checkout_state_dir)) => (checkout_root, checkout_state_dir),
        _ => {
            // Find `checkout_path` that `path` is a sub path of.
            // Canonicalize config paths too so both sides match
            // (resolves UNC prefix and symlink differences).
            let all_checkouts = instance.get_configured_mounts_map()?;
            if let Some((checkout_path, checkout_name)) =
                all_checkouts.iter().find(|&(checkout_path, _)| {
                    checkout_path
                        .canonicalize()
                        .map(|cp| path.starts_with(&cp))
                        .unwrap_or_else(|_| path.starts_with(checkout_path))
                })
            {
                let checkout_state_dir = instance.config_directory(checkout_name);
                (checkout_path.into(), checkout_state_dir)
            } else {
                return Err(EdenFsError::Other(anyhow!(
                    "Checkout path {} is not handled by EdenFS",
                    path.display()
                )));
            }
        }
    };

    let config = CheckoutConfig::parse_config(&checkout_state_dir)?;
    Ok(EdenFsCheckout::from_config(
        checkout_root,
        checkout_state_dir,
        config,
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::ErrorKind;
    use std::io::Write;
    use std::path::Path;
    use std::path::PathBuf;

    use anyhow::Context;
    use anyhow::anyhow;
    use edenfs_error::EdenFsError;
    use edenfs_error::Result;
    use edenfs_error::ResultExt;
    use tempfile::TempDir;
    use tempfile::tempdir;
    use thrift_types::edenfs::MountState;

    use crate::checkout::CheckoutConfig;
    use crate::checkout::EdenFsCheckout;
    use crate::checkout::MOUNT_CONFIG;
    use crate::checkout::REPO_SOURCE;
    use crate::checkout::RedirectionType;
    use crate::checkout::load_configured_checkout_configs;
    use crate::redirect::Redirection;
    use crate::redirect::RedirectionState;

    // path and type are required... /tmp/ is probably the safest place to use as the path
    const DEFAULT_CHECKOUT_CONFIG: &str = r#"
    [repository]
    path = "/tmp/"
    type = "hg"

    [redirections]

    [profiles]"#;

    /// creates a checkout config and returns the tempdir in which the config is located
    fn create_test_checkout_config() -> Result<TempDir> {
        let temp_dir = tempdir().context("couldn't create temp dir")?;
        let mut config_file = File::create(temp_dir.path().join(MOUNT_CONFIG)).from_err()?;
        config_file
            .write_all(DEFAULT_CHECKOUT_CONFIG.as_bytes())
            .from_err()?;
        Ok(temp_dir)
    }

    fn create_clients_dir() -> Result<TempDir> {
        let temp_dir = tempdir().context("couldn't create temp dir")?;
        std::fs::create_dir_all(temp_dir.path().join("clients")).from_err()?;
        Ok(temp_dir)
    }

    /// Takes a (repo_path, redir_type) pair and checks if a checkout config contains that redirection
    fn checkout_config_contains_redirection(
        repo_path: &Path,
        redir_type: RedirectionType,
        config_dir: &Path,
    ) -> Result<()> {
        let config = CheckoutConfig::parse_config(config_dir)?;
        let config_redir_type = config.redirections.get(repo_path);
        match config_redir_type {
            Some(r_type) => {
                if *r_type != redir_type {
                    Err(EdenFsError::Other(anyhow!(
                        "Redirection type did not match the redirection type in the config"
                    )))
                } else {
                    Ok(())
                }
            }
            None => Err(EdenFsError::Other(anyhow!(
                "Did not find redirection in checkout config"
            ))),
        }
    }

    fn update_and_test_redirection(
        redir: Redirection,
        config_dir: &Path,
        should_be_inserted: bool,
    ) -> Result<()> {
        // parse config from test_path
        let mut config = CheckoutConfig::parse_config(config_dir)?;

        // create map of redirections we want to add to the checkout config
        let mut redir_map: BTreeMap<PathBuf, Redirection> = BTreeMap::new();

        // We need to track repo_path and redir type to confirm the redirection was added correctly
        let repo_path = redir.repo_path();
        let redir_type = redir.redir_type;

        // insert the redirection and update the checkout config
        redir_map.insert(repo_path.clone(), redir);
        config
            .update_redirections(config_dir, &redir_map)
            .expect("failed to update checkout config");

        // check if redirection was inserted (or not inserted) correctly
        if should_be_inserted {
            checkout_config_contains_redirection(&repo_path, redir_type, config_dir)
        } else {
            // In some cases, we don't want the checkout to be inserted (when source == REPO_SOURCE)
            // In that case, we should fail if we find the redirection in the checkout config
            match checkout_config_contains_redirection(&repo_path, redir_type, config_dir) {
                Ok(_) => Err(EdenFsError::Other(anyhow!(
                    "Redirection was present in config when it shouldn't have been present"
                ))),
                Err(_) => Ok(()),
            }
        }
    }

    #[test]
    fn test_mount_visibility_display_and_json() {
        let checkout = EdenFsCheckout {
            path: PathBuf::from("/tmp/mount"),
            data_dir: PathBuf::from("/tmp/client"),
            state: Some(MountState::RUNNING),
            configured: true,
            backing_repo: None,
            fs_channel_type: None,
            fuse_transport: None,
            visible_in_daemon_namespace: Some(false),
            redirections: None,
            redirection_targets: None,
        };

        assert_eq!(
            "/tmp/mount (not visible in daemon namespace)",
            checkout.display(false)
        );
        let value = serde_json::to_value(checkout).expect("failed to serialize checkout");
        assert_eq!(
            Some(&serde_json::Value::Bool(false)),
            value.get("visible_in_daemon_namespace")
        );
    }

    #[test]
    fn test_update_redirections() {
        let config_test_dir =
            create_test_checkout_config().expect("failed to create test checkout config");
        let config_dir = config_test_dir.path();

        // test inserting a sylink redirection (this should be inserted because source != REPO_SOURCE)
        let redir1 = Redirection {
            repo_path: PathBuf::from("test_path"),
            redir_type: RedirectionType::Symlink,
            target: None,
            source: "NotARepoSource".into(),
            state: RedirectionState::UnknownMount,
        };
        assert!(update_and_test_redirection(redir1, config_dir, true).is_ok());

        // test inserting a symlink redirection whose source == REPO_SOURCE
        let redir2 = Redirection {
            repo_path: PathBuf::from("test_path2"),
            redir_type: RedirectionType::Symlink,
            target: None,
            source: REPO_SOURCE.into(),
            state: RedirectionState::UnknownMount,
        };
        assert!(update_and_test_redirection(redir2, config_dir, false).is_ok());

        // test inserting a bind redirection whose source != REPO_SOURCE
        let redir3 = Redirection {
            repo_path: PathBuf::from("test_path3"),
            redir_type: RedirectionType::Bind,
            target: None,
            source: "NotARepoSource".into(),
            state: RedirectionState::UnknownMount,
        };
        assert!(update_and_test_redirection(redir3, config_dir, true).is_ok());

        // test inserting a bind redirection whose source == REPO_SOURCE
        let redir4 = Redirection {
            repo_path: PathBuf::from("test_path4"),
            redir_type: RedirectionType::Bind,
            target: None,
            source: REPO_SOURCE.into(),
            state: RedirectionState::UnknownMount,
        };
        assert!(update_and_test_redirection(redir4, config_dir, false).is_ok());
    }

    #[test]
    fn test_load_configured_checkout_configs_skips_missing_client_state() {
        let temp_dir = create_clients_dir().expect("failed to create clients dir");
        // Manually insert a missing client state
        let checkout_path = PathBuf::from("/tmp/missing-checkout");
        let configured_mounts =
            BTreeMap::from([(checkout_path.clone(), "missing-client".to_string())]);

        let configs =
            load_configured_checkout_configs(configured_mounts, &temp_dir.path().join("clients"))
                .expect("missing client state should be skipped");

        assert!(
            configs.is_empty(),
            "missing client state should not abort mount discovery"
        );
    }

    #[test]
    fn test_parse_config_missing_returns_not_found() {
        let temp_dir = create_clients_dir().expect("failed to create clients dir");
        let error = CheckoutConfig::parse_config(&temp_dir.path().join("clients").join("missing"))
            .expect_err("missing config should return not found");

        assert!(
            matches!(error, EdenFsError::IOError(ref err) if err.kind() == ErrorKind::NotFound),
            "missing config should be reported as ENOENT"
        );
    }

    #[test]
    fn test_load_configured_checkout_configs_preserves_invalid_config_errors() {
        let temp_dir = create_clients_dir().expect("failed to create clients dir");
        let clients_dir = temp_dir.path().join("clients");
        let client_dir = clients_dir.join("broken-client");
        std::fs::create_dir_all(&client_dir).expect("failed to create client dir");
        File::create(client_dir.join(MOUNT_CONFIG))
            .and_then(|mut file| file.write_all(b"not valid toml"))
            .expect("failed to write invalid config");

        let configured_mounts = BTreeMap::from([(
            PathBuf::from("/tmp/broken-checkout"),
            "broken-client".to_string(),
        )]);
        let error = load_configured_checkout_configs(configured_mounts, &clients_dir)
            .expect_err("invalid config should still fail");

        assert!(
            matches!(error, EdenFsError::Other(_)),
            "invalid config should be surfaced to the caller"
        );
    }
}

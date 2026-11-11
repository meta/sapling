/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#[cfg(target_os = "linux")]
use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::ffi::CString;
#[cfg(target_os = "linux")]
use std::mem::MaybeUninit;
#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::path::PathBuf;

use anyhow::anyhow;
use edenfs_error::EdenFsError;
use edenfs_error::Result;
use edenfs_error::ResultExt;
use edenfs_utils::trace_env;
use regex::Regex;
use subprocess::Exec;
use subprocess::Redirection;

#[derive(Debug, PartialEq)]
pub(crate) struct MountTableInfo {
    device: String,
    mount_point: PathBuf,
    vfstype: String,
}

impl MountTableInfo {
    pub(crate) fn mount_point(&self) -> PathBuf {
        self.mount_point.clone()
    }
}

fn parse_linux_mtab(mtab_string: String) -> Vec<MountTableInfo> {
    let mut mounts = Vec::new();
    for line in mtab_string.trim().lines() {
        let entries: Vec<&str> = line.split_ascii_whitespace().collect();
        if entries.len() != 6 {
            eprintln!(
                "mount table line `{}` has {} entries instead of 6",
                line,
                entries.len()
            );
        } else if let [device, mount_point, vfstype, _opts, _freq, _passno] = &entries[..] {
            mounts.push(MountTableInfo {
                device: String::from(*device),
                mount_point: PathBuf::from(
                    mount_point
                        .replace(r"\040", " ")
                        .replace(r"\011", "\t")
                        .replace(r"\012", "\n")
                        .replace(r"\134", "\\"),
                ),
                vfstype: String::from(*vfstype),
            });
        }
    }
    mounts
}

/// A lazy mount-table snapshot for one batch without intervening mount changes.
#[cfg(target_os = "linux")]
#[derive(Default)]
pub(crate) struct MountTableSnapshot {
    mount_points: Option<HashSet<PathBuf>>,
}

#[cfg(target_os = "linux")]
impl MountTableSnapshot {
    pub(crate) fn is_mount_point(&mut self, path: &Path) -> Result<bool> {
        self.is_mount_point_with(path, is_visible_mount_point(path)?, read_mount_table)
    }

    fn is_mount_point_with(
        &mut self,
        path: &Path,
        visible_mount: bool,
        read_table: impl FnOnce() -> Result<Vec<MountTableInfo>>,
    ) -> Result<bool> {
        if visible_mount {
            return Ok(true);
        }
        // A path lookup cannot see mounts covered by another mount. Keep the table
        // check when statx does not confirm a visible mount root, including on older kernels.
        let mount_points = match &self.mount_points {
            Some(mount_points) => mount_points,
            None => self.mount_points.insert(
                read_table()?
                    .into_iter()
                    .map(|mount| mount.mount_point)
                    .collect(),
            ),
        };
        Ok(mount_points.contains(path))
    }
}

/// Observe current mounts without reusing a planning snapshot.
#[cfg(target_os = "linux")]
pub(crate) fn is_mount_point(path: &Path) -> Result<bool> {
    if is_visible_mount_point(path)? {
        return Ok(true);
    }
    Ok(read_mount_table()?
        .iter()
        .any(|mount| mount.mount_point == path))
}

// Unlike comparing st_dev with the parent, this also detects same-filesystem binds.
#[cfg(target_os = "linux")]
fn is_visible_mount_point(path: &Path) -> Result<bool> {
    let c_path = CString::new(path.as_os_str().as_bytes()).from_err()?;
    let mut stat = MaybeUninit::<libc::statx>::uninit();
    // SAFETY: c_path is NUL-terminated and stat points to writable storage.
    let result = unsafe {
        libc::statx(
            libc::AT_FDCWD,
            c_path.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_BASIC_STATS,
            stat.as_mut_ptr(),
        )
    };
    if result == 0 {
        // SAFETY: a successful statx call initialized stat.
        let stat = unsafe { stat.assume_init() };
        if stat.stx_attributes_mask & stat.stx_attributes & libc::STATX_ATTR_MOUNT_ROOT as u64 != 0
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
#[test]
fn test_is_mount_point() {
    let temp = tempfile::tempdir().unwrap();
    assert!(is_mount_point(Path::new("/")).unwrap());
    assert!(!is_mount_point(temp.path()).unwrap());
    let link = temp.path().join("root-link");
    std::os::unix::fs::symlink("/", &link).unwrap();
    assert!(!is_mount_point(&link).unwrap());
    assert!(!is_mount_point(&temp.path().join("missing")).unwrap());
}

#[cfg(all(test, target_os = "linux"))]
mod snapshot_tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn planning_fallbacks_share_one_read_and_detect_covered_mounts() {
        let mut snapshot = MountTableSnapshot::default();
        let reads = Cell::new(0);
        // Neither covered path is visible to statx, but both remain in the table.
        for (path, expected) in [
            ("/checkout/buck-out", true),
            ("/checkout/missing", false),
            ("/checkout/space dir", true),
        ] {
            assert_eq!(
                snapshot
                    .is_mount_point_with(Path::new(path), false, || {
                        reads.set(reads.get() + 1);
                        Ok(parse_linux_mtab(
                            "/dev/root /checkout/buck-out ext4 rw 0 0\n\
                             /dev/root /checkout/space\\040dir ext4 rw 0 0"
                                .to_owned(),
                        ))
                    })
                    .unwrap(),
                expected,
                "unexpected mount-table lookup for {path}"
            );
        }
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn failed_read_propagates_and_can_be_retried() {
        let mut snapshot = MountTableSnapshot::default();
        let path = Path::new("/checkout/buck-out");
        let error = snapshot
            .is_mount_point_with(path, false, || {
                Err(EdenFsError::Other(anyhow!("mount table unavailable")))
            })
            .expect_err("a read error must not mean not mounted");
        assert!(error.to_string().contains("mount table unavailable"));
        assert!(snapshot.mount_points.is_none());
        assert!(
            snapshot
                .is_mount_point_with(path, false, || {
                    Ok(parse_linux_mtab(
                        "/dev/root /checkout/buck-out ext4 rw 0 0".to_owned(),
                    ))
                })
                .unwrap()
        );
    }
}

fn parse_macos_mtab(mtab_string: String) -> Vec<MountTableInfo> {
    let mut mounts = Vec::new();
    let mount_regex = Regex::new(r"^(\S+) on (.+) \(([^,]+),.*\)$")
        .expect("Expect each macos mtab to follow a specific format.");
    for line in mtab_string.split('\n') {
        for caps in mount_regex.captures_iter(line) {
            mounts.push(MountTableInfo {
                device: String::from(&caps[1]),
                mount_point: PathBuf::from(&caps[2]),
                vfstype: String::from(&caps[3]),
            });
        }
    }
    mounts
}

/// Returns the list of system mounts
pub(crate) fn read_mount_table() -> Result<Vec<MountTableInfo>> {
    if cfg!(target_os = "linux") {
        Ok(parse_linux_mtab(
            std::fs::read_to_string(PathBuf::from("/proc/self/mounts")).from_err()?,
        ))
    } else if cfg!(target_os = "macos") {
        // Specifying the path is important, as sudo may have munged the path
        // such that /sbin is not part of it any longer
        let output = Exec::cmd("/sbin/mount")
            .env_extend(trace_env("mount"))
            .stdout(Redirection::Pipe)
            .stderr(Redirection::Pipe)
            .capture()
            .from_err()?;

        if output.success() {
            Ok(parse_macos_mtab(output.stdout_str()))
        } else {
            Err(EdenFsError::Other(anyhow!(
                "Failed to execute /sbin/mount, stderr: {}",
                output.stderr_str()
            )))
        }
    } else {
        Ok(Vec::new())
    }
}

#[test]
fn test_parse_linux_mtab() {
    let contents = "
homedir.eden.com:/home109/chadaustin/public_html /mnt/public/chadaustin nfs rw,context=user_u:object_r:user_home_dir_t,relatime,vers=3,rsize=65536,wsize=65536,namlen=255,soft,nosharecache,proto=tcp6,timeo=100,retrans=2,sec=krb5i,mountaddr=2401:db00:fffe:1007:face:0000:0:4007,mountvers=3,mountport=635,mountproto=udp6,local_lock=none,addr=2401:db00:fffe:1007:0000:b00c:0:4007 0 0
squashfuse_ll /mnt/xarfuse/uid-0/2c071047-ns-4026531840 fuse.squashfuse_ll rw,nosuid,nodev,relatime,user_id=0,group_id=0 0 0
bogus line here
edenfs: /tmp/eden_test.4rec6drf/mounts/main fuse rw,nosuid,relatime,user_id=138655,group_id=100,default_permissions,allow_other 0 0
".to_string();
    let mount_infos = parse_linux_mtab(contents);
    assert_eq!(3, mount_infos.len());
    assert_eq!("edenfs:", mount_infos[2].device);
    assert_eq!(
        PathBuf::from("/tmp/eden_test.4rec6drf/mounts/main"),
        mount_infos[2].mount_point
    );
    assert_eq!("fuse", mount_infos[2].vfstype);
}

#[test]
fn linux_mount_paths_decode_escapes_once() {
    let mounts = parse_linux_mtab(
        r"/dev/root /checkout\040space/buck\134040out\011tab\012line ext4 rw 0 0".to_owned(),
    );
    assert_eq!(mounts.len(), 1);
    assert_eq!(
        mounts[0].mount_point,
        PathBuf::from("/checkout space/buck\\040out\ttab\nline")
    );
}

#[test]
fn test_parse_mtab_macos() {
    let contents = "
/dev/disk1s1 on / (apfs, local, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk1s4 on /private/var/vm (apfs, local, noexec, journaled, noatime, nobrowse)
map -hosts on /net (autofs, nosuid, automounted, nobrowse)
map auto_home on /home (autofs, automounted, nobrowse)
map -fstab on /Network/Servers (autofs, automounted, nobrowse)
"
    .to_string();

    let expected = [
        MountTableInfo {
            device: "/dev/disk1s1".to_string(),
            mount_point: PathBuf::from("/"),
            vfstype: "apfs".to_string(),
        },
        MountTableInfo {
            device: "devfs".to_string(),
            mount_point: PathBuf::from("/dev"),
            vfstype: "devfs".to_string(),
        },
        MountTableInfo {
            device: "/dev/disk1s4".to_string(),
            mount_point: PathBuf::from("/private/var/vm"),
            vfstype: "apfs".to_string(),
        },
    ];
    let actual = parse_macos_mtab(contents);

    assert_eq!(expected.len(), actual.len());
    assert!(expected.iter().zip(&actual).all(|(a, b)| *a == *b));
}

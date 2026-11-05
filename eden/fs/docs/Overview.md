# EdenFS Overview

EdenFS is a virtual filesystem designed for efficiently serving large source
control repositories.

In particular, EdenFS is targeted at massive
[monorepos](https://en.wikipedia.org/wiki/Monorepo), where a single repository
may contain numerous projects, potentially spanning many millions of files in
total. In most situations individual developers may only need to interact with a
fraction of the files in the repository when working on their specific projects.
EdenFS speeds up workflows in this case by lazily fetching file data, so that it
only needs to fetch file information for portions of the repository that are
actually used.

EdenFS aims to speed up several different types of operations:

- Determining files modified from the current source control state. (e.g.,
  computing the output for `hg status` or `git status`)
- Switching the filesystem state from one commit to another. (e.g., performing
  an `hg checkout` or `git checkout` operation).
- Tracking and delivering notifications about modified files. EdenFS can deliver
  notifications of file changes events through Watchman, to allow downstream
  tools like build tools and IDEs to build functionality that depends on file
  notification events.

Additionally, EdenFS also provides several additional features like efficiently
returning file content hashes. This allows downstream build tools to retrieve
file hashes without actually needing to read and hash the file contents.

## Operating System Interface

EdenFS is supported on Linux, macOS, and Windows. The mechanism used to interact
with the filesystem layer is different across these three different platforms.

On Linux, EdenFS uses
[FUSE](https://en.wikipedia.org/wiki/Filesystem_in_Userspace) to provide
filesystem functionality.

On macOS, EdenFS uses [NFSv3](https://datatracker.ietf.org/doc/html/rfc1813).

On Windows, EdenFS uses Microsoft's
[Projected File System](https://docs.microsoft.com/en-us/windows/win32/projfs/projected-file-system).
This behaves fairly differently from FUSE and NFS, but EdenFS still shares most
of the same internal logic for tracking file state.

Parts of this design discussion focus primarily on the Linux and macOS
implementations. On Windows, the interface to the OS behaves a bit differently,
but internally EdenFS still tracks its state using the same inode structure that
is used on Linux and macOS.

# High-Level Design

The following documents describe the design of relatively high-level aspects of
EdenFS's behavior:

- [Process Overview](./Process_State.md)
- [Source Control Data Model](./Data_Model.md)
- [Inodes](./Inodes.md)
- [Glossary](./Glossary.md)

# Design Specifics

The following documents cover specific features and implementation details in
more depth:

- [Configuration](./Config.md)
- [Caching](./Caching.md)
- [Globbing](./Globbing.md)
- [Inode Lifetime Management](./InodeLifetime.md)
- [Inode Locking](./InodeLocks.md)
- [Inode Storage](./InodeStorage.md)
- [Online fsck](./OnlineFsck.md)
- [Path Handling](./Paths.md)
- [Rename Handling](./Rename.md)
- [Redirections](./Redirections.md)
- [Threading](./Threading.md)
- [Windows](./Windows.md)

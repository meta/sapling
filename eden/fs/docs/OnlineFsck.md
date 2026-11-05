# Online fsck

## Background

Historically, EdenFS on Linux and macOS has relied on offline fsck during
startup after a crash.
The startup fsck requires O(inodes) time, which can be very long for a
checkout with many inodes in the overlay.

## Goals

The main goal of online fsck is to remove this startup cost by moving checks
from startup to runtime. Naively skipping startup fsck could break internal
assumptions or produce an unusable filesystem, so online fsck also needs
to:

- Perform checks during object construction (deserializing on-disk state) to
  maintain internal assumptions.
- Avoid unusable filesystem state (best-effort). For example, if
  `open`, `stat`, and `unlink` on a path all return `EIO`, then it's bad. If
  `unlink` can remove the bad path, then it's better.

EdenFS is performance sensitive, so online fsck also needs to:

- Avoid undesirable overhead. For example, to detect duplicated inodes or names
  when deserializing a directory, one might use an extra hash table.
  But such an extra table adds hashing and allocation costs.

And, to help with maintainability:

- Limit logical complexity. For example, a nice UX could be automatically
  recovering bad files from source control. But downloading from source
  control and updating related metadata in a race-free way, with correct error
  handling, is complex. It's much simpler to return `EIO` for content access
  and let the user `rm` the bad file.

## Trade-off examples

Sometimes the goals conflict (e.g. to avoid bad states, one has to add more
checks, and the checks add unwanted overhead).

In those cases, consider how often an abnormal state can occur in practice and
the overhead and complexity of various choices, and pick a reasonable balance.

Here are some examples:

### Directory loading

When loading a large directory, we could:

1. (avoid bad state) build extra hash tables to check for duplicate inode
   numbers.
2. (more efficient) reuse the directory's name map, allow multiple names
   pointing to a same inode.

Option 1 is ~6% slower than 2. The `InodeMap` can handle the "one inode,
multiple paths" case, and it's more generic (not limited to one directory).
So we chose option 2 here.

### File recovery

When `cat notes.txt` cannot read the overlay contents, we could:

1. (more automatic) restore source control contents or create an empty file.
2. (automatic) delete the broken file automatically.
3. (manual) return `EIO` for content access, but allow `rm notes.txt`.

Both 1 and 2 are complex - multiple steps, each step requires their own error
handling or race condition consideration (e.g. what if the file is also being
renamed?). We chose option 3 here for simplicity.

## Existing behavior

The `EXPECTED_ONLINE_FSCK_ON` matrix in
`eden/integration/overlay_corruption_test.py` records behavior for corrupt
states. Its `rm` and `recreate` columns show whether each case's directory can
be removed and recreated after the preceding operations.

When making related changes, consider adding a test there to demonstrate the
change more clearly to reviewers.

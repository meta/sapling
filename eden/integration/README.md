> Note: `sl` is the new command name to replace `hg`

# Integration Test

Eden integration tests are python tests located under `fbsource/fbcode/eden/integration/`.

## Eden Sapling Integration Test

Tests for sl with Eden are located under fbsource/fbcode/eden/integration/hg.

e.g. `status_test.py` is testing sl status works correctly with Eden.

### Write a New Integration Test

If you are starting a whole new group of new tests for testing a Sapling command, then you want to implement a new test class based on top of EdenHgTestCase.

Otherwise, just identify the right testing file and testing class that matches your testing purpose and simply add a new test case for that.

```python
@hg_cached_status_test
class StatusTest(EdenHgTestCase):
```

The decorator @hg_cached_status_test is used so the same test can be replicated into variant setup.

**Initial Repo Setup**
You define the initial directory/file structure for your test by implementing this function

```python
    def populate_backing_repo(self, repo: hgrepo.HgRepository) -> None:
        raise NotImplementedError(
            "individual test classes must implement " "populate_backing_repo()"
        )
```

**Trigger Sapling Command**

You call `sl` command from the test by using this helper function

```python
    def hg(
        self,
        *args: str,
        encoding: str = "utf-8",
        input: Optional[str] = None,
        hgeditor: Optional[str] = None,
        cwd: Optional[str] = None,
        check: bool = True,
    ) -> str:
```

**Run an Integration Test**

See `AGENTS.md` → "Verification" for the `buck2 test` command.

### FUSE io_uring variants

The repository, Hg, filtered-Hg, cached-status, and custom FUSE decorators add
io_uring siblings on Linux. NFS variants are unchanged; plain `eden_test` classes
opt in with `run_io_uring=True`. Decorators accept `run_io_uring=False` to opt out.
Migration tests use transport-only siblings to preserve their repository setup.
Saved-snapshot verification and mount-based fsck cases also exercise both transports;
offline fsck cases and snapshot-generation commands are unchanged.
The mount-based io_uring variants are Linux-only; platform-independent harness
unit tests still run on macOS and Windows.

io_uring tests require an fbk 6.13 or 6.16 kernel. Other kernels skip these variants.
They enable `fuse:io-uring-pre-create-queues` so queue allocation failures can fall
back before INIT. A successful devfuse fallback skips the io_uring test with a
reason; it does not count as io_uring coverage. Startup errors, timeouts, and
unexpected transport values still fail. Baseline FUSE variants explicitly select
devfuse. A fallback on one mount cannot hide an invalid transport on another.
Takeover suites and GC cases that perform takeover currently skip io_uring because
in-flight request handoff can stall; remove those skips once validated on both
supported kernels. Bind-redirection GC still runs without takeover.

Successful clone, remount, restart, and takeover paths verify running FUSE mounts;
custom asynchronous lifecycle paths
should call `self.eden.assert_running_fuse_transports()` after mount readiness.

Run the same suite on each supported kernel; a run on one does not validate the other:

```bash
buck2 test '@fbcode//mode/opt' fbcode//eden/integration/... -- --regex IoUring
```

When adding io_uring support for a new kernel, update `require_io_uring_kernel()`
in [`lib/edenclient.py`](lib/edenclient.py), the kernel-gate tests in
[`testcase_test.py`](testcase_test.py), and the supported versions documented here.
Run the sweep above on that kernel and verify eligible io_uring variants execute
rather than skip.

### Debug an Integration Test

**DBG Level**

To tune DBG level for integration tests, just override this method for your testing class

```python
    def edenfs_logging_settings(self) -> Dict[str, str]:
        return {
            "eden.fs.inodes.TreeInode": "DBG5",
            "eden.fs.inodes.CheckoutAction": "DBG5",
            "eden.fs.inodes.CheckoutContext": "DBG5",
        }
```

### Test Framework Repo Objects

A **backing repo** is the on-disk Sapling/Hg repository that EdenFS reads data from (see `eden/fs/docs/Glossary.md` → "Backing Repository"). EdenFS only reads the object store — it ignores the backing repo's working copy.

The two test base classes use **different naming conventions** for the same concepts:

| Concept | `EdenRepoTest` | `EdenHgTestCase` | Use for |
|---------|---------------|------------------|---------|
| **Backing repo** | `self.repo` | `self.backing_repo` | Adding commits the mount will serve |
| **EdenFS mount** | `self.eden_repo` | `self.repo` | `hg update`, `hg status`, commands through EdenFS |
| **Mount path** (`str`) | `self.mount` | `self.mount` | `os.listdir`, `open()`, `os.stat` |

`EdenRepoTest`: override `populate_repo()`. `EdenHgTestCase`: override `populate_backing_repo()`.

> ⚠️ **`self.repo` means different things in each base class.** In `EdenRepoTest` it's the backing repo; in `EdenHgTestCase` it's the EdenFS mount.

### Common Pitfalls

- **Updating in the wrong repo:** `self.repo.hg("update", commit)` in an `EdenRepoTest` subclass updates the backing repo's working copy, which EdenFS ignores — use `self.eden_repo.hg("update", commit)` instead.
- **Bookmark behavior:** `eden clone` checks out the backing repo's active bookmark, not the working-copy parent. If `populate_repo` sets a bookmark, the mount starts there. Explicitly call `self.eden_repo.hg("update", target_commit)` to be safe.

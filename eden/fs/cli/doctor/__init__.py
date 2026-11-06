#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

# pyre-strict

import errno
import logging
import os
import shlex
import sys
from datetime import timedelta
from pathlib import Path
from textwrap import dedent
from typing import Dict, Iterable, List, Optional, Set

from eden.fs.cli import (
    config as config_mod,
    daemon,
    filesystem,
    mtab,
    prjfs,
    proc_utils as proc_utils_mod,
    ui,
    util as util_mod,
    version,
)
from eden.fs.cli.config import CLIENTS_DIR, CONFIG_JSON, EdenInstance
from eden.fs.cli.doctor.util import (
    CheckoutInfo,
    format_approx_duration,
    get_checkouts_info,
    get_dependent_repos,
    hg_doctor_in_backing_repo,
)
from eden.fs.service.eden.thrift_types import MountState
from fb303_core.thrift_types import fb303_status
from filelock import FileLock, Timeout

from . import (
    check_filesystems,
    check_hg,
    check_network,
    check_os,
    check_recent_writes,
    check_redirections,
    check_rogue_edenfs,
    check_stale_mounts,
    check_watchman,
)
from .problem import (
    DryRunFixer,
    FixableProblem,
    Problem,
    ProblemFixer,
    ProblemSeverity,
    ProblemTracker,
)

try:
    from .facebook import check_facebook
except ImportError:
    from typing import Any

    def check_facebook(*_args: Any, **_kwargs: Any) -> None:
        pass


try:
    from eden.fs.cli.facebook.hostcaps import is_asic
except ImportError:

    def is_asic() -> bool:
        return False


try:
    from .facebook.check_vscode_extensions import VSCodeExtensionsChecker
except ImportError:
    from typing import Any

    class VSCodeExtensionsChecker:
        def check_problematic_vscode_extensions(
            self, *_args: Any, **_kwargs: Any
        ) -> None:
            pass


try:
    from .facebook.internal_consts import (
        get_doctor_link,
        get_local_commit_recovery_link,
    )
except ImportError:

    def get_doctor_link() -> str:
        return ""

    def get_local_commit_recovery_link() -> str:
        return ""


try:
    from .facebook.internal_error_messages import get_reclone_advice_link
except ImportError:

    def get_reclone_advice_link() -> str:
        return ""


# working_directory_was_stale may be set to True by the CLI main module
# if the original working directory referred to a stale eden mount point.
working_directory_was_stale = False
log: logging.Logger = logging.getLogger(__name__)


def get_reclone_msg(checkout_path: str) -> str:
    reclone_msg = """To recover, you will need to remove and reclone the repo.
Your local commits will be unaffected, but reclones will lose uncommitted work or shelves.
However, the local changes are manually recoverable before the reclone."""

    if get_local_commit_recovery_link():
        reclone_msg += f"\nIf you have local changes you would like to save before reclone, see {get_local_commit_recovery_link()}, or reachout to the EdenFS team."

    if get_doctor_link():
        reclone_msg += (
            "\nTo reclone the corrupted repo, run: `fbclone $REPO --reclone --eden`"
        )
        reclone_msg += f"\nFor additional info see the wiki at {get_doctor_link()}"
    else:
        reclone_msg += f"\nTo remove the corrupted repo, run: `eden rm {checkout_path}`"
    return reclone_msg


def cure_what_ails_you(
    instance: EdenInstance,
    dry_run: bool,
    *,
    debug: bool = False,
    fast: bool = False,
    wait: bool = False,
    min_severity_to_report: ProblemSeverity = ProblemSeverity.ADVICE,
    mount_table: Optional[mtab.MountTable] = None,
    fs_util: Optional[filesystem.FsUtil] = None,
    proc_utils: Optional[proc_utils_mod.ProcUtils] = None,
    vscode_extensions_checker: Optional[VSCodeExtensionsChecker] = None,
    network_checker: Optional[check_network.NetworkChecker] = None,
    out: Optional[ui.Output] = None,
) -> int:
    return EdenDoctor(
        instance,
        dry_run,
        debug,
        fast,
        wait,
        min_severity_to_report,
        mount_table,
        fs_util,
        proc_utils,
        vscode_extensions_checker,
        network_checker,
        out,
    ).cure_what_ails_you()


class UnexpectedPrivHelperProblem(Problem):
    def __init__(self, ex: Exception) -> None:
        super().__init__("Unexpected error while checking PrivHelper", exception=ex)


class UnexpectedMountProblem(Problem):
    def __init__(self, mount: Path, ex: Exception) -> None:
        super().__init__(f"unexpected error while checking {mount}", exception=ex)


class UnknownElevationProblem(Problem):
    def __init__(self, pid: Optional[int], ex: Exception) -> None:
        super().__init__(
            description=f"Unable to determine elevation of process {pid}: {ex}",
        )


class RunningElevatedProblem(Problem):
    def __init__(self, pid: Optional[int]) -> None:
        super().__init__(
            description=f"EdenFS is running as elevated process {pid}, which isn't supported",
            remediation="Run `edenfsctl restart` from a non-elevated command prompt",
        )


class EdenDoctorChecker:
    """EdenDoctorChecker is a base class for EdenDoctor, and only supports
    running checks, without reporting or fixing problems.
    """

    instance: EdenInstance
    mount_table: mtab.MountTable
    fs_util: filesystem.FsUtil
    proc_utils: proc_utils_mod.ProcUtils
    vscode_extensions_checker: VSCodeExtensionsChecker
    network_checker: check_network.NetworkChecker
    tracker: ProblemTracker
    out: ui.Output
    # Setting run_system_wide_checks to False causes EdenDoctor to skip checks that
    # try to detect system-wide problems (e.g., stale mount points, old OS/kernel
    # version, low disk space, etc.).  It is often desirable to disable this during
    # integration tests, since tests normally only want to report issues with their
    # specific EdenFS instance under test.
    run_system_wide_checks: bool = True

    def __init__(
        self,
        instance: EdenInstance,
        tracker: ProblemTracker,
        dry_run: bool,
        debug: bool,
        fast: bool,
        mount_table: Optional[mtab.MountTable] = None,
        fs_util: Optional[filesystem.FsUtil] = None,
        proc_utils: Optional[proc_utils_mod.ProcUtils] = None,
        vscode_extensions_checker: Optional[VSCodeExtensionsChecker] = None,
        network_checker: Optional[check_network.NetworkChecker] = None,
        out: Optional[ui.Output] = None,
    ) -> None:
        self.instance = instance
        self.tracker = tracker
        self.dry_run = dry_run
        self.debug = debug
        self.fast = fast
        self.mount_table = mount_table if mount_table is not None else mtab.new()
        self.fs_util = fs_util if fs_util is not None else filesystem.new()
        self.proc_utils = proc_utils if proc_utils is not None else proc_utils_mod.new()
        self.vscode_extensions_checker = (
            vscode_extensions_checker
            if vscode_extensions_checker is not None
            else VSCodeExtensionsChecker()
        )
        self.network_checker = (
            network_checker
            if network_checker is not None
            else check_network.NetworkChecker()
        )
        self.out = out if out is not None else ui.get_output()

    def run_checks(self) -> None:
        self.check_working_directory()

        if self.run_system_wide_checks:
            # check OS type, kernel version etc.
            check_os.run_operating_system_checks(self.tracker, self.instance, self.out)

            # check multiple edenfs running with some rogue stale PIDs
            check_rogue_edenfs.check_many_edenfs_are_running(
                self.tracker, self.proc_utils
            )

        status = self.instance.check_health()
        if status.status == fb303_status.ALIVE:
            self.run_normal_checks()
        elif status.status == fb303_status.STARTING:
            self.tracker.add_problem(EdenfsStarting())
        elif status.status == fb303_status.STOPPING:
            self.tracker.add_problem(EdenfsStopping())
        elif (
            status.status == fb303_status.DEAD or status.status == fb303_status.STOPPED
        ):
            self.run_edenfs_not_healthy_checks()
        else:
            self.tracker.add_problem(EdenfsUnexpectedStatus(status))

    def check_working_directory(self) -> None:
        problem = check_for_working_directory_problem()
        if problem:
            self.tracker.add_problem(problem)

    def run_edenfs_not_healthy_checks(self) -> None:
        # None means the list could not be read, so whether EdenFS is in use
        # is unknown: skip the checks that depend on it.
        configured_mounts = self._get_configured_mount_paths()
        if configured_mounts:
            if not self.fast and not is_asic():
                # Run network checks without a backing repo
                try:
                    self.network_checker.check_network(
                        # On windows, if run in a mount when eden is stopped, it will
                        # report an error, so run in a neutral location
                        self.tracker,
                        Path("/"),
                        set(),
                        False,
                    )
                except Exception as ex:
                    raise RuntimeError("Failed to check network for mount") from ex

            self.tracker.add_problem(EdenfsNotHealthy(self.instance, self.out))
        elif configured_mounts is not None:
            self.tracker.using_edenfs = False
            return

        if self.run_system_wide_checks:
            check_stale_mounts.check_for_stale_mounts(
                self.tracker, self.mount_table, self.instance
            )
            check_filesystems.check_disk_usage(
                self.tracker,
                configured_mounts or [],
                self.instance,
                fs_util=self.fs_util,
            )

    def _get_checkouts_info(self) -> Dict[Path, CheckoutInfo]:
        return get_checkouts_info(self.instance)

    def _get_configured_mount_paths(self) -> Optional[List[str]]:
        try:
            return list(self.instance.get_mount_paths())
        except Exception as ex:
            self.tracker.add_problem(
                ConfiguredCheckoutsUnreadable(self.instance.state_dir, ex)
            )
            return None

    def check_privhelper(self) -> None:
        try:
            connected = self.instance.check_privhelper_connection()
            if not connected:
                self.tracker.add_problem(EdenfsPrivHelperNotHealthy())
        except Exception as ex:
            # This check is only reached after we've determined that the EdenFS
            # daemon is healthy, so any error thrown while calling into Thrift
            # would be unexpected.
            self.tracker.add_problem(UnexpectedPrivHelperProblem(ex))

    def check_running_elevated(self) -> None:
        if sys.platform != "win32":
            return

        # proc_utils_win's ctypes dependences can't be imported on non-Windows.
        from eden.fs.cli import proc_utils_win

        health_status = self.instance.check_health()
        if health_status.pid is None:
            return

        try:
            # pyrefly: ignore [bad-argument-type]
            process_handle = proc_utils_win.open_process(health_status.pid)
            token_handle = proc_utils_win.open_process_token(process_handle)
            elevated = proc_utils_win.is_token_elevated(token_handle)
        except Exception as ex:
            self.tracker.add_problem(UnknownElevationProblem(health_status.pid, ex))
            return

        # pyrefly: ignore [unbound-name]
        if elevated:
            self.tracker.add_problem(RunningElevatedProblem(health_status.pid))

    def run_normal_checks(self) -> None:
        check_edenfs_version(self.tracker, self.instance, self.dry_run or self.fast)
        try:
            checkouts = self._get_checkouts_info()
        except RuntimeError as ex:
            self.tracker.add_problem(EdenCheckoutInfosCorruption(ex))
            return
        log_missing_eden_root_mount_health_issues(self.instance, checkouts.values())
        checked_backing_repos = set()
        checked_network_backing_repos = set()

        if sys.platform == "win32":
            self.check_running_elevated()
        else:
            self.check_privhelper()

        if self.run_system_wide_checks:
            check_filesystems.check_eden_directory(self.tracker, self.instance)
            check_stale_mounts.check_for_stale_mounts(
                self.tracker, self.mount_table, self.instance
            )
            check_filesystems.check_disk_usage(
                self.tracker,
                self._get_configured_mount_paths() or [],
                self.instance,
                fs_util=self.fs_util,
            )
            check_slow_hg_import(self.tracker, self.instance)
            check_facebook(
                self.tracker,
                list(checkouts.values()),
                checked_backing_repos,
                # pyrefly: ignore [bad-argument-type]
                vscode_extensions_checker=self.vscode_extensions_checker,
                eden_instance=self.instance,
            )

        watchman_info = check_watchman.pre_check()

        for path, checkout in sorted(checkouts.items()):
            self.out.writeln(f"Checking {path}")
            try:
                check_mount(
                    self.out,
                    self.tracker,
                    self.instance,
                    checkout,
                    self.mount_table,
                    watchman_info,
                    list(checkouts.values()),
                    checked_backing_repos,
                    checked_network_backing_repos,
                    self.network_checker,
                    self.debug,
                    self.fast,
                )
            except Exception as ex:
                self.tracker.add_problem(UnexpectedMountProblem(path, ex))


class EdenDoctor(EdenDoctorChecker):
    fixer: ProblemFixer
    min_severity_to_report: ProblemSeverity
    lock_file_path: Path
    timeout: int
    TIMEOUT_SECONDS: int = 10

    def __init__(
        self,
        instance: EdenInstance,
        dry_run: bool,
        debug: bool,
        fast: bool,
        wait: bool,
        min_severity_to_report: ProblemSeverity,
        mount_table: Optional[mtab.MountTable] = None,
        fs_util: Optional[filesystem.FsUtil] = None,
        proc_utils: Optional[proc_utils_mod.ProcUtils] = None,
        vscode_extensions_checker: Optional[VSCodeExtensionsChecker] = None,
        network_checker: Optional[check_network.NetworkChecker] = None,
        out: Optional[ui.Output] = None,
    ) -> None:
        self.min_severity_to_report = min_severity_to_report
        self.lock_file_path = instance.state_dir / ".doctor.lock"
        if wait:
            self.timeout = -1  # wait forever
        else:
            self.timeout = self.TIMEOUT_SECONDS
        out = out if out is not None else ui.get_output()
        if dry_run:
            self.fixer = DryRunFixer(
                instance,
                out,
                debug,
                min_severity_to_report,
            )
        else:
            self.fixer = ProblemFixer(
                instance,
                out,
                debug,
                min_severity_to_report,
            )

        super().__init__(
            instance,
            tracker=self.fixer,
            dry_run=dry_run,
            debug=debug,
            fast=fast,
            mount_table=mount_table,
            fs_util=fs_util,
            proc_utils=proc_utils,
            vscode_extensions_checker=vscode_extensions_checker,
            network_checker=network_checker,
            out=out,
        )

    def cure_what_ails_you(self) -> int:
        if self.instance.get_config_bool("doctor.single-instance-only", False):
            lock = FileLock(self.lock_file_path, timeout=self.timeout)
            try:
                with lock:
                    self.run_checks()
                    return self._report_problems()
            except Timeout:
                print("An instance of EdenFS doctor is already running")
                return 1
        else:
            self.run_checks()
            return self._report_problems()

    def _report_problems(self) -> int:
        fixer = self.fixer
        out = self.out

        if not self.dry_run:
            self.instance.log_sample(
                "eden_doctor",
                num_problems=fixer.num_problems,
                problems=fixer.problem_types.union(fixer.ignored_problem_types),
                problem_description=fixer.problem_description,
                num_fixed_problems=fixer.num_fixed_problems,
                num_failed_fixes=fixer.num_failed_fixes,
                num_manual_fixes=fixer.num_manual_fixes,
                num_no_fixes=fixer.num_no_fixes,
                num_advisory_fixes=fixer.num_advisory_fixes,
                problem_failed_fixes=fixer.problem_failed_fixes,
                problem_successful_fixes=fixer.problem_successful_fixes,
                problem_manual_fixes=fixer.problem_manual_fixes,
                problem_no_fixes=fixer.problem_no_fixes,
                problem_advisory_fixes=fixer.problem_advisory_fixes,
                exception="\n".join(fixer.problem_failed_fixes_exceptions),
            )
        elif sys.platform == "win32":
            # dry run doesn't run fixes so we count the number of fixable problems rather
            # than the number of failed fixes
            self.instance.log_sample(
                "eden_doctor_dry_run",
                num_problems=fixer.num_problems,
                problems=fixer.problem_types.union(fixer.ignored_problem_types),
                problem_description=fixer.problem_description,
                num_fixable=fixer.num_fixable,
                num_manual_fixes=fixer.num_manual_fixes,
                num_no_fixes=fixer.num_no_fixes,
                num_advisory_fixes=fixer.num_advisory_fixes,
                problem_fixable=fixer.problem_fixable,
                problem_manual_fixes=fixer.problem_manual_fixes,
                problem_no_fixes=fixer.problem_no_fixes,
                problem_advisory_fixes=fixer.problem_advisory_fixes,
            )

        if fixer.num_problems == 0:
            if not fixer.using_edenfs:
                out.writeln(
                    "EdenFS daemon is not running. No Eden checkouts are "
                    "configured, so this is not considered an issue. "
                    "If you need Eden checkouts, create one with "
                    "'fbclone <repo> --eden'."
                )
            else:
                out.writeln("No issues detected.", fg=out.GREEN)
            return 0

        def problem_count(num: int) -> str:
            if num == 1:
                return "1 problem"
            return f"{num} problems"

        if self.dry_run:
            out.writeln(
                f"Discovered {problem_count(fixer.num_problems)} during --dry-run",
                fg=out.YELLOW,
            )
            return 1

        if fixer.num_fixed_problems:
            out.writeln(
                f"Successfully fixed {problem_count(fixer.num_fixed_problems)}.",
                fg=out.YELLOW,
            )

        if fixer.num_failed_fixes:
            out.writeln(
                f"Failed to fix {problem_count(fixer.num_failed_fixes)}.", fg=out.RED
            )

        if fixer.num_advisory_fixes:
            out.writeln(
                f"{fixer.num_advisory_fixes} issue{'' if fixer.num_advisory_fixes == 1 else 's'} with recommended fixes.",
                fg=out.YELLOW,
            )

        if fixer.num_manual_fixes:
            if fixer.num_manual_fixes == 1:
                msg = "1 issue requires manual attention."
            else:
                msg = f"{fixer.num_manual_fixes} issues require manual attention."
            out.writeln(msg, fg=out.YELLOW)

        if fixer.num_no_fixes:
            if fixer.num_no_fixes == 1:
                msg = "No standard fix for 1 issue."
            else:
                msg = f"No standard fix for {fixer.num_no_fixes} issues."
            out.writeln(msg, fg=out.RED)

        if fixer.num_fixed_problems == fixer.num_problems:
            return 0

        if sys.platform == "darwin":
            help_url = "https://fb.workplace.com/groups/edenfsmacos"
            help_group = "EdenFS macOS"
        elif sys.platform == "win32":
            help_url = "https://fb.workplace.com/groups/edenfswindows"
            help_group = "EdenFS Windows"
        else:
            help_url = "https://fb.workplace.com/groups/eden.users"
            help_group = "EdenFS"

        out.write(
            f"Collect an 'eden rage' and ask in the {help_group} Users group if you need help fixing issues with EdenFS:\n"
            f"{help_url}\n"
        )
        return 1


class EdenfsNotHealthy(FixableProblem):
    def __init__(
        self,
        instance: EdenInstance,
        out: ui.Output,
    ) -> None:
        self._instance = instance
        self._out = out

    def description(self) -> str:
        return "EdenFS is not running"

    def dry_run_msg(self) -> str:
        return "Would run `eden start` to start EdenFS"

    def start_msg(self) -> str:
        return "Running `eden start` to start EdenFS..."

    def perform_fix(self) -> None:
        """Try to start EdenFS. If Eden is running, an exception will be thrown (and ignored)."""
        try:
            daemon.start_edenfs_service(
                self._instance, daemon_binary=None, edenfs_args=None, preserved_env=None
            )
        except Exception:
            # Eden start failed, or Eden is already running/starting. Either way,
            # check_fix will determine if the fix worked.
            pass

    def check_fix(self) -> bool:
        health = self._instance.check_health()
        if health.is_starting():
            self._out.writeln(
                "EdenFS still starting, use `eden status --wait` to watch progress and ensure it starts",
                fg=self._out.YELLOW,
            )
            return False
        elif health.is_healthy():
            return True
        else:
            return False


class EdenfsPrivHelperNotHealthy(Problem):
    def __init__(self) -> None:
        super().__init__(
            "The PrivHelper process is not accessible.",
            remediation="To restore the connection to the PrivHelper, run `eden restart`",
        )


class EdenfsStarting(Problem):
    def __init__(self) -> None:
        remediation = '''\
Please wait for edenfs to finish starting. You can watch its progress with
`eden status --wait`.

If EdenFS seems to be taking too long to start you can try restarting it
with "eden restart --force"'''
        super().__init__(
            "EdenFS is currently still starting.",
            remediation=remediation,
            severity=ProblemSeverity.ADVICE,
        )


class EdenfsStopping(Problem):
    def __init__(self) -> None:
        remediation = """\
Either wait for edenfs to exit, or to forcibly kill EdenFS, run:

    eden stop --kill"""
        super().__init__("EdenFS is currently shutting down.", remediation=remediation)


class EdenfsUnexpectedStatus(Problem):
    def __init__(self, status: config_mod.HealthStatus) -> None:
        msg = f"Unexpected health status reported by edenfs: {status}"
        remediation = 'You can try restarting EdenFS by running "eden restart"'
        super().__init__(msg, remediation=remediation)


class CheckoutIsStartingUp(Problem):
    def __init__(self, checkout: CheckoutInfo) -> None:
        super().__init__(
            f"Checkout {checkout.path} is currently starting up.",
            "If this checkout does not successfully finish starting soon, "
            'try running "eden restart"',
            severity=ProblemSeverity.ADVICE,
        )


class CheckoutIsShuttingDown(Problem):
    def __init__(self, checkout: CheckoutInfo) -> None:
        super().__init__(
            f"Checkout {checkout.path} is currently shutting down.",
            "If this checkout does not successfully finish shutting down soon, "
            'try running "eden restart"',
        )


class CheckoutFailedDuetoFuseError(Problem):
    def __init__(self, checkout: CheckoutInfo) -> None:
        super().__init__(
            f"Checkout {checkout.path} encountered a FUSE error while mounting"
        )


class CheckoutInUnknownState(Problem):
    def __init__(self, checkout: CheckoutInfo) -> None:
        super().__init__(
            f"edenfs reports that checkout {checkout.path} is in "
            "unknown state {checkout.state}"
        )


class NestedCheckout(Problem):
    def __init__(
        self, checkout: CheckoutInfo, existing_checkout: config_mod.EdenCheckout
    ) -> None:
        super().__init__(
            f"""\
edenfs reports that checkout {checkout.path} is nested within an existing checkout {existing_checkout.path}
Nested checkouts are usually not intended and can cause spurious behavior.""",
            f"Consider running `eden rm {checkout.path}` to remove misplaced repo(s)\n",
            severity=ProblemSeverity.ADVICE,
        )


class CheckoutInsideBackingRepo(Problem):
    def __init__(
        self, checkout: CheckoutInfo, existing_checkout: config_mod.EdenCheckout
    ) -> None:
        super().__init__(
            f"""\
edenfs reports that checkout {checkout.path} is created within backing repo of an existing checkout {existing_checkout.path} (backing repo: {existing_checkout.get_backing_repo_path()})
Checkouts inside backing repo are usually not intended and can cause spurious behavior.""",
            f"Consider running `eden rm {checkout.path}` to remove misplaced repo(s)\n",
            severity=ProblemSeverity.ADVICE,
        )


class EdenCheckoutCorruption(Problem):
    def __init__(self, checkout: CheckoutInfo, ex: Exception) -> None:
        remediation = get_reclone_msg(str(checkout.path))

        super().__init__(
            f"Eden's checkout state for {checkout.path} has been corrupted: {ex}",
            remediation=remediation,
        )


class CheckoutConfigUnreadable(Problem):
    def __init__(self, checkout: CheckoutInfo, ex: Exception) -> None:
        super().__init__(
            f"{checkout.path} is configured, but EdenFS cannot mount it because "
            f"its checkout config could not be read:\n{ex}\n"
            "EdenFS skips this checkout on startup, so it will stay unmounted.",
            remediation=f"To remove it, run: `eden rm {checkout.path}`\n"
            "Then clone it again if you still need it.",
        )


class EdenCheckoutConfigCorruption(FixableProblem):
    _checkout_info: CheckoutInfo
    _ex: Exception

    def __init__(self, checkout_info: CheckoutInfo, ex: Exception) -> None:
        self._checkout_info = checkout_info
        self._ex = ex

    def is_nfs_default(self) -> bool:
        default_protocol = util_mod.get_platform_default_mount_protocol()
        return (
            default_protocol == util_mod.NFS_MOUNT_PROTOCOL_STRING
            or self._checkout_info.instance.get_config_value(
                "clone.default-mount-protocol", default_protocol
            ).lower()
            == util_mod.NFS_MOUNT_PROTOCOL_STRING
        )

    def description(self) -> str:
        return f"Eden's checkout state for {self._checkout_info.path} has been corrupted: {self._ex}"

    def dry_run_msg(self) -> str:
        return "Would reinitialize the checkout config"

    def start_msg(self) -> str:
        return "Reinitialize checkout config...."

    def get_repo_type(self, state_dir: Path) -> str:
        hgpath = state_dir / ".hg"
        if not hgpath.exists():
            return "other"
        if (hgpath / "requires").exists():
            with open(hgpath / "requires", "r") as f:
                for line in f:
                    if line.startswith("edensparse"):
                        return "filteredhg"
        return "hg"

    def get_backup_path(self, config_path: Path) -> Path:
        for i in range(1, 9):
            backup_path = config_path.with_suffix(f".bak{i}")
            if not backup_path.exists():
                return backup_path
        print(
            f"Maximum number of Eden CheckoutConfig backup files reached. Please delete some of the backup files at {config_path} manually.",
            file=sys.stderr,
        )
        return config_path.with_suffix(".bak_final")

    def perform_fix(self) -> None:
        """
        Attempts to regenerate the config.toml file for the checkout from
        the running state's checkout info. This does not work if eden is not
        running.
        """
        # Get the state dir. This is where the config will be written to.
        if self._checkout_info.running_state_dir is not None:
            state_dir = self._checkout_info.running_state_dir
        elif self._checkout_info.configured_state_dir is not None:
            state_dir = self._checkout_info.configured_state_dir
        else:
            raise Exception("checkout info missing state dir")

        # Determine the repo type from the running mount.
        # We can regenerate the config for hg and
        # filteredhg repos.
        repotype = self.get_repo_type(self._checkout_info.path)
        if repotype not in config_mod.HG_REPO_TYPES:
            raise Exception("Cannot fix config for non-hg repo")

        # Get the backing repo. This info links the checkout to the
        # correct backing repo and is written into the config.
        if self._checkout_info.backing_repo is None:
            raise Exception("checkout info missing backing repo")

        repo = util_mod.get_repo(str(self._checkout_info.backing_repo), repotype)
        if repo is None:
            raise util_mod.RepoError(
                f"{self._checkout_info.backing_repo!r} does not look like a valid repository"
            )
        # Double check that this repo is hg
        if repo.type not in config_mod.HG_REPO_TYPES:
            raise Exception("Cannot fix config for non-hg repo")

        # using defaults
        checkout_config = config_mod.create_checkout_config(
            repo,
            self._checkout_info.instance,
            nfs=self.is_nfs_default(),
            case_sensitive=sys.platform == "linux",
            overlay_type=None,
            off_mount_repo_dir=self._checkout_info.instance.get_config_bool(
                "clone.off-mount-repo-dir", False
            ),
        )
        checkout = config_mod.EdenCheckout(
            self._checkout_info.instance, self._checkout_info.path, state_dir
        )
        config_path = checkout._config_path()
        backup_path = self.get_backup_path(config_path)
        print(
            f"Recreated config for checkout {checkout.path}. Previous config file backed up at {backup_path}\n"
            f"This config is created using default values. If further issues persist consider recloning {get_reclone_advice_link()}"
        )
        if os.path.exists(config_path):
            # Create a copy instead of renaming so it is impossible to end up with a missing config
            # file.
            with open(config_path, "rb") as config_file:
                util_mod.write_file_atomically(backup_path, config_file.read())
        checkout.save_config(checkout_config)

    def check_fix(self) -> bool:
        # Tries to read checkout again
        checkout = self._checkout_info.get_checkout()
        try:
            checkout.get_config()
        except Exception as ex:
            print("Could not fix corrupted config.toml")
            raise ex
        return True


class EdenCheckoutInfosCorruption(Problem):
    def __init__(self, ex: Exception) -> None:
        remediation = get_reclone_msg("$CHECKOUT_PATH")

        super().__init__(
            f"Encountered errors reading Eden's checkout info for the following checkouts:\n{ex}",
            remediation=remediation,
        )


class ConfiguredCheckoutsUnreadable(Problem):
    def __init__(self, state_dir: Path, ex: Exception) -> None:
        super().__init__(
            "Could not read the list of configured EdenFS checkouts",
            remediation=(
                f"Make sure {state_dir / CONFIG_JSON} can be read and holds a JSON "
                "object that maps each checkout path to the name of its directory "
                f"in {state_dir / CLIENTS_DIR}."
            ),
            exception=ex,
        )


def check_mount(
    out: ui.Output,
    tracker: ProblemTracker,
    instance: EdenInstance,
    checkout: CheckoutInfo,
    mount_table: mtab.MountTable,
    watchman_info: check_watchman.WatchmanCheckInfo,
    all_checkouts: List[CheckoutInfo],
    checked_backing_repos: Set[str],
    checked_network_backing_repos: Set[str],
    network_checker: check_network.NetworkChecker,
    debug: bool,
    fast: bool,
) -> None:
    if checkout.state is None and checkout.config_error is not None:
        # Remounting would fail the same way, so don't offer it as a fix.
        tracker.add_problem(CheckoutConfigUnreadable(checkout, checkout.config_error))
    elif checkout.state is None:
        # This checkout is configured but not currently running.
        tracker.add_problem(
            CheckoutNotMounted(out, checkout, all_checkouts, checked_backing_repos)
        )
    elif checkout.state == MountState.RUNNING:
        try:
            check_running_mount(
                out,
                tracker,
                instance,
                checkout,
                mount_table,
                watchman_info,
                debug,
                fast,
            )
        except Exception as ex:
            raise RuntimeError("Failed to check running mount") from ex
    elif checkout.state in (
        MountState.UNINITIALIZED,
        MountState.INITIALIZING,
        MountState.INITIALIZED,
        MountState.STARTING,
    ):
        try:
            check_starting_mount(
                tracker,
                instance,
                checkout,
                mount_table,
                watchman_info,
                debug,
                fast,
            )
        except Exception as ex:
            raise RuntimeError("Failed to check initializing/starting mount") from ex
    elif checkout.state in (
        MountState.SHUTTING_DOWN,
        MountState.SHUT_DOWN,
        MountState.DESTROYING,
    ):
        tracker.add_problem(CheckoutIsShuttingDown(checkout))
    elif checkout.state == MountState.FUSE_ERROR:
        # TODO: We could potentially try automatically unmounting and remounting.
        # In general mounts shouldn't remain in this state for long, so we probably
        # don't need to worry too much about this case.
        tracker.add_problem(CheckoutFailedDuetoFuseError(checkout))
    else:
        tracker.add_problem(CheckoutInUnknownState(checkout))

    try:
        # Check if this checkout is nested inside another one
        problem_type, existing_checkout = config_mod.detect_checkout_path_problem(
            checkout.path,
            instance,
        )

        if problem_type is not None and existing_checkout is not None:
            if problem_type == config_mod.CheckoutPathProblemType.NESTED_CHECKOUT:
                tracker.add_problem(NestedCheckout(checkout, existing_checkout))
            if problem_type == config_mod.CheckoutPathProblemType.INSIDE_BACKING_REPO:
                tracker.add_problem(
                    CheckoutInsideBackingRepo(checkout, existing_checkout)
                )
    except Exception as ex:
        raise RuntimeError("Failed to detect nested checkout") from ex

    # Network issues could prevent a mount from starting
    if not fast and not is_asic():
        try:
            backing_repo = checkout.get_backing_repo()
            run_repo_check = True
        except AssertionError:
            # This can happen if the backing repo is not yet configured
            backing_repo = Path(os.getcwd())
            run_repo_check = False
        try:
            network_checker.check_network(
                tracker,
                backing_repo,
                checked_network_backing_repos,
                run_repo_check,
            )
        except Exception as ex:
            raise RuntimeError("Failed to check network for mount") from ex


def log_missing_eden_root_mount_health_issues(
    instance: EdenInstance, checkouts: Iterable[CheckoutInfo]
) -> None:
    # Windows stores Eden checkout metadata in .eden/config, not .eden/root.
    if sys.platform == "win32":
        return

    for checkout in checkouts:
        if checkout.state != MountState.RUNNING:
            continue
        if checkout.configured_state_dir is None:
            continue

        root_path = checkout.path / ".eden" / "root"
        try:
            os.readlink(root_path)
        except OSError as ex:
            # Only missing .eden/root should be reported as a health issue
            if ex.errno != errno.ENOENT:
                continue
            checkout_path = str(checkout.path)
            try:
                instance.log_sample(
                    "eden_mount_health_issue",
                    source="doctor_startup",
                    reason="missing_eden_root",
                    mount_path=checkout_path,
                    checkout_path=checkout_path,
                    path_type="configured_checkout",
                    error=str(ex),
                    attempted_repair=False,
                    success=False,
                )
            except Exception:
                log.warning(
                    "Failed to log missing .eden/root mount health telemetry for %s",
                    checkout_path,
                    exc_info=True,
                )


def check_starting_mount(
    tracker: ProblemTracker,
    instance: EdenInstance,
    checkout_info: CheckoutInfo,
    mount_table: mtab.MountTable,
    watchman_info: check_watchman.WatchmanCheckInfo,
    debug: bool,
    fast: bool,
) -> None:
    checkout = checkout_info.get_checkout()
    try:
        checkout.get_config()
        checkout.get_snapshot()
    except config_mod.CheckoutConfigCorruptedError as ex:
        # Config file is missing or invalid
        tracker.add_problem(
            EdenCheckoutConfigCorruption(checkout_info, ex),
        )
        return
    except Exception as ex:
        # Other error
        tracker.add_problem(
            EdenCheckoutCorruption(
                checkout_info,
                ex,
            )
        )
        return
    # Return this if there are no other problems
    tracker.add_problem(CheckoutIsStartingUp(checkout_info))


def check_running_mount(
    out: ui.Output,
    tracker: ProblemTracker,
    instance: EdenInstance,
    checkout_info: CheckoutInfo,
    mount_table: mtab.MountTable,
    watchman_info: check_watchman.WatchmanCheckInfo,
    debug: bool,
    fast: bool,
) -> None:
    if checkout_info.configured_state_dir is None:
        running_state_dir = checkout_info.running_state_dir
        if running_state_dir is None or not instance.can_register_running_mount(
            checkout_info.path, running_state_dir
        ):
            tracker.add_problem(CheckoutNotConfiguredAndStateMissing(checkout_info))
            return
        problem = CheckoutNotConfigured(checkout_info, running_state_dir)
        tracker.add_problem(problem)
        if not problem.check_fix():
            return
        # Registered now: the remaining checks apply as to any running mount.
        checkout_info.configured_state_dir = running_state_dir
    elif checkout_info.configured_state_dir != checkout_info.running_state_dir:
        try:
            configured_state_dir_inode = (
                checkout_info.configured_state_dir.stat().st_ino
                if checkout_info.configured_state_dir is not None
                and checkout_info.configured_state_dir.exists()
                else None
            )
            running_state_dir_inode = (
                checkout_info.running_state_dir.stat().st_ino
                if checkout_info.running_state_dir is not None
                and checkout_info.running_state_dir.exists()
                else None
            )
            if (
                configured_state_dir_inode is not None
                and configured_state_dir_inode == running_state_dir_inode
            ):
                return
        except OSError as ex:
            # If we fail to confirm they have the same inode, we'll just
            # assume they're different and report the problem.
            out.write(f"Error reading inode for state directories: {ex}", flush=True)
        tracker.add_problem(CheckoutConfigurationMismatch(checkout_info))
        return

    checkout = checkout_info.get_checkout()
    try:
        config = checkout.get_config()
    except config_mod.CheckoutConfigCorruptedError as ex:
        # Config file is missing or invalid
        tracker.add_problem(
            EdenCheckoutConfigCorruption(checkout_info, ex),
        )
        return
    except Exception as ex:
        # Other error
        tracker.add_problem(
            EdenCheckoutCorruption(
                checkout_info,
                ex,
            )
        )
        # Just skip the remaining checks.
        # Most of them rely on values from the configuration.
        return

    if checkout_info.visible_in_daemon_namespace is False:
        tracker.add_problem(RunningMountNotVisible(checkout_info))
        return

    try:
        check_filesystems.check_inode_counts(tracker, instance, checkout_info)
    except Exception as ex:
        raise RuntimeError("Failed to check inode counts for mount") from ex

    try:
        check_filesystems.check_using_nfs_path(tracker, checkout.path)
    except Exception as ex:
        raise RuntimeError("Failed to check if backing store is on NFS") from ex

    try:
        check_watchman.check_active_mount(tracker, str(checkout.path), watchman_info)
    except Exception as ex:
        raise RuntimeError("Failed to check watchman status for mount") from ex

    try:
        check_redirections.check_redirections(tracker, instance, checkout, mount_table)
    except Exception as ex:
        raise RuntimeError("Failed to check redirections for mount") from ex

    try:
        check_recent_writes.check_recent_writes(tracker, instance, debug)
    except Exception as ex:
        raise RuntimeError("Failed to check recent writes counts for mount") from ex

    if sys.platform == "win32":
        try:
            if not fast:
                check_filesystems.check_materialized_are_accessible(
                    tracker, instance, checkout, lambda p: os.lstat(p).st_mode
                )
        except Exception as ex:
            raise RuntimeError(
                "Failed to check if materialized files are accessible"
            ) from ex

        try:
            if not fast:
                check_filesystems.check_loaded_content(
                    tracker, instance, checkout, prjfs.PrjGetOnDiskFileState
                )
        except Exception as ex:
            raise RuntimeError("Failed to check loaded content integrity") from ex

    if config.scm_type in ["hg", "filteredhg"]:
        try:
            check_hg.check_hg(tracker, checkout)
        except RuntimeError as ex:
            tracker.add_problem(EdenCheckoutCorruption(checkout_info, ex))
            # Exit here but don't reraise since we're already reporting a problem.
            return
        except Exception as ex:
            raise RuntimeError("Failed to check Sapling status") from ex

        if not fast:
            try:
                check_filesystems.check_hg_status_match_hg_diff(
                    tracker, instance, checkout
                )
            except Exception as ex:
                raise RuntimeError(
                    "Failed to compare `sl status` with `sl diff`"
                ) from ex


class CheckoutNotConfigured(FixableProblem):
    """A mount the daemon serves but config.json does not list: an `eden clone`
    that stopped after the mount and before the directory-map write. EdenFS
    would not remount it after a restart. Registering it with the client
    directory the running mount uses restores the normal state."""

    def __init__(self, checkout_info: CheckoutInfo, client_dir: Path) -> None:
        self._instance = checkout_info.instance
        self._path = checkout_info.path
        self._client_dir = client_dir

    def description(self) -> str:
        return (
            f"Checkout {self._path} is running but not listed in Eden's "
            "configuration file, so EdenFS would not remount it after a restart."
        )

    def dry_run_msg(self) -> str:
        return f"Would register {self._path} in Eden's configuration file"

    def start_msg(self) -> str:
        return f"Registering {self._path} in Eden's configuration file"

    def perform_fix(self) -> None:
        self._instance.register_running_mount(self._path, self._client_dir)

    def check_fix(self) -> bool:
        return str(self._path) in self._instance.get_mount_paths()


class CheckoutNotConfiguredAndStateMissing(Problem):
    """A mount the daemon serves whose client directory is gone or is not one
    of this instance's: it cannot be registered, only removed."""

    def __init__(self, checkout_info: CheckoutInfo) -> None:
        msg = (
            f"Checkout {checkout_info.path} is running but not listed in Eden's "
            "configuration file, and its state directory "
            f"{checkout_info.running_state_dir} cannot be registered."
        )
        quoted_path = shlex.quote(str(checkout_info.path))
        remediation = f'Running "eden rm {quoted_path}" will remove this checkout.'
        super().__init__(msg, remediation)


class CheckoutConfigurationMismatch(Problem):
    def __init__(self, checkout_info: CheckoutInfo) -> None:
        msg = f"""\
The running configuration for {checkout_info.path} is different than "
the on-disk state in Eden's configuration file:
- Running state directory:    {checkout_info.running_state_dir}
- Configured state directory: {checkout_info.configured_state_dir}"""
        remediation = """\
Running `eden restart` will cause EdenFS to restart and use the data from the
on-disk configuration."""
        super().__init__(msg, remediation)


class RunningMountNotVisible(FixableProblem):
    def __init__(self, checkout_info: CheckoutInfo) -> None:
        self._instance = checkout_info.instance
        self._mount_path = checkout_info.path

    def description(self) -> str:
        return (
            f"{self._mount_path} is RUNNING in EdenFS state, but is not visible "
            "in EdenFS's mount namespace"
        )

    def dry_run_msg(self) -> str:
        return f"Would unmount and remount {self._mount_path}"

    def start_msg(self) -> str:
        return f"Unmounting and remounting {self._mount_path}"

    def perform_fix(self) -> None:
        self._instance.unmount(
            str(self._mount_path),
            use_force=True,
            unintentional_unmount=True,
        )
        self._instance.mount(str(self._mount_path), False)

    def check_fix(self) -> bool:
        try:
            with self._instance.get_thrift_client() as client:
                for mount in client.listMounts():
                    if Path(os.fsdecode(mount.mountPoint)) == self._mount_path:
                        return (
                            mount.state == MountState.RUNNING
                            and mount.visibleInDaemonNamespace is not False
                        )
        except Exception:
            return False
        return False


class CheckoutNotMounted(FixableProblem):
    _out: ui.Output
    _instance: EdenInstance
    _mount_path: Path
    _backing_repo: Path
    _all_checkouts: List[CheckoutInfo]
    _checked_backing_repos: Set[str]

    def __init__(
        self,
        out: ui.Output,
        checkout_info: CheckoutInfo,
        all_checkouts: List[CheckoutInfo],
        checked_backing_repos: Set[str],
    ) -> None:
        self._out = out
        self._instance = checkout_info.instance
        self._mount_path = checkout_info.path
        self._backing_repo = checkout_info.get_backing_repo()
        self._all_checkouts = all_checkouts
        self._checked_backing_repos = checked_backing_repos

    def description(self) -> str:
        return f"{self._mount_path} is not currently mounted"

    def dry_run_msg(self) -> str:
        return f"Would remount {self._mount_path}"

    def start_msg(self) -> str:
        return f"Remounting {self._mount_path}"

    def perform_fix(self) -> None:
        try:
            self._instance.mount(str(self._mount_path), False)
            return
        except Exception as ex:
            # eden corruption
            if "is too short for header" in str(ex):
                raise Exception(
                    f"""\
{ex}

{self._mount_path} appears to have been corrupted.
This can happen if your machine was hard-rebooted.
{get_reclone_msg(str(self._mount_path))}"""
                )

            # if it is not this ^ eden corruption then it could be hg
            # corruption that hg doctor could fix. Let's try hg doctor and then
            # retry the mount for this case.

        self._out.write(
            "\nMount failed. Running `sl doctor` in the backing repo and then "
            "will retry the mount.\n",
            flush=True,
        )
        result = hg_doctor_in_backing_repo(
            self._backing_repo,
            get_dependent_repos(self._backing_repo, self._all_checkouts),
            self._checked_backing_repos,
        )

        try:
            self._instance.mount(str(self._mount_path), False)
        except Exception as ex:
            # If the result is non None then hg tried to fix something. It must
            # not have succeeded all the way because the mount still failed.
            if result is not None:
                raise Exception(
                    f"""\
Failed to remount this mount with error:

{ex}

{result}"""
                )
            else:
                raise

    def check_fix(self) -> bool:
        """
        Check that mount return value is 1(already mounted) when rerunning command
        """
        try:
            mount_value = self._instance.mount(str(self._mount_path), False)
        except Exception as ex:
            """
            This should only happen if the mount becomes corrupted
            between the time of the remount and the time of the check.
            """
            self._out.write(
                f"\nAttempt to fix missing mount failed: {ex}.\n",
                flush=True,
            )
            return False
        if mount_value == 1:
            return True
        elif mount_value == 0:
            """
            This should only happen if the mount somehow gets unmounted
            between the time of the remount and the time of the check.
            """
            return True
        return False


class StaleWorkingDirectory(Problem):
    def __init__(self, msg: str) -> None:
        remediation = 'Run "cd / && cd -" to update your shell\'s working directory.'
        super().__init__(msg, remediation)


def check_for_working_directory_problem() -> Optional[Problem]:
    # Report an issue if the working directory points to a stale mount point
    if working_directory_was_stale:
        msg = "Your current working directory appears to be a stale EdenFS mount point"
        return StaleWorkingDirectory(msg)

    # If the $PWD environment variable is set, confirm that it points our current
    # working directory.
    #
    # This helps catch problems where the current working directory has been replaced
    # with a new mount point but the user hasn't cd'ed into the new mount yet.  For
    # instance this can happen if the user cd'ed into a checkout directory before Eden
    # was running, and then started EdenFS.  The user will still need to cd again to see
    # the EdenFS checkout contents.
    pwd = os.environ.get("PWD")
    if pwd is None:
        # If $PWD isn't set we can't check anything else
        return None

    try:
        pwd_stat = os.stat(pwd)
        cwd_stat = os.stat(".")
    except Exception:
        # If we fail to stat either directory just ignore the error and don't report a
        # problem for now.  If we ever see a real issue in practice hit this scenario we
        # can add an appropriate error message at that point in time.
        #
        # We've already handled stale mounts above, and `os.stat()` checks normally
        # succeed in this case anyway.
        #
        # Users can get into situations where they don't have permissions to
        # stat the current working directory in some cases, but this should be very
        # rare and they will probably be able to identify the issue themselves in this
        # case.
        return None

    if (pwd_stat.st_dev, pwd_stat.st_ino) == (cwd_stat.st_dev, cwd_stat.st_ino):
        # Everything looks okay
        return None

    msg = """\
Your current working directory is out-of-date.
This can happen if you have (re)started EdenFS but your shell is still pointing to
the old directory from before the EdenFS checkouts were mounted.
"""
    return StaleWorkingDirectory(msg)


class BadEdenFsVersion(Problem):
    def __init__(self, running_version: str, reasons: List[str]) -> None:
        reasons_string = "\n    ".join(reasons)
        help_string = f"""\
The version of EdenFS that is running on your machine is:
    {running_version}
This version is known to have issue:
    {reasons_string}
"""

        remediation_string = f"Run `edenfsctl restart{'' if sys.platform == 'win32' else ' --graceful'}` to migrate to the newer version to avoid these issues."
        super().__init__(
            dedent(help_string), remediation_string, severity=ProblemSeverity.ADVICE
        )


class OutOfDateVersion(Problem):
    def __init__(self, installed_version: str, running_version: str) -> None:
        help_string = f"""\
The version of EdenFS that is installed on your machine is:
    {installed_version}
but the version of EdenFS that is currently running is:
    {running_version}
"""

        remediation_string = f"""Consider running `edenfsctl restart{" --graceful" if sys.platform != "win32" else ""}` to migrate to the newer version,
which may have important bug fixes or performance improvements.
"""
        super().__init__(
            dedent(help_string),
            dedent(remediation_string),
            severity=ProblemSeverity.ADVICE,
        )


def check_edenfs_version(
    tracker: ProblemTracker, instance: EdenInstance, likely_automation: bool
) -> None:
    # get released version parts
    rver, release = instance.get_running_version_parts()
    if not rver or not release:
        # This could be a dev build that returns the empty
        # string for both of these values.
        return

    # check for bad eden fs running version
    bad_version_reasons_map = instance.get_known_bad_edenfs_versions()
    running_version = version.format_eden_version((rver, release))
    running_version_str = (
        f"fb.eden {running_version}"
        if sys.platform == "win32"
        else f"fb-eden-{running_version}.x86_64"
    )
    if running_version in bad_version_reasons_map:
        reasons = bad_version_reasons_map[running_version]
        tracker.add_problem(BadEdenFsVersion(running_version_str, reasons))
        # if bad version, don't check for out of date version
        return

    # (T232917962): The out of date version advice as its implemented now is really only helpful for
    # users who manually are running doctor. This is because of an edge case in the logic below:
    # If we haven't had a release in >2 weeks, then we will immediately report the out of date
    # version problem if `eden doctor` is ran anytime between the release and the next restart.
    # The automatic restarter script will eventually fix this, but in the worst case, this could
    # take more than a week. At the time of writing, the automatic restarter will only attempt to
    # restart daemons with an uptime of more than 7 days, so if the daemon was restarted right
    # before the release, it will trigger this code path for at least 7 days. This advice was
    # originally added under the assumption that users would be running doctor manually, but now
    #  many tools run it continuously, so this check as it exists today causes confusion. Until the
    # task is addressed, we will disable this check for automation, which tends to run doctor with
    # `--dry-run` and/or `--fast`.
    if likely_automation:
        return

    # get installed version parts
    iversion, irelease = version.get_current_version_parts()
    if not iversion or not irelease:
        # dev build of eden client returns empty strings here
        return

    # check if the running version is more than two weeks old
    iversion_date = version.date_from_version(iversion)
    rversion_date = version.date_from_version(rver)
    if not iversion_date or not rversion_date:
        return
    daysgap = iversion_date - rversion_date
    if daysgap.days < 14:
        return

    installed_version = version.format_eden_version((iversion, irelease))
    installed_version_str = (
        f"fb.eden {installed_version}"
        if sys.platform == "win32"
        else f"fb-eden-{installed_version}.x86_64"
    )
    tracker.add_problem(OutOfDateVersion(installed_version_str, running_version_str))


class SlowHgImportProblem(Problem):
    def __init__(self, max_fetch_duration: timedelta) -> None:
        super().__init__(
            description=f"Slow file download taking up to {format_approx_duration(max_fetch_duration)} observed",
            remediation="""\
Try:
- Running `sl debugnetwork`.
- Checking your network connection's performance.
- Running `eden top` to check whether downloads are making progress.""",
            severity=ProblemSeverity.ADVICE,
        )


def check_slow_hg_import(tracker: ProblemTracker, instance: EdenInstance) -> None:
    threshold_s = instance.get_config_int(
        "doctor.slow-hg-import-problem-threshold-seconds", 60
    )
    threshold = timedelta(seconds=threshold_s)

    with instance.get_thrift_client() as client:
        max_duration_us = client.getCounter("store.sapling.live_import.max_duration_us")

    max_duration = timedelta(microseconds=max_duration_us)
    if max_duration > threshold:
        tracker.add_problem(SlowHgImportProblem(max_duration))

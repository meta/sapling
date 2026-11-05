#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2.

"""Script to build Sapling (sl) and ISL

Currently support:
- facebook/sapling (github) cargo build (without edenfs).
- facebook/sapling (github) getdeps cargo build (with edenfs, requires fbthrift).
- fbsource (monorepo) cargo build with fb-internal features.
- fbsource (monorepo) cargo build without fb-internal features (--oss).

Does not support EdenFS related features yet.
"""

import argparse
import datetime
import glob
import hashlib
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parent
FBSOURCE = ROOT.parent.parent.parent if (ROOT / "fb").is_dir() else None
OUT = ROOT / "out"
CARGO_TARGET = OUT / "cargo-target"
CARGO_CONFIG_DIR = OUT / "cargo_config"
HGMAIN_MANIFEST = ROOT / "exec" / "hgmain" / "Cargo.toml"
_VCVARS_ENV = None
BUILD_MODES = ("oss", "fbsource", "getdeps")
RUST_BUILD_TOOLS = ("cargo", "buck")
BUCK_SL_TARGET = "fbcode//eden/scm:hg"


def status(args, message):
    if not args.quiet:
        print(message, file=sys.stderr)


def run(cmd, *, cwd=ROOT, env=None, quiet=False):
    if not quiet:
        print("$", subprocess.list2cmdline([str(c) for c in cmd]), file=sys.stderr)
    subprocess.run([str(c) for c in cmd], cwd=cwd, env=env, check=True)


def output(cmd, *, cwd=ROOT, env=None):
    return subprocess.check_output(
        [str(c) for c in cmd], cwd=cwd, env=env, stderr=subprocess.DEVNULL
    ).decode("utf-8", "replace")


def load_out_env():
    env_path = OUT / "env"
    if not env_path.exists():
        return {}
    result = {}
    with open(env_path, "r") as f:
        for line in f:
            line = line.rstrip("\n")
            if "=" in line:
                key, value = line.split("=", 1)
                result[key] = value
    return result


def find_vcvarsall():
    if os.name != "nt":
        return ""

    program_files_x86 = os.environ.get("ProgramFiles(x86)")
    if program_files_x86:
        vswhere = os.path.join(
            program_files_x86,
            "Microsoft Visual Studio",
            "Installer",
            "vswhere.exe",
        )
        if os.path.isfile(vswhere):
            try:
                paths = output(
                    [
                        vswhere,
                        "-latest",
                        "-products",
                        "*",
                        "-find",
                        r"VC\Auxiliary\Build\vcvarsall.bat",
                        "-utf8",
                    ]
                ).splitlines()
            except (OSError, subprocess.CalledProcessError) as ex:
                print(f"warning: failed to run {vswhere}: {ex}", file=sys.stderr)
            else:
                for path in paths:
                    path = path.strip()
                    if path and os.path.isfile(path):
                        return path

    patterns = []
    for envvar in ("ProgramFiles", "ProgramFiles(x86)"):
        root = os.environ.get(envvar)
        if root:
            for version_pattern in ("20[0-9][0-9]", "1[0-9]"):
                patterns.append(
                    os.path.join(
                        root,
                        "Microsoft Visual Studio",
                        version_pattern,
                        "*",
                        "VC",
                        "Auxiliary",
                        "Build",
                        "vcvarsall.bat",
                    )
                )
    paths = [path for pattern in patterns for path in glob.glob(pattern)]
    return sorted(paths, reverse=True)[0] if paths else ""


def strip_outer_quotes(value):
    value = value.strip()
    if len(value) >= 2 and value[0] == value[-1] and value[0] in ("'", '"'):
        return value[1:-1]
    return value


def capture_vcvars_env(args, base_env):
    global _VCVARS_ENV
    if os.name != "nt":
        return {}
    if _VCVARS_ENV is not None:
        return _VCVARS_ENV

    vcvarsall = (
        args.vcvarsall
        or base_env.get("VCVARSALL")
        or base_env.get("VCVARSALL_PATH")
        or find_vcvarsall()
    )
    vcvarsall = strip_outer_quotes(vcvarsall)
    if not vcvarsall or not os.path.exists(vcvarsall):
        _VCVARS_ENV = {}
        return _VCVARS_ENV

    status(args, "Loading Visual Studio build environment")
    fd, script = tempfile.mkstemp(suffix=".cmd")
    os.close(fd)
    script_path = Path(script)
    try:
        script_path.write_text(
            f'@echo off\r\ncall "{vcvarsall}" amd64 >nul\r\n'
            "if errorlevel 1 exit /b %errorlevel%\r\nset\r\n",
            encoding="mbcs",
        )
        out = subprocess.check_output(
            ["cmd.exe", "/d", "/q", "/c", str(script_path)],
            env=base_env,
            encoding="utf-8",
            errors="replace",
        )
    finally:
        os.remove(script)
    _VCVARS_ENV = {}
    for line in out.splitlines():
        if "=" in line:
            key, value = line.split("=", 1)
            _VCVARS_ENV[key] = value
    return _VCVARS_ENV


def scoped_env(args, *, msvc=False, base=None):
    env = dict(base or os.environ)
    env.update(load_out_env())
    if msvc:
        env.update(capture_vcvars_env(args, env))
    return env


def ensure_out_dir(args):
    if OUT.exists():
        return
    if OUT.is_symlink():
        OUT.unlink()
    status(args, "Creating out directory")
    try:
        scratch = output(["mkscratch", "path", "--subdir", "sl-out"]).strip()
        scratch_path = Path(scratch)
        scratch_path.mkdir(parents=True, exist_ok=True)
        OUT.symlink_to(scratch_path, target_is_directory=True)
    except Exception:
        OUT.mkdir(parents=True, exist_ok=True)


def pick_python():
    env = os.environ.copy()
    env.update(load_out_env())
    return (
        subprocess.check_output(
            [sys.executable, ROOT / "contrib" / "pick_python.py", sys.executable],
            env=env,
        )
        .decode("utf-8", "replace")
        .strip()
    )


def set_windows_python_path(env, python):
    if os.name != "nt":
        return
    try:
        version = output(
            [
                python,
                "-c",
                "import sys; print(f'{sys.version_info.major}{sys.version_info.minor}')",
            ]
        ).strip()
    except Exception:
        version = f"{sys.version_info.major}{sys.version_info.minor}"
    dll = f"python{version}.dll"
    dirs = [Path(python).parent, Path(sys.base_prefix), Path(sys.prefix)]
    for path in dirs:
        if (path / dll).exists():
            env["PATH"] = os.pathsep.join([str(path), env.get("PATH", "")])
            return


def auto_version():
    env_version = os.environ.get("SAPLING_VERSION")
    if env_version:
        return env_version

    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%d_%H%M%S")
    node = ""
    for cmd in (
        ["sl", "log", "-r.", "-T", "{node|short}"],
        ["git", "rev-parse", "--short=12", "HEAD"],
    ):
        try:
            node = output(cmd).strip()
            if node:
                break
        except Exception:
            pass
    return f"{now}_{node}" if node else now


def version_hash(version):
    data = version.encode("utf-8")
    return str(struct.unpack(">Q", hashlib.sha1(data).digest()[:8])[0])


def rustup_override():
    try:
        for line in output(["rustup", "override", "list"]).splitlines():
            path, toolchain = (line + " ").split()[:2]
            if Path(path).resolve() == ROOT:
                return toolchain
    except Exception:
        pass
    return None


def rust_paths(args):
    suffix = ".exe" if os.name == "nt" else ""
    env = dict(os.environ)
    env.update(load_out_env())
    override = rustup_override()
    if override:
        status(args, f"Using rustup overridden toolchain: {override}")
        return {
            name: output(["rustup", "which", "--toolchain", override, name]).strip()
            for name in ("cargo", "rustc", "rustdoc")
        }

    if FBSOURCE is not None:
        toolchain = FBSOURCE / "third-party/rust-toolchain/rustup/current/basic/bin"
        toolchain = toolchain.resolve()
        cargo = toolchain / f"cargo{suffix}"
        if cargo.exists():
            return {
                "cargo": str(cargo),
                "rustc": str(toolchain / f"rustc{suffix}"),
                "rustdoc": str(toolchain / f"rustdoc{suffix}"),
            }

    return {"cargo": env.get("CARGO_BIN", "cargo")}


def write_cargo_config(args, paths):
    status(args, "Configuring cargo")
    config = """
# On OS X targets, configure the linker to perform dynamic lookup of undefined
# symbols. This allows Python extension dependencies to resolve symbols.

[target.i686-apple-darwin]
rustflags = ["-C", "link-args=-Wl,-undefined,dynamic_lookup"]

[target.x86_64-apple-darwin]
rustflags = ["-C", "link-args=-Wl,-undefined,dynamic_lookup"]

[target.aarch64-apple-darwin]
rustflags = ["-C", "link-args=-Wl,-undefined,dynamic_lookup"]

[target.arm64-apple-darwin]
rustflags = ["-C", "link-args=-Wl,-undefined,dynamic_lookup"]
"""
    if Path("/usr/bin/lld").exists():
        config += """
# Use lld on Linux to improve build time.
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-args=-fuse-ld=lld"]
"""
    elif Path("/usr/bin/ld.gold").exists():
        config += """
# Use ld.gold on Linux to improve build time.
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-args=-fuse-ld=gold"]
"""

    config += "\n[build]\n"
    if args.rust_target:
        config += f'target = "{args.rust_target}"\n'
    config += f'target-dir = "{CARGO_TARGET.resolve()}"\n'
    for name in ("rustc", "rustdoc"):
        if name in paths:
            config += f'{name} = "{paths[name]}"\n'

    vendored = None
    if FBSOURCE is not None:
        candidate = (FBSOURCE / "third-party/rust/vendor").resolve()
        if candidate.exists():
            vendored = candidate
    if not vendored and os.environ.get("RUST_VENDORED_CRATES_DIR"):
        vendored = Path(os.environ["RUST_VENDORED_CRATES_DIR"]).resolve()
    if vendored:
        config += f"""
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "{vendored}"
"""

    if os.name == "nt":
        config = config.replace("\\", "\\\\")
    CARGO_CONFIG_DIR.mkdir(parents=True, exist_ok=True)
    config_path = CARGO_CONFIG_DIR / "config.toml"
    config_path.write_text(config)

    cargo_dir = ROOT / ".cargo"
    link_cargo_dir(cargo_dir, CARGO_CONFIG_DIR)


def link_cargo_dir(cargo_dir, target_dir):
    link_target = Path("out") / "cargo_config"
    if cargo_dir.is_dir() and not cargo_dir.is_symlink():
        shutil.rmtree(cargo_dir)
    elif cargo_dir.exists() or cargo_dir.is_symlink():
        cargo_dir.unlink()

    try:
        cargo_dir.symlink_to(link_target, target_is_directory=True)
    except OSError:
        cargo_dir.mkdir(exist_ok=True)
        link_config_file(cargo_dir, target_dir / "config.toml")


def link_config_file(cargo_dir, config_path):
    link_path = cargo_dir / "config.toml"
    link_target = Path("..") / "out" / "cargo_config" / "config.toml"
    if link_path.exists() or link_path.is_symlink():
        link_path.unlink()
    try:
        link_path.symlink_to(link_target)
    except OSError:
        shutil.copy2(config_path, link_path)


def cargo_features(mode):
    # sl_oss: disable ".hg" support
    # eden: edenfs related features, requires Thrift
    # fb: (fbsource-only) fb-only features
    features = []
    if mode == "oss":
        features.append("sl_oss")
    elif mode == "fbsource":
        features.extend(["fb", "eden"])
    elif mode == "getdeps":
        features.extend(["eden", "sl_oss"])
    else:
        raise RuntimeError(f"unknown build mode: {mode}")
    return " ".join(features)


def windows_openssl_dir(args):
    if FBSOURCE is None or os.name != "nt":
        return None

    # Built with vcpkg 001713e802 (`vcpkg install openssl:x64-windows`, OpenSSL
    # 3.6.5); `packages\openssl_x64-windows` was renamed to this directory and
    # zipped.
    openssl_dirname = "openssl3-windows_x64-windows"
    openssl_filename = f"{openssl_dirname}.zip"
    ensure_out_dir(args)
    openssl_dir = OUT / openssl_dirname
    if openssl_dir.exists():
        return openssl_dir

    archive = OUT / openssl_filename
    lfs_script = FBSOURCE / "fbcode/tools/lfs/lfs.py"
    status(args, "Downloading OpenSSL binaries")
    run(
        [sys.executable, lfs_script, "download", openssl_filename],
        cwd=OUT,
        quiet=args.quiet,
    )

    status(args, "Extracting OpenSSL binaries")
    with zipfile.ZipFile(archive) as zip_file:
        zip_file.extractall(OUT)

    if not openssl_dir.exists():
        raise RuntimeError(f"OpenSSL archive did not create {openssl_dir}")
    return openssl_dir


def copy_windows_openssl_dlls(args, dest):
    openssl_dir = windows_openssl_dir(args)
    if openssl_dir is None:
        return
    status(args, "Copying OpenSSL DLLs")
    for name in ("libcrypto-3-x64.dll", "libssl-3-x64.dll"):
        copy_artifact(openssl_dir / "bin" / name, dest / name)


def cargo_env(args):
    env = scoped_env(args, msvc=(os.name == "nt"))
    if args.mode == "getdeps":
        env["GETDEPS_BUILD"] = "1"
        getdeps_install = env.get("GETDEPS_INSTALL_DIR")
        if getdeps_install and "THRIFT" not in env:
            env["THRIFT"] = str(Path(getdeps_install) / "fbthrift/bin/thrift1")
    env["PYTHON_SYS_EXECUTABLE"] = env["PYO3_PYTHON"] = args.python
    env["SAPLING_VERSION"] = args.version
    env["SAPLING_VERSION_HASH"] = version_hash(args.version)
    env["LIB_DIRS"] = str((OUT / "temp").resolve())
    env.pop("HOMEBREW_CCCFG", None)
    if sys.platform == "darwin" and "OPENSSL_DIR" not in env:
        openssl = Path("/opt/homebrew/opt/openssl")
        if openssl.is_dir():
            env["OPENSSL_DIR"] = str(openssl)
    openssl = windows_openssl_dir(args)
    if openssl is not None:
        env["OPENSSL_DIR"] = str(openssl)
        env.pop("OPENSSL_STATIC", None)
    env["RUSTFLAGS"] = (env.get("RUSTFLAGS", "") + " -Anon_local_definitions").strip()
    # Allow nightly features (smallvec specialization, once_cell_try) on stable Rust.
    # Required for `make oss` / cargo oss builds. Buck builds use nightly toolchain internally.
    env.setdefault("RUSTC_BOOTSTRAP", "1")
    set_windows_python_path(env, args.python)
    return env


def cargo_output_path(args):
    parts = [CARGO_TARGET]
    if args.rust_target:
        parts.append(args.rust_target)
    parts.append("debug" if args.debug else "release")
    exe = ".exe" if os.name == "nt" else ""
    return Path(*parts) / f"hgmain{exe}"


def buck_build_mode():
    if os.name == "nt":
        return "@fbcode//mode/opt-win"
    if sys.platform == "darwin":
        return "@fbcode//mode/opt-mac"
    return "@fbcode//mode/opt"


def buck_output_path(output_text):
    for line in output_text.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[0] == BUCK_SL_TARGET:
            return (FBSOURCE / parts[1]).resolve()
    raise RuntimeError(f"buck build did not print an output path for {BUCK_SL_TARGET}")


def copy_artifact(src, dest):
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists() or dest.is_symlink():
        dest.unlink()
    if os.name == "nt":
        try:
            os.link(src, dest)
            return
        except OSError:
            pass
    elif sys.platform.startswith("linux"):
        try:
            import fcntl

            with open(src, "rb") as srcf, open(dest, "wb") as destf:
                fcntl.ioctl(destf.fileno(), 0x40049409, srcf.fileno())
            shutil.copystat(src, dest)
            return
        except OSError:
            dest.unlink(missing_ok=True)
    shutil.copy2(src, dest)


def env_path(env):
    for key, value in env.items():
        if key.upper() == "PATH":
            return value
    return None


def set_long_paths_manifest(args, exe):
    if os.name != "nt":
        return
    long_paths_manifest = """\
<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
    <application>
        <windowsSettings
        xmlns:ws2="http://schemas.microsoft.com/SMI/2016/WindowsSettings">
            <ws2:longPathAware>true</ws2:longPathAware>
        </windowsSettings>
    </application>
</assembly>"""
    env = scoped_env(args, msvc=True)
    fd, manifest = tempfile.mkstemp(suffix=".xpy.manifest")
    os.close(fd)
    Path(manifest).write_text(long_paths_manifest)
    inputresource = f"-inputresource:{exe};#1"
    outputresource = f"-outputresource:{exe};#1"
    mt = shutil.which("mt.exe", path=env_path(env))
    if mt is None:
        raise RuntimeError("mt.exe not found in Visual Studio build environment PATH")
    cmd = [mt, "-nologo", "-manifest", manifest, outputresource, inputresource]
    try:
        try:
            subprocess.check_output(cmd, stderr=subprocess.STDOUT, env=env)
        except subprocess.CalledProcessError as ex:
            if b"c101008c" not in ex.output.lower():
                raise
            cmd.remove(inputresource)
            subprocess.check_output(cmd, stderr=subprocess.STDOUT, env=env)
    finally:
        os.remove(manifest)


def eden_prefetch_fbsource(args, path, message):
    if FBSOURCE is None:
        return
    status(args, message)
    run(
        ["eden", "prefetch", f"{path}/**"],
        cwd=FBSOURCE,
        quiet=args.quiet,
    )


def build_sl(args):
    ensure_out_dir(args)
    if args.rust_build_tool == "buck":
        build_sl_with_buck(args)
    else:
        build_sl_with_cargo(args)


def build_sl_with_cargo(args):
    paths = rust_paths(args)
    write_cargo_config(args, paths)

    lock = HGMAIN_MANIFEST.parent / "Cargo.lock"
    if lock.exists():
        status(args, "Removing Cargo.lock")
        lock.unlink()

    cmd = [paths.get("cargo", "cargo"), "build", "--manifest-path", HGMAIN_MANIFEST]
    if args.rust_target:
        cmd.append(f"--target={args.rust_target}")
    if not args.debug:
        cmd.append("--release")
    features = cargo_features(args.mode)
    if features:
        cmd.extend(["--features", features])

    eden_prefetch_fbsource(
        args, "third-party/rust/vendor", "Prefetching vendored Rust crates"
    )
    status(args, "Building sl")
    run(cmd, env=cargo_env(args), quiet=args.quiet)

    src = cargo_output_path(args)
    if os.name == "nt":
        status(args, "Embedding Windows long path manifest")
        for _ in range(6):
            try:
                set_long_paths_manifest(args, src)
                break
            except Exception:
                if _ == 5:
                    raise

    exe = ".exe" if os.name == "nt" else ""
    dest = OUT / f"sl{exe}"
    status(args, f"Copying sl to {dest}")
    copy_artifact(src, dest)

    pdb = src.with_suffix(".pdb")
    if pdb.exists():
        status(args, "Copying sl debug symbols")
        copy_artifact(pdb, OUT / "sl.pdb")
    copy_windows_openssl_dlls(args, OUT)


def build_sl_with_buck(args):
    cmd = [
        "buck2",
        "build",
        buck_build_mode(),
        BUCK_SL_TARGET,
        "--show-output",
    ]
    status(args, "Building sl with buck")
    if not args.quiet:
        print("$", subprocess.list2cmdline(cmd), file=sys.stderr)
    output_text = subprocess.check_output(
        cmd,
        cwd=FBSOURCE,
        env=scoped_env(args),
        encoding="utf-8",
        errors="replace",
    )
    if not args.quiet:
        print(output_text, end="", file=sys.stderr)

    exe = ".exe" if os.name == "nt" else ""
    dest = OUT / f"sl{exe}"
    status(args, f"Copying sl to {dest}")
    copy_artifact(buck_output_path(output_text), dest)


def build_isl(args):
    ensure_out_dir(args)
    addons = (ROOT / "../../addons").resolve()
    if not addons.is_dir():
        addons = (ROOT / "../addons").resolve()
    if not addons.is_dir():
        return

    env = scoped_env(args)
    if FBSOURCE is not None and "YARN" not in env:
        yarn = FBSOURCE / "xplat/third-party/yarn"
        yarn = yarn / ("yarn.bat" if os.name == "nt" else "yarn")
        yarn = yarn.resolve()
        if yarn.exists():
            env["YARN"] = str(yarn)
    # Skip prefetching the yarn offline mirror: it downloads too many files and
    # is too slow for this helper.
    # eden_prefetch_fbsource(
    #     args, "xplat/third-party/yarn", "Prefetching yarn offline mirror"
    # )
    status(args, "Building ISL assets")
    run(
        [args.python, "build-tar.py", "-o", OUT / "isl-dist.tar.xz"],
        cwd=addons,
        env=env,
        quiet=args.quiet,
    )


def add_common_options(parser):
    parser.add_argument(
        "--with-python",
        dest="with_python",
        metavar="PYTHON",
        help=(
            "Python executable used by Rust Python build scripts. Defaults to "
            "the selection from contrib/pick_python.py."
        ),
    )
    parser.add_argument(
        "--with-version",
        dest="with_version",
        metavar="VERSION",
        help=(
            "Sapling version embedded in the binary. By default, uses "
            "SAPLING_VERSION if set, otherwise generates a timestamp plus the "
            "current sl/hg/git node."
        ),
    )
    parser.add_argument(
        "--mode",
        metavar="MODE",
        help=(
            "Build mode. Defaults to getdeps when GETDEPS_BUILD=1, "
            "fbsource when fb/ exists, otherwise oss. Choices: oss, "
            "fbsource, getdeps."
        ),
    )
    parser.add_argument(
        "--oss",
        action="store_true",
        help="Shortcut for --mode oss.",
    )
    parser.add_argument(
        "--getdeps",
        action="store_true",
        help="Shortcut for --mode getdeps.",
    )
    parser.add_argument(
        "--rust-build-tool",
        metavar="TOOL",
        help=(
            "Rust build tool. Defaults to buck in fbsource mode, otherwise "
            "cargo. Choices: cargo, buck. buck is only valid with --mode "
            "fbsource."
        ),
    )
    parser.add_argument(
        "--buck",
        action="store_true",
        help="Shortcut for --rust-build-tool buck.",
    )
    parser.add_argument(
        "--cargo",
        action="store_true",
        help="Shortcut for --rust-build-tool cargo.",
    )
    parser.add_argument(
        "--debug",
        action="store_true",
        help="Run cargo without --release and copy the debug binary to out/sl.",
    )
    parser.add_argument(
        "-q",
        "--quiet",
        action="store_true",
        help="Do not print build progress.",
    )
    parser.add_argument(
        "--rust-target",
        metavar="TRIPLE",
        default=os.environ.get("RUST_TARGET"),
        help=(
            "Cargo target triple. Defaults to RUST_TARGET when that environment "
            "variable is set."
        ),
    )
    parser.add_argument(
        "--vcvarsall",
        metavar="PATH",
        default=os.environ.get("VCVARSALL", ""),
        help=(
            "Windows-only path to vcvarsall.bat. If omitted, out/env "
            "VCVARSALL or VCVARSALL_PATH is used, then Visual Studio "
            "is discovered using vswhere or common install paths."
        ),
    )


def auto_mode():
    if (
        os.environ.get("GETDEPS_BUILD") == "1"
        or os.environ.get("GETDEPS_BUILD_DIR")
        or os.environ.get("GETDEPS_INSTALL_DIR")
    ):
        return "getdeps"
    if FBSOURCE is not None:
        return "fbsource"
    return "oss"


def normalize_mode(args, parser):
    if args.mode and args.mode not in BUILD_MODES:
        choices = ", ".join(repr(mode) for mode in BUILD_MODES)
        parser.error(
            f"argument --mode: invalid choice: {args.mode!r} (choose from {choices})"
        )

    modes = []
    if args.mode:
        modes.append(args.mode)
    if args.oss:
        modes.append("oss")
    if args.getdeps:
        modes.append("getdeps")
    if len(set(modes)) > 1:
        parser.error("--mode, --oss, and --getdeps specify conflicting modes")
    args.mode = modes[0] if modes else auto_mode()


def normalize_rust_build_tool(args, parser):
    if args.rust_build_tool and args.rust_build_tool not in RUST_BUILD_TOOLS:
        choices = ", ".join(repr(tool) for tool in RUST_BUILD_TOOLS)
        parser.error(
            "argument --rust-build-tool: invalid choice: "
            f"{args.rust_build_tool!r} (choose from {choices})"
        )

    tools = []
    if args.rust_build_tool:
        tools.append(args.rust_build_tool)
    if args.buck:
        tools.append("buck")
    if args.cargo:
        tools.append("cargo")
    if len(set(tools)) > 1:
        parser.error("--rust-build-tool, --buck, and --cargo specify conflicting tools")
    args.rust_build_tool = (
        tools[0] if tools else ("buck" if args.mode == "fbsource" else "cargo")
    )

    if args.rust_build_tool == "buck" and args.mode != "fbsource":
        parser.error("--rust-build-tool buck requires --mode fbsource")


def normalize_args(args, parser):
    normalize_mode(args, parser)
    status(args, f"Using build mode (--mode): {args.mode}")
    normalize_rust_build_tool(args, parser)
    status(args, f"Using Rust build tool (--rust-build-tool): {args.rust_build_tool}")
    args.python = args.with_python or pick_python()
    status(args, f"Using Python: {args.python}")
    args.version = args.with_version or auto_version()
    status(args, f"Using Sapling version: {args.version}")


def main(argv):
    build_targets = ("sl", "isl")
    parser = argparse.ArgumentParser(
        description="Build sl and isl.",
        epilog=("Generated files live under out/."),
    )
    add_common_options(parser)
    parser.add_argument(
        "targets",
        metavar="TARGET",
        nargs="*",
        help="Targets to build. Choices: sl, isl. Defaults to both.",
    )

    args = parser.parse_intermixed_args(argv)
    invalid_targets = [target for target in args.targets if target not in build_targets]
    if invalid_targets:
        choices = ", ".join(repr(target) for target in build_targets)
        parser.error(
            "argument TARGET: invalid choice: "
            f"{invalid_targets[0]!r} (choose from {choices})"
        )
    normalize_args(args, parser)

    for target in args.targets or build_targets:
        if target == "sl":
            build_sl(args)
        elif target == "isl":
            build_isl(args)
        else:
            raise SystemExit(f"unknown target: {target}")


if __name__ == "__main__":
    main(sys.argv[1:])

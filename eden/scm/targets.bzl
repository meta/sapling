load("@fbcode_macros//build_defs:native_rules.bzl", "buck_genrule", "buck_sh_binary")
load("@fbcode_macros//build_defs/lib:rust_oss.bzl", "rust_oss")
load("@fbsource//tools/build_defs:rust_binary.bzl", "rust_binary")
load("@fbsource//tools/build_defs:rust_library.bzl", "rust_library")
load("@fbsource//tools/target_determinator/macros:ci_hint.bzl", "ci_hint")
load("@prelude//utils:buckconfig.bzl", "read_bool")

def _set_default(obj, *keys):
    for key in keys:
        obj[key] = obj.get(key) or {}
        obj = obj[key]
    return obj

_RUST_DEP_OVERRIDES = {
    "fs-err": {
        "features": [],
    },
    "futures": {
        "default-features": False,
        "features": ["std", "async-await"],
    },
    "indexmap": {
        "features": [],
    },
    "smallvec": {
        "default-features": False,
        "features": [],
    },
    "termwiz": {
        "default-features": False,
        "features": [],
        "git": None,
        "rev": None,
    },
    "twox-hash": {
        "default-features": False,
        "features": ["std", "xxhash64"],
    },
}

_RUST_PACKAGE_DEFAULTS = {
    "authors": ["Meta Source Control Team <sourcecontrol-dev@meta.com>"],
    "homepage": "https://sapling-scm.com/",
    "license": "MIT",
    "repository": "https://github.com/facebook/sapling",
}

_RUST_PACKAGE_VERSIONS = {
    crate: "0.2.0"
    for crate in [
        "sapling-atomicfile",
        "sapling-configmodel",
        "sapling-dag",
        "sapling-dag-types",
        "sapling-drawdag",
        "sapling-indexedlog",
        "sapling-lazystr",
        "sapling-mincode",
        "sapling-minibytes",
        "sapling-nonblocking",
        "sapling-rewrite-macros",
        "sapling-tree-pattern-match",
        "sapling-util",
        "sapling-vlqencoding",
    ]
}
_RUST_PACKAGE_VERSIONS.update({
    crate: "0.1.0"
    for crate in [
        "sapling-btrfs",
        "sapling-metrics",
        "sapling-pathmatcher",
        "sapling-pathmatcher-types",
        "sapling-small-set",
        "sapling-try-once-lock",
        "sapling-types",
    ]
})
_RUST_PACKAGE_VERSIONS.update({
    crate: "0.3.0"
    for crate in [
        "sapling-renderdag",
    ]
})

def debugcas_enabled():
    """Whether `sl.debugcas` adds `debugcas`, which links the C++ CAS client."""
    return read_bool("sl", "debugcas", False) and not rust_oss.is_oss_build()

def sl_cas_select(values):
    """Values for internal Linux builds of Sapling, which link the native CASd client."""
    if rust_oss.is_oss_build():
        return []
    return select({
        "DEFAULT": [],
        "ovr_config//os:linux": values,
    })

def edenfs_cas_select(values):
    if rust_oss.is_oss_build():
        return []
    return select({
        "DEFAULT": [],
        "fbcode//eden/scm/lib/backingstore:edenfs-cas[enabled]": select({
            "DEFAULT": [],
            "ovr_config//os:linux": values,
        }),
    })

def _minimal_third_party_rust_overrides(deps):
    deps_repr = repr(deps or [])
    overrides = {}
    for crate, override in _RUST_DEP_OVERRIDES.items():
        if "fbsource//third-party/rust:" + crate in deps_repr:
            overrides[crate] = dict(override)
    return overrides

def sl_autocargo_metadata(autocargo, lib_name_hint = None):
    """Apply shared publication defaults to an autocargo attribute.

    Package names default to sapling-<lib.name>; explicit names take precedence.
    lib_name_hint supplies the library name when lib.name is not configured.
    Defaults apply to Sapling's lib packages with autocargo configured.
    Vendored crates retain their upstream metadata.
    """
    package_path = native.package_name()
    if autocargo == None or not package_path.startswith("eden/scm/lib/") or "/third-party/" in package_path:
        return autocargo
    cargo_config = (autocargo or {}).get("cargo_toml_config") or {}
    package = cargo_config.get("package") or {}
    name = package.get("name")
    if name == None:
        lib_name = (cargo_config.get("lib") or {}).get("name") or lib_name_hint
        if lib_name != None:
            name = "sapling-" + lib_name.replace("_", "-")
    if name == None:
        return autocargo
    result = dict(autocargo)
    package = _set_default(result, "cargo_toml_config", "package")
    package.setdefault("name", name)
    for key, value in _RUST_PACKAGE_DEFAULTS.items():
        package.setdefault(key, value)
    # Cargo's include list takes precedence over exclude.
    if "include" not in package:
        package.setdefault("exclude", ["BUCK"])
    version = _RUST_PACKAGE_VERSIONS.get(name)
    if version != None:
        package.setdefault("version", version)
    return result

def _autocargo_overrides(lib_name_hint = None, **kwargs):
    """Apply shared package metadata and avoid unnecessary monorepo-wide features or git urls.

    For examples, turns generated Cargo.toml:

        smallvec = { version = "1.15.2", features = ["specialization", ...] }
        termwiz = { git = "...", rev = "...", features = ["use_serde", "widgets"], default-features = false }

    into:

        smallvec = { version = "1.15.2", default-features = false }
        termwiz = { version = "0.23.3", default-features = false }
    """
    autocargo = sl_autocargo_metadata(kwargs.get("autocargo"), lib_name_hint)
    if autocargo != None and autocargo.get("ignore_rule"):
        return autocargo

    cargo_config = (autocargo or {}).get("cargo_toml_config") or {}
    extra_deps = cargo_config.get("extra_buck_dependencies") or {}
    dependencies = _minimal_third_party_rust_overrides([
        kwargs.get("deps"),
        kwargs.get("named_deps"),
        extra_deps.get("dependencies"),
    ])
    dev_dependencies = _minimal_third_party_rust_overrides([
        kwargs.get("test_deps"),
        kwargs.get("test_named_deps"),
        extra_deps.get("dev-dependencies"),
    ])
    if not dependencies and not dev_dependencies:
        return autocargo

    result = dict(autocargo or {})
    _apply_autocargo_dep_overrides(result, "dependencies", dependencies)
    _apply_autocargo_dep_overrides(result, "dev-dependencies", dev_dependencies)
    return result

def _apply_autocargo_dep_overrides(autocargo, dep_kind, overrides):
    for crate, override in overrides.items():
        _set_autocargo_dep_override(autocargo, dep_kind, crate, override)

    if "termwiz" in overrides:
        # D105187099 switched fbsource termwiz to the wezterm git workspace.
        # Buck's wezterm-dynamic target enables std, but autocargo does not inherit that:
        #
        #     error[E0599]: no associated function or constant named `from_dynamic`
        #     found for struct `HashMap<K, V, S, A>` in the current scope
        #     --> termwiz-0.23.3/src/hyperlink.rs:19:39
        #
        # The missing HashMap impl is gated behind wezterm-dynamic/std. The published
        # wezterm-dynamic 0.2.1 does not have that feature, so disable it in OSS manifests.
        _set_autocargo_dep_override(
            autocargo,
            dep_kind,
            "wezterm-dynamic",
            {
                "default-features": False,
                "features": ["std"],
                "git": None,
                "rev": None,
            },
        )
        _set_autocargo_dep_override(
            autocargo,
            dep_kind,
            "wezterm-dynamic",
            {"features": []},
            override_kind = "oss_dependencies_override",
        )
        _add_extra_buck_dependency(autocargo, dep_kind, "fbsource//third-party/rust:wezterm-dynamic")

def _set_autocargo_dep_override(autocargo, dep_kind, crate, override, override_kind = "dependencies_override"):
    dep = _set_default(
        autocargo,
        "cargo_toml_config",
        override_kind,
        dep_kind,
        crate,
    )
    for key, value in override.items():
        dep.setdefault(key, value)

def _add_extra_buck_dependency(autocargo, dep_kind, dep):
    extra = _set_default(autocargo, "cargo_toml_config", "extra_buck_dependencies")
    deps = extra.get(dep_kind, [])
    if dep not in deps:
        extra[dep_kind] = deps + [dep]

def sl_rust_library(**kwargs):
    autocargo = _autocargo_overrides(lib_name_hint = kwargs.get("crate") or kwargs.get("name"), **kwargs)
    if autocargo != None:
        kwargs["autocargo"] = autocargo
    # avoid nightly features
    kwargs["rustc_flags"] = (kwargs.get("rustc_flags") or []) + ["-Funstable-features"]
    return rust_library(**kwargs)

def sl_rust_binary(embeds_python = False, **kwargs):
    autocargo = _autocargo_overrides(**kwargs)
    if autocargo != None:
        kwargs["autocargo"] = autocargo
    if embeds_python:
        kwargs["deps"] = _with_libpython(kwargs.get("deps"))
    return rust_binary(**kwargs)

# An embedder starts its own interpreter, so unlike an extension module it has no
# host to borrow the Python C API from and needs the real libpython. The
# `python3-sys` fixup deliberately supplies only `:python`, which on macOS resolves
# the C API at extension-load time; embedders opt in to the real dylib here.
# Concatenate rather than `list()`: `deps` may be a `select()`.
def _with_libpython(deps):
    libpython = ["fbsource//third-party/python:python-for-embedding"]
    return libpython if deps == None else deps + libpython

def exec_compatible_with_target():
    """Intended to be used by genrule's exec_compatible_with to force
    execution platform to match target platform. Without this, one
    might get "exec format error" when target platform (ex. macos)
    binary is run on incompatible execution platform (ex. linux).

    This function provides a shorter way for code like:

        exec_compatible_with = select({
            "ovr_config//os:linux": ["ovr_config//os:linux"],
            "ovr_config//os:macos": ["ovr_config//os:macos"],
            ...
            "DEFAULT": [],
        }) + select({
            "ovr_config//cpu:x86_64": ["ovr_config//cpu:x86_64"],
            "ovr_config//cpu:arm64": ["ovr_config//cpu:arm64"],
            ...
            "DEFAULT": [],
        })
    """

    # To list low-level constraints (could be noisy), use:
    #     buck uquery "attrfilter(constraint_setting, 'ovr_config//os/constraints:os', deps('ovr_config//os/constraints:'))"
    #     buck uquery "attrfilter(constraint_setting, 'ovr_config//cpu/constraints:cpu', deps('ovr_config//cpu/constraints:'))"
    # To list high-level constraints (that might mix OS and CPU like "windows-arm64"), use:
    #     buck uquery "deps('ovr_config//os:', 0)"
    #     buck uquery "deps('ovr_config//cpu:', 0)"
    os_list = ["emscripten", "freebsd", "linux", "macos", "wasi", "windows"]
    cpu_list = ["arm", "arm32", "arm64", "riscv32", "riscv64", "wasm32", "x86_32", "x86_64", "xtensa"]

    def select_with(prefix, names):
        choices = {"DEFAULT": []}
        for name in names:
            full_name = prefix + name
            choices[full_name] = [full_name]
        return select(choices)

    return select_with("ovr_config//os/constraints:", os_list) + select(
        {"DEFAULT": select_with("ovr_config//cpu/constraints:", cpu_list), "ovr_config//os/constraints:macos": []}
    )

def rust_python_library(deps = None, include_python_sys = False, include_cpython = True, pyo3 = False, **kwargs):
    # Python 3 target
    kwargs3 = dict(kwargs)

    deps3 = list(deps or [])
    if include_cpython:
        deps3.append("fbsource//third-party/rust:cpython")
        asan_opts = kwargs3.setdefault("test_env", {}).setdefault("ASAN_OPTIONS", "")
        if asan_opts:
            kwargs3["test_env"]["ASAN_OPTIONS"] += ":use_sigaltstack=0"
        else:
            kwargs3["test_env"]["ASAN_OPTIONS"] = "use_sigaltstack=0"
        cpython = _set_default(kwargs3, "autocargo", "cargo_toml_config", "dependencies_override", "dependencies", "cpython")
        cpython["features"] = cpython.get("features") or ["python3-sys"]
    if include_python_sys:
        deps3.append("fbsource//third-party/rust:python3-sys")
    if pyo3:
        deps3.append("fbsource//third-party/rust:pyo3")

    # The generated `-unittest` binary runs standalone, with no interpreter to
    # borrow the Python C API from, so it embeds CPython and needs the real
    # libpython. Scoping this to `test_deps` keeps the library itself -- and the
    # extensions built from it -- on the extension-safe `:python`.
    if include_cpython or include_python_sys or pyo3:
        kwargs3["test_deps"] = _with_libpython(kwargs3.get("test_deps"))

    kwargs3["name"] = kwargs["name"]
    kwargs3["crate"] = kwargs["name"].replace("-", "_")
    kwargs3["deps"] = deps3
    sl_rust_library(**kwargs3)

def gen_slpython(sl_target, suffix = ""):
    if read_bool("fbcode", "mode_win_enabled", False) and "ovr_config//os:windows":
        return buck_genrule(
            name = "slpython" + suffix,
            out = "python.exe",
            bash = "ln -s $(location " + sl_target + ") $OUT",
            cmd_exe = "mklink $OUT $(location :sl)",
            executable = True,
        )

    # We cannot quite use symlinks outside of Windows since the `dev-nosan-lg` mode is
    # used sometimes, and that copies the binary into another location rather
    # than actually creating a symlink like in other modes for some reason.
    return buck_sh_binary(
        name = "slpython" + suffix,
        main = "run_buck_hgpython.sh",
        resources = [
            sl_target,
        ],
    )

def fetch_as_eden():
    return read_bool("sl", "fetch_as_eden", False)

def sl_binary(name, extra_deps = [], extra_features = [], **kwargs):
    kwargs.setdefault(
        "allocator",
        select({
            "DEFAULT": "malloc",
            "ovr_config//os:linux": "jemalloc",
            "ovr_config//os:macos": "jemalloc",
        }),
    )
    sl_rust_binary(
        name = name,
        embeds_python = True,
        srcs = glob(["exec/hgmain/src/**/*.rs"]),
        features = [
            "fb",
        ]
        + extra_features,
        link_style = "static",
        linker_flags = select({
            "DEFAULT": [],
            "ovr_config//os:windows": [
                "/MANIFEST:EMBED",
                "/MANIFESTINPUT:$(location :windows-manifest)",
            ],
        }),
        # hgmain should be buildable by stable Rust (via cargo). Disallow nightly Rust features.
        rustc_flags = ["-Zallow-features="],
        deps = [
            "fbsource//third-party/rust:tracing",
            "//eden/scm/lib/clidispatch:clidispatch",
            "//eden/scm/lib/config/loader:configloader",
            "//eden/scm/lib/commands:commands",
            "//eden/scm/lib/util/atexit:atexit",
        ]
        + (
            []
            if rust_oss.is_oss_build()
            else [
                "//scm/telemetry/distributed-tracing-artillery:distributed-tracing-artillery",
            ]
        )
        + extra_deps
        + select({
            "DEFAULT": [],
            "ovr_config//os:linux": [
                "fbsource//third-party/rust:libc",
                "//eden/scm/lib/hg-http:hg-http",
            ],
            "ovr_config//os:macos": [
                "fbsource//third-party/rust:libc",
                "//eden/scm/lib/webview-app:webview-app",
            ],
            "ovr_config//os:windows": [
                "fbsource//third-party/rust:anyhow",
                "fbsource//third-party/rust:winapi",
            ],
        }),
        **kwargs,
    )

    # Try to override target depth so //eden/scm/tests:sl_run_tests and other
    # important test targets reliably pick up Python code changes despite target
    # depth greater than 4.
    ci_hint(
        ci_deps = ["fbcode//eden/scm/lib/python-modules:python-modules"],
        reason = "sl is very close to Python source files despite large target depth",
        target = name,
    )

# Backwards-compatible aliases
hg_binary = sl_binary

def gen_hgpython(hg_target, suffix = ""):
    """Backwards-compatible alias that generates hgpython-named targets."""
    if read_bool("fbcode", "mode_win_enabled", False) and "ovr_config//os:windows":
        return buck_genrule(
            name = "hgpython" + suffix,
            out = "python.exe",
            bash = "ln -s $(location " + hg_target + ") $OUT",
            cmd_exe = "mklink $OUT $(location :hg)",
            executable = True,
        )

    return buck_sh_binary(
        name = "hgpython" + suffix,
        main = "run_buck_hgpython.sh",
        resources = [
            hg_target,
        ],
    )

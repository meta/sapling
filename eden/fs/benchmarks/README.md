# "Macro" Benchmarks

This directory contains benchmarks of EdenFS through its filesystem and Thrift
APIs. Several of these benchmarks allow comparison of EdenFS's performance to
native filesystems.

## Concurrent reads of many small files

`eden debug bench fs-io --file-list` opens, reads and closes every path in a
list with many threads, and reports per-call latencies, files/s and MiB/s.
With a list of the files a build reads, it measures how fast EdenFS serves
small-file reads, and which of its locks limit that. Against an EdenFS
checkout it also reports the daemon's request counts, object store fetches,
and CPU time.

Each open goes back to EdenFS only when the kernel's cached entry and
attributes for the path have expired. To measure that case, shorten the FUSE
TTLs in the instance's `.edenrc` (values are quoted strings) and restart
EdenFS:

```
[mount]
fuse-ttl-min-seconds = "3"
fuse-ttl-max-seconds = "3"
```

Then run a warm-up pass, wait longer than the TTL, and run the measured pass
without dropping the kernel caches in between:

```
eden debug bench fs-io --file-list=reads.txt --jobs=64 --drop-kernel-caches=0
sleep 5
eden debug bench fs-io --file-list=reads.txt --jobs=64 --drop-kernel-caches=0 --json
```

To use FUSE over io_uring, also set `[fuse]` `use-io-uring = "true"` and an
`io-uring-kernel-release-regex` that matches the running kernel.

## Continuous runs in ServiceLab

`servicelab/` registers the ServiceLab benchmark `benchmark_edenfs_fsio`
(`fbcode/eden/fs/benchmarks/facebook/BUCK`). Its fbpkg carries `edenfs`
and both `edenfsctl` CLIs built at the revision under test; the benchmark
starts a private daemon from them in the ServiceLab container, clones
fbsource at a pinned commit, and runs the fs-io workloads in the clone. Each
fs-io phase's timings and the daemon's counters become benchmark metrics.
The Tupperware side of the job is `fbcode/tupperware/config/scm/eden/fsio_benchmark.cinc`.

Run it locally in a Source Control container (one made by
`scm-eden-container-clone`, such as an agent FaaS container) with the binaries
in one directory:

```
buck2 build @fbcode//mode/opt //eden/fs/service:edenfs //eden/fs/cli_rs/edenfsctl:edenfsctl //eden/fs/cli:edenfsctl //eden/fs/benchmarks/facebook:edenfs_fsio
# copy edenfs, edenfsctl, and the Python CLI as edenfsctl.real into $PKG
EDENFS_FSIO_PACKAGE_DIR=$PKG EDENFS_FSIO_WORK_DIR=$HOME/fsio_work \
  buck-out/.../edenfs_fsio.par --bm_regex 'clean_reads$' --json
```

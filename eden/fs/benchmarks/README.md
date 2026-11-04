# "Macro" Benchmarks

This directory contains benchmarks of EdenFS through its filesystem and Thrift
APIs. Several of these benchmarks allow comparison of EdenFS's performance to
native filesystems.

## Concurrent reads of many small files

`read_files_parallel` opens, reads and closes every path in a list with many
threads, and reports files/s and MB/s per pass. With a list of the files a
build reads, it measures how fast EdenFS serves small-file reads, and which of
its locks limit that.

Each open goes back to EdenFS only when the kernel's cached entry and
attributes for the path have expired. To measure that case, shorten the FUSE
TTLs in the instance's `.edenrc` (values are quoted strings) and restart
EdenFS:

```
[mount]
fuse-ttl-min-seconds = "3"
fuse-ttl-max-seconds = "3"
```

Then run a warm-up pass and measured passes with a wait longer than the TTL
between them:

```
read_files_parallel --file_list=reads.txt --threads=64 --passes=3 --wait_between_passes_seconds=5
```

To use FUSE over io_uring, also set `[fuse]` `use-io-uring = "true"` and an
`io-uring-kernel-release-regex` that matches the running kernel.

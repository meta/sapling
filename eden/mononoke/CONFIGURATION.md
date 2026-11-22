# CONFIGURATION.md \- Mononoke Open Source Configuration Guide

## TLDR

This guide provideshigh level configuration instructions for deploying Mononoke
in open source environment for experimentation.

Key highlights:

- **TOML Configuration**: Mononoke uses TOML files for configuration with
  separate storage and repository settings
- **Storage Architecture**: MySQL for metadata \+ S3-compatible storage (like
  RustFS) for blob data
- **Build System**: Use `sapling cli getdeps build` for cross-platform builds
  and dependency management
- **Repository Import**: Use `gitimport` tool to populate Mononoke with existing
  Git repositories like buck2
- **OSS Limitations**: Some Facebook-internal features are not available in open
  source deployments
- **Local Development**: Supports local development with SQLite and file-based
  storage

## Mononoke TOML Configuration Structure

Mononoke uses TOML configuration files for human-readable, maintainable
configuration. The configuration is split into several files:

### Configuration File Structure

```
# config/common/common.toml - Common settings
[internal_identity]
identity_type = "USER"
identity_data = "your-username"

[[global_allowlist]]
identity_type = "USER"
identity_data = "your-username"
```

### Repository Configuration

```
# config/repos/repo.toml - Repository-specific settings
[[repos]]
name = "my-repo"
config = "default_config"
repoid = 1

[storage]
mysql = { connection_string = "mysql://user:pass@localhost:3306/mononoke" }
blobstore = "compressed_blobstore"

[blobstore.compressed_blobstore]
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "s3",
    manifold_bucket = "mononoke-blobs",
    s3_region = "us-west-2",
    s3_bucket = "my-mononoke-blobs"
}
```

### Multiplexed Blobstore Configuration

```
# For redundancy across multiple storage backends
[blobstore.multiplexed]
type = "packblob"
zstd_level = 3
format = "zstd"
write_quorum = 2
read_preference = ["primary", "secondary"]
inner = { type = "multiplexed", components = [] }

[[blobstore.multiplexed.inner.components]]
id = 1
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "s3",
    config = { bucket = "primary-bucket", region = "us-west-2" }
}

[[blobstore.multiplexed.inner.components]]
id = 2
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "s3",
    config = { bucket = "backup-bucket", region = "us-east-1" }
}
```

Obviously, deployments cannot use meta-internal storage systems like Manifold
and must configure alternative backends.

## MySQL Database Setup

MySQL serves as the metadata store for mutable data including bookmarks, phases,
and mappings.

### Database Schema Setup

**1\. Create Database and User**

```sql
CREATE DATABASE mononoke_metadata CHARACTER SET utf8mb4 COLLATE utf8mb4_bin;
CREATE USER 'mononoke'@'%' IDENTIFIED BY 'secure_password';
GRANT ALL PRIVILEGES ON mononoke_metadata.* TO 'mononoke'@'%';
FLUSH PRIVILEGES;
```

**2\. Configuration in TOML**

```
[storage.metadata]
db_address = "mysql://mononoke:secure_password@localhost:3306/mononoke_metadata"
connection_pool_size = 10
connection_timeout_ms = 5000

# Optional: Use MySQL client for compatibility
use_mysql_client = true
```

**3\. Connection Parameters**

For external deployments, you may need additional connection parameters:

```
[storage.metadata.mysql]
host = "localhost"
port = 3306
user = "mononoke"
password = "secure_password"
database = "mononoke_metadata"
ssl_mode = "PREFERRED"
max_connections = 50
```

### Schema Considerations

- **File Metadata**: Stores repo ID, file type, filename, file node hash, and
  linknode mappings
- **Normalization**: Use separate tables for 1:1 vs. 1:many relationships
- **Indexing**: Primary keys based on (repo_id, file_type, filename,
  file_node_hash)
- **Storage Engine**: MyRocks recommended for write-heavy workloads, InnoDB for
  read-heavy

**Storage Capacity Planning**: Aim for \<1TB per MySQL shard. For larger
datasets, consider sharding by repository or content hash ranges.

## S3-Compatible Storage Configuration with RustFS

Configure S3-compatible storage for blob data using RustFS or other
S3-compatible object stores.

### RustFS Setup

[RustFS](https://github.com/rustfs/rustfs) is an open source S3-compatible
object storage system written in Rust. If you'd like to try out S3 storage
without cloud accounts/costs, it's a good starting point.

See its [installation guide](https://docs.rustfs.com/installation/) for
quickstart instructions. In its default developer setup, it runs on
`localhost:9000` and writes to /data.

### Mononoke S3 Blobstore Configuration

```
[blobstore.s3_primary]
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "s3",
    bucket = "mononoke-blobs",
    region = "us-west-2",
    endpoint = "http://localhost:9000",  # For RustFS
    access_key = "my_key",
    secret_key = "my_key123",
    max_connections = 100,
    connection_timeout = "30s",
    request_timeout = "300s",
    server_side_encryption = "AES256"
}
```

### AWS S3 Configuration

For production deployments with real AWS S3:

```
[blobstore.aws_s3]
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "s3",
    bucket = "my-mononoke-production",
    region = "us-west-2",
    access_key = "my_key",
    secret_key = "my_key_secret",
    max_connections = 200,
    connection_timeout = "10s",
    request_timeout = "120s",
    server_side_encryption = "aws:kms",
    sse_kms_key_id = "arn:aws:kms:us-west-2:123456789:key/..."
}
```

### Multiplexed S3 Configuration

```
[blobstore.multiplexed_s3]
type = "packblob"
zstd_level = 3
format = "zstd"
inner = {
    type = "multiplexed",
    write_quorum = 2,
    components = [
        {
            id = 1,
            type = "packblob",
            zstd_level = 3,
            format = "zstd",
            inner = {
                type = "s3",
                config = { bucket = "primary-bucket", region = "us-west-2" }
            }
        },
        {
            id = 2,
            type = "packblob",
            zstd_level = 3,
            format = "zstd",
            inner = {
                type = "s3",
                config = { bucket = "backup-bucket", region = "us-east-1" }
            }
        }
    ]
}
```

**Performance Note**: S3-compatible storage provides \~200-500ms read latency.
Configure appropriate connection pooling and timeouts based on your deployment
needs.

**Warning:** Mononoke will _ignore_ and may _fail to start_ if you include
unsupported fields like `server_side_encryption`, `sse_kms_key_id`, or any
encryption-related options in your blobstore configuration blocks.
Encryption/Key Management _must_ be performed at the S3 bucket level. Always
consult your storage provider's documentation to enable default encryption and
KMS policies as required by your organization.

## Git LFS Server Configuration

Mononoke can serve as its own Git LFS server, allowing you to store and serve
large files directly from your Mononoke instance.

### Enable LFS in your config:

```
[lfs]
enabled = true
# Optional: proxy to another LFS server for missing blobs
# upstream_server = "https://your-upstream-lfs-server.example.com/lfs"
tasks_per_content = 20
cache_size = "150GiB"
```

### Running the LFS server:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && mononoke_admin lfs run --mononoke-config-path /path/to/your/config.toml --skip-authorization --filter-repos my-repo
```

### Client usage:

- By default, Git LFS will use the same base URL as your Mononoke Git server.
- No need to set `.lfsconfig` unless you want to override the default.

### Importing with LFS:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport --repo-name my-repo --config-path /path/to/mononoke/config --lfs-server https://your-mononoke-server.example.com/lfs ...
```

### Testing the LFS server:

You can test the LFS server with a batch request:

```shell
curl -k https://your-mononoke-server.example.com/<repo>/objects/batch/ \
     --data-binary "@/tmp/request.json"
```

Where `/tmp/request.json` contains:

```json
{
  "operation": "download",
  "transfers": ["basic"],
  "objects": [
    {
      "oid": "1cc6aac3e57118d63fa4e408693d97f8e5b91708b2469b5451a928c1ed185f3f",
      "size": 84744740
    }
  ]
}
```

## Sapling CLI and Getdeps Build Process

Build Mononoke using the getdeps build system and connect with Sapling CLI.

### Installing Prerequisites

**System Dependencies Installation**:

To install system dependencies required for building Mononoke, use the getdeps
script.

Execute the following commands in the terminal for a unified approach across
different systems:

```shell
./build/fbcode_builder/getdeps.py install-system-deps --recursive mononoke

./build/fbcode_builder/getdeps.py install-system-deps --recursive sapling
```

### Building with Getdeps

**1\. Clone the Repository**

```shell
git clone https://github.com/facebook/sapling.git
cd sapling
```

**2\. Build Mononoke with Getdeps**

```shell
# Install system dependencies
sudo ./build/fbcode_builder/getdeps.py install-system-deps --recursive mononoke

# Build Mononoke (takes 30-60 minutes)
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --allow-system-packages mononoke

# Build specific components
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --allow-system-packages eden_scm
```

**3\. Build Configuration Options**

```shell
# Debug build
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --mode debug mononoke

# Release build (recommended for production)
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --mode release mononoke

# With additional features
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --allow-system-packages --shared-libs mononoke
```

### Sapling CLI Installation

**Build from Source**:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); ./build/fbcode_builder/getdeps.py build --allow-system-packages --mode release sapling
```

### Connecting Sapling to Mononoke

```shell
# Clone from Mononoke server
sl clone mononoke://localhost:8080/my-repo

# Configure Sapling for Mononoke
sl config --user paths.default mononoke://your-server:8080/repo-name
sl config --user edenapi.url https://your-server:8080/edenapi

# Basic operations
sl pull
sl push
sl status
```

**Connection Configuration**:

```shell
# ~/.sapling/config
[paths]
default = mononoke://mononoke.example.com:8080/my-repo

[edenapi]
url = https://mononoke.example.com:8080/edenapi

# Authentication (if required)
[auth]
mononoke.cert = /path/to/client.pem
mononoke.key = /path/to/client.key
```

## Gitimport Tool Usage and Repository Population

Use Mononoke's gitimport tool to import existing Git repositories and populate
storage.

### Basic Gitimport Usage

**Command Structure**:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport [OPTIONS] <GIT_REPO_PATH> full-repo
```

### Importing the buck2 Repository

**1\. Clone the Source Repository**

```shell
mkdir ~/git-repos
cd ~/git-repos
git clone https://github.com/facebook/buck2.git
```

**2\. Import into Mononoke**

```shell
# Basic import
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport --repo-name buck2 --generate-bookmarks ~/git-repos/buck2 full-repo
```

### Advanced Import with S3 and MySQL

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport --repo-name buck2 --config-path /path/to/mononoke/config --bypass-readonly --generate-bookmarks --concurrency 100 --lfs-server https://your-lfs-server.com/lfs --allow-dangling-lfs-pointers --manifold-write-retries 3 ~/git-repos/buck2 full-repo
```

### Import Configuration Options

**Essential Flags**:

- `--repo-name`: Target repository name in Mononoke
- `--generate-bookmarks`: Create bookmarks from Git branches
- `--bypass-readonly`: Allow writes during import
- `--concurrency N`: Number of parallel operations

**LFS Support**:

- `--lfs-server URL`: LFS server for large file support
- `--allow-dangling-lfs-pointers`: Continue on missing LFS objects
- `--lfs-import-max-attempts N`: Retry failed LFS downloads

**Performance Tuning**:

- `--manifold-write-retries N`: Retry failed storage writes
- `--concurrency N`: Parallel processing (start with 100-500)

### Repository Import Examples

**Large Repository with LFS**:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport --repo-name large-project --generate-bookmarks --concurrency 500 --lfs-server https://lfs.example.com/lfs --allow-dangling-lfs-pointers --manifold-write-retries 5 ~/repos/large-project full-repo
```

**Import Specific Branches**:

```shell
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && ./gitimport --repo-name project --include-refs refs/heads/main --include-refs refs/heads/develop --generate-bookmarks ~/repos/project full-repo
```

### Verification After Import

```shell
# Check imported bookmarks
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && mononoke_admin bookmarks list --repo-name buck2

# Verify specific bookmark
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && mononoke_admin bookmarks get heads/main --repo-name buck2

# Check derived data
source <(./build/fbcode_builder/getdeps.py --allow-system-packages env mononoke); cd $GETDEPS_INSTALL_DIR/mononoke && mononoke_admin derived-data exists --repo-name buck2 --bookmark heads/main
```

**Import Progress Monitoring**: Large repositories can take hours to days.
Monitor logs for progress indicators and memory usage. Consider importing in
chunks for very large repositories (\>1M commits).

## Open Source Deployment Considerations

Key differences and limitations when deploying Mononoke outside Facebook's
infrastructure.

### Limitations

**Missing Facebook-Internal Features**:

- **Manifold**: Meta's internal object storage (use S3-compatible alternatives)
- **XDB**: Internal database sharding system (use standard MySQL)
- **Cachelib**: 's caching library is not connect (impacts performance).
  Cachelib is available in OSS if you wish to try integrating it.
- **Internal Authentication**: Use external auth systems or disable auth for
  development

### Storage Backend Differences

**Internal vs External Storage**:

```
# Meta Internal example (NOT available in OSS)
[storage]
blobstore = "manifold"
metadata = "xdb.mononoke_metadata_db"

# External OSS Configuration
[storage]
blobstore = "s3_compatible"
metadata = "mysql://user:pass@host:3306/db"
```

### Authentication and Authorization

Beyond local experimentation you'll need to protect your Mononoke instance
behind an authenticating proxy.

There are some internal auth config options that you may need to disable for
development:

```
[internal_identity]
identity_type = "USER"
identity_data = "admin"

[[global_allowlist]]
identity_type = "USER"
identity_data = "admin@example.com"
```

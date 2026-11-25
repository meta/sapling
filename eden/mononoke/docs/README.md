# Mononoke Documentation

## About Mononoke

Mononoke is a scalable source control server designed to handle massive repositories with millions of files, thousands of commits per day, and hundreds of thousands of branches. As the server-side component of Meta's Sapling source control ecosystem, Mononoke works together with EdenFS (a virtual filesystem) and the Sapling CLI to provide version control operations at large scale.

Unlike traditional version control servers that are tied to a specific VCS, Mononoke uses a canonical, VCS-agnostic data model called Bonsai as its single source of truth. This enables Mononoke to serve both Sapling and Git clients from the same backend, converting between formats as needed while maintaining consistency. The architecture separates write operations from read operations, computes expensive indexes asynchronously, and uses horizontal scaling to handle load.

For a complete introduction, see [What is Mononoke?](1.1-what-is-mononoke.md).

## About This Documentation

This documentation provides high-level architectural and conceptual information about Mononoke for developers, operators, and AI agents. It covers core architectural patterns, major components (servers, jobs, tools), key features, VCS integration, and operational aspects.

**Not covered here:** Build/test workflows are in [AGENTS.md](../AGENTS.md). API documentation is in rustdoc comments. Detailed operational procedures are in component-specific directories.

## Getting Started

**New to Mononoke?** Start with Section 1 (Introduction).

**Building or developing?** See [AGENTS.md](../AGENTS.md) for build instructions and testing procedures.

**Looking for specific topics?** Use the table of contents below.

## Documentation Contents

### 1. Introduction

*Core concepts and orientation for understanding Mononoke*

**[1.1 - What is Mononoke?](1.1-what-is-mononoke.md)**
Mononoke's role as a scalable source control server, its place in the Sapling ecosystem, design goals, and deployment scenarios.

**[1.2 - Key Concepts](1.2-key-concepts.md)**
Essential concepts: Bonsai (canonical data model), content addressing (hash-based storage), repository facets (trait-based composition), derived data (asynchronous indexes), blobstore (immutable storage), metadata database, and VCS mappings.

**[1.3 - Architecture Overview](1.3-architecture-overview.md)**
System architecture (how services are composed) and code architecture (how applications are structured). Covers data flow, write vs. read paths, and how components fit together.

**[1.4 - Navigating the Codebase](1.4-navigating-the-codebase.md)**
Guide to finding components in 70+ directories. Covers directory organization, BUCK files, and how to locate implementations and tests.

### 2. Architecture

*Deep dive into Mononoke's data model, patterns, and storage design*

**[2.1 - Bonsai Data Model](2.1-bonsai-data-model.md)**
Mononoke's core data model: what Bonsai is, how it represents commits and file changes, content addressing, and how it enables multi-VCS support.

**[2.2 - Repository Facets](2.2-repository-facets.md)**
The facet pattern used throughout Mononoke. Covers major facet categories (identity, storage, commit graph, derived data, VCS mappings, bookmarks, operations) and how features compose facets.

**[2.3 - Derived Data](2.3-derived-data.md)**
The derived data framework: what derived data is, why it's computed off the write path, the derivation process, major types, and remote derivation.

**[2.4 - Storage Architecture](2.4-storage-architecture.md)**
How Mononoke stores and caches data. Covers blobstore architecture (backends, decorator pattern), metadata database, caching strategy, and packblob compression.

### 3. Components

*Servers, background jobs, command-line tools, and shared libraries*

**[3.1 - Servers and Services](3.1-servers-and-services.md)**
Main protocol servers (Mononoke/SLAPI, SCS, Git, LFS) and internal microservices (Land, Derived Data, Bookmark, Diff, Load Limiter).

**[3.2 - Jobs and Background Workers](3.2-jobs-and-background-workers.md)**
Background maintenance tasks: Walker (graph validation), Blobstore Healer (storage durability), Derivation Worker, Cross-Repo Sync, and Statistics Collector.

**[3.3 - Tools and Utilities](3.3-tools-and-utilities.md)**
Command-line tools: admin CLI (primary tool), import/export tools (gitimport), verification tools (aliasverify), and maintenance utilities (packer, sqlblob_gc).

**[3.4 - Libraries and Frameworks](3.4-libraries-and-frameworks.md)**
Shared libraries: cmdlib/mononoke_app framework for new binaries, common utilities (async, SQL, logging), core types, and testing utilities.

### 4. Features

*Key source control operations and workflows*

**[4.1 - Pushrebase](4.1-pushrebase.md)**
Server-side rebasing for maintaining linear history at scale. Covers the pushrebase process, conflict detection, hooks integration, and use in Sapling and Git workflows.

**[4.2 - Cross-Repo Sync](4.2-cross-repo-sync.md)**
Repository synchronization between large and small repos. Covers sync patterns, commit transformation, and sync job operation.

**[4.3 - Hooks](4.3-hooks.md)**
Policy enforcement at push time. Covers hook types (bookmark, changeset, file), configuration, execution, and the hook manager facet.

**[4.4 - Redaction](4.4-redaction.md)**
Content redaction for removing sensitive data. Covers how redaction works at the blobstore level and access control.

### 5. VCS Integration

*Support for Git, Mercurial, and Sapling clients*

**[5.1 - Git Support](5.1-git-support.md)**
Git protocol server, Bonsai ↔ Git conversion, Git-specific derived data, reference handling, LFS integration, and source of truth tracking.

**[5.2 - Mercurial and Sapling Support](5.2-mercurial-sapling-support.md)**
Historical context, Bonsai ↔ Mercurial conversion, wire protocol, Mercurial-specific derived data, EdenAPI protocol, and compatibility requirements.

### 6. Operations

*Operational concerns: performance, validation, and observability*

**[6.1 - Rate Limiting and Load Shedding](6.1-rate-limiting-and-load-shedding.md)**
Load management strategies, the Load Limiter service, QPS limits, and load shedding under pressure.

**[6.2 - Walker and Validation](6.2-walker-and-validation.md)**
Graph traversal and validation tool. Covers scrubbing (validation and repair), corpus generation, and compression analysis.

**[6.3 - Monitoring and Observability](6.3-monitoring-and-observability.md)**
Metrics (ODS/stats), logging (Scuba), tracing, dashboards, health checks, and performance indicators.

**[6.4 - SCS Reduced Availability Workarounds](6.4-scs-reduced-availability.md)**
How to work around SCS reduced availability when downstream consequences would be SEV1 or above. Covers immediate mitigation actions, alternative access paths, and guidance for downstream service owners.

### Appendix A. Future Improvements

**[A.1 - Better Engineering](A.1-better-engineering.md)**
A living document tracking long-term engineering improvements and technical debt that could be addressed in the codebase.

## Additional Resources

**Development:**
- [AGENTS.md](../AGENTS.md) - Build commands, tests, build modes, development patterns

**Code:**
- Mononoke codebase: `fbcode/eden/mononoke/`
- Open source: [Sapling on GitHub](https://github.com/facebook/sapling)

**Related Projects:**
- EdenFS: `fbcode/eden/fs/` - Virtual filesystem
- Sapling CLI: `fbcode/eden/scm/` - Command-line interface
- Eden project docs: `fbcode/eden/AGENTS.md`

**External:**
- Sapling website: https://sapling-scm.com/
- Oncall: `scm_server_infra`

## Contributing

Update these docs for architectural changes, new components, or component reorganization. Do not update for implementation details, bug fixes, or configuration changes—those belong in component directories, rustdoc, or AGENTS.md.

Maintain a professional, neutral, factual tone. Keep content high-level, cross-reference liberally, and verify accuracy against the code.

---

For questions, contact the `scm_server_infra` oncall.

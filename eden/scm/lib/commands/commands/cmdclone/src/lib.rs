/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::anyhow;
use async_runtime::block_on;
use clidispatch::ReqCtx;
use clidispatch::TermLogger;
use clidispatch::abort;
use clidispatch::abort_if;
use clidispatch::errors;
use clidispatch::errors::triage_error;
use clidispatch::fallback;
use cmdpy::HgPython;
use cmdutil::ConfigSet;
use cmdutil::Result;
use cmdutil::define_flags;
use configloader::hg::PinnedConfig;
use configloader::hg::RepoInfo;
use configmodel::Config;
use configmodel::ConfigExt;
use configmodel::Text;
use configmodel::ValueSource;
use eagerepo::EagerRepo;
use filters::util::filter_paths_from_config;
use hgrc_parser::format::Error as ConfigFormatError;
use hgrc_parser::format::format_config_include;
use hgrc_parser::format::format_config_section;
use migration::feature::deprecate;
use regex::Regex;
use repo::repo::Repo;
use repourl::RepoUrl;
use repourl::encode_repo_name;
use tracing::instrument;
use types::HgId;
use url::Url;
use util::path::absolute;

static COMMIT_GRAPH_SEGMENTS_CAPABILITY: &str = "commit-graph-segments";
static GIT_FORMAT_CAPABILITY: &str = "git-format";

define_flags! {
    pub struct CloneOpts {
        /// clone an empty working directory
        #[short('U')]
        noupdate: bool,

        /// revision or branch to check out
        #[short('u')]
        #[argtype("REV")]
        updaterev: String,

        /// use remotefilelog (has no effect) (DEPRECATED)
        shallow: bool = true,

        /// use git protocol (EXPERIMENTAL)
        git: bool,

        /// enable a sparse profile
        enable_profile: Vec<String>,

        /// files to include in a sparse profile (DEPRECATED)
        include: String,

        /// files to exclude in a sparse profile (DEPRECATED)
        exclude: String,

        /// use EdenFS (EXPERIMENTAL)
        eden: Option<bool>,

        /// location of the backing repo to be used or created (EXPERIMENTAL)
        eden_backing_repo: String,

        /// configure repo to run against AWS (EXPERIMENTAL) (FBCODE)
        aws: bool,

        #[arg]
        source: String,

        #[args]
        args: Vec<String>,
    }
}

impl CloneOpts {
    fn source(&self, config: &dyn Config) -> Result<RepoUrl> {
        if let Ok(Some(abs_path)) = local_path(&self.source) {
            RepoUrl::from_str(
                config,
                abs_path
                    .to_str()
                    .ok_or_else(|| anyhow!("invalid source path {}", self.source))?,
            )
        } else {
            RepoUrl::from_str(config, &self.source)
        }
    }

    fn eden(&self, config: &dyn Config) -> Result<bool> {
        if let Some(eden) = self.eden {
            return Ok(eden);
        }
        Ok(config.get_or_default("clone", "use-eden")?)
    }
}

fn local_path(s: &str) -> Result<Option<PathBuf>> {
    if looks_like_windows_path(s)
        || matches!(Url::parse(s), Err(url::ParseError::RelativeUrlWithoutBase))
    {
        Ok(Some(util::path::absolute(s)?))
    } else {
        Ok(None)
    }
}

fn looks_like_windows_path(s: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }

    // UNC prefix
    if s.starts_with(r"\\") {
        return true;
    }

    // Drive prefix (e.g. "c:")
    let bytes = s.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn log_clone_info(clone_type_str: &str, reponame: &str, ctx: &ReqCtx<CloneOpts>) {
    tracing::debug!(target: "clone_info", rust_clone="true", repo=reponame, clone_type=clone_type_str, is_update_clone=!ctx.opts.noupdate);
    if !ctx.opts.enable_profile.is_empty() {
        tracing::debug!(target: "clone_info", cloned_sparse_profiles=ctx.opts.enable_profile.join(" "));
    }
}

#[cfg(feature = "eden")]
fn run_eden(
    reponame: &str,
    destination: &Path,
    ctx: &ReqCtx<CloneOpts>,
    mut config: ConfigSet,
) -> Result<()> {
    let logger = ctx.logger();

    if let Some(preferred_regex) = ctx
        .config()
        .get_nonempty_opt::<Regex>("clone", "eden-preferred-destination-regex")?
    {
        let dest_str = destination.to_string_lossy();
        if !preferred_regex.is_match(&dest_str) {
            logger.warn(format!("WARNING: Clone destination {dest_str} is not a preferred location and may result in a bad experience."));
            if let Some(default) = ctx
                .config()
                .get_opt::<PathBuf>("clone", "default-destination-dir")?
            {
                logger.warn(format!(
                    "         Consider cloning to the default location '{}'.",
                    default.join(reponame).display()
                ));
            }
            logger.warn("         Run '@prog@ config clone.eden-preferred-destination-regex' to see the preferred location regex.");
        }
    }

    // We don't return an error immediately because we need to log the clone
    // type before that, yet we might need to log something different if we
    // were able to clone a backing repo.
    let backing_clone_result = || -> Result<(PathBuf, Repo)> {
        let backing_path = if !ctx.opts.eden_backing_repo.is_empty() {
            PathBuf::from(&ctx.opts.eden_backing_repo)
        } else if let Some(dir) = clone::get_default_eden_backing_directory(&config)? {
            dir.join(encode_repo_name(reponame))
        } else {
            abort!("please specify --eden-backing-repo");
        };

        let backing_repo = if identity::sniff_dir(&backing_path)?.is_none() {
            logger.verbose(|| {
                format!(
                    "Cloning {} backing repo to {}",
                    reponame,
                    backing_path.display(),
                )
            });
            try_clone_metadata(ctx, &logger, &mut config, reponame, &backing_path)?
        } else {
            Repo::load(
                &backing_path,
                &PinnedConfig::from_cli_opts(
                    &ctx.global_opts().config,
                    &ctx.global_opts().configfile,
                ),
            )?
        };

        Ok((backing_path, backing_repo))
    }();

    let config_filter = if let Ok((_, ref backing_repo)) = backing_clone_result {
        filter_paths_from_config(backing_repo.config())
    } else {
        filter_paths_from_config(&config)
    };

    // We should consider both filters from the command line options and from config settings
    let arg_filters = ctx
        .opts
        .enable_profile
        .iter()
        .map(|o| o.clone().into())
        .collect::<HashSet<Text>>();
    let config_filters = config_filter.unwrap_or_default();
    let filters = arg_filters
        .union(&config_filters)
        .cloned()
        .collect::<HashSet<Text>>();
    let edenfs_filter = if filters.is_empty() {
        None
    } else {
        Some(filters)
    };

    let clone_type_str = if edenfs_filter.is_some() {
        "eden_sparse"
    } else {
        "eden_fs"
    };
    log_clone_info(clone_type_str, reponame, ctx);

    let (backing_path, backing_repo) = backing_clone_result?;

    let target_rev = get_update_target(&logger, &backing_repo, &ctx.opts)?.map(|(rev, _)| rev);
    logger.verbose(|| {
        format!(
            "Performing EdenFS clone {}@{} from {} to {}",
            reponame,
            target_rev.map_or(String::new(), |t| t.to_hex()),
            backing_path.display(),
            destination.display(),
        )
    });
    clone::eden_clone(&backing_repo, destination, target_rev, edenfs_filter)?;
    Ok(())
}

fn run_non_eden(
    reponame: &str,
    destination: &Path,
    ctx: &ReqCtx<CloneOpts>,
    mut config: ConfigSet,
) -> Result<()> {
    let logger = ctx.logger();

    let clone_type_str = if !ctx.opts.enable_profile.is_empty() {
        "sparse"
    } else {
        "full"
    };
    log_clone_info(clone_type_str, reponame, ctx);

    let mut repo = try_clone_metadata(ctx, &logger, &mut config, reponame, destination)?;

    let target_rev = match get_update_target(&logger, &repo, &ctx.opts)? {
        Some((id, name)) => {
            logger.info(format!("Checking out '{name}'"));

            logger.verbose(|| {
                format!(
                    "Initializing non-EdenFS working copy to commit {}",
                    id.to_hex(),
                )
            });

            Some(id)
        }
        None => {
            logger.verbose("Initializing empty non-EdenFS working copy");
            None
        }
    };

    let (section, prefix) = ("clone", "additional-sparse-profiles.");
    let keys = config.keys_prefixed(section, prefix);
    let config_profiles =
        keys.iter()
            .filter_map(|key| match config.must_get::<String>(section, key) {
                Ok(value) => Some(value),
                Err(e) => {
                    tracing::warn!(
                        "skipping prefixed config '{}.{}' with key '{}': {:?}",
                        section,
                        prefix,
                        key,
                        e
                    );
                    None
                }
            });
    let sparse_profiles = ctx
        .opts
        .enable_profile
        .iter()
        .cloned()
        .chain(config_profiles)
        .collect();

    clone::init_working_copy(&ctx.core, &mut repo, target_rev, sparse_profiles)?;

    Ok(())
}

pub fn run(mut ctx: ReqCtx<CloneOpts>) -> Result<u8> {
    let logger = ctx.logger();

    let config = ctx.config();

    let deprecated_options = [
        (
            "--include",
            "clone-include-option",
            ctx.opts.include.is_empty(),
        ),
        (
            "--exclude",
            "clone-exclude-option",
            ctx.opts.exclude.is_empty(),
        ),
    ];
    for (option_name, option_config, option_is_empty) in deprecated_options {
        if !option_is_empty {
            deprecate(
                &config,
                option_config,
                format!("the {option_name} option has been deprecated"),
            )?;
        }
    }

    let use_eden = ctx.opts.eden(config)?;

    abort_if!(
        !use_eden && !ctx.opts.eden_backing_repo.is_empty(),
        "--eden-backing-repo requires --eden",
    );

    abort_if!(
        use_eden && ctx.opts.noupdate,
        "--noupdate is not compatible with --eden",
    );

    let force_rust = config
        .get_or_default::<Vec<String>>("commands", "force-rust")?
        .contains(&"clone".to_owned());
    let use_rust = force_rust || config.get_or_default("clone", "use-rust")?;
    if !use_rust {
        abort_if!(use_eden, "--eden requires --config clone.use-rust=True");

        logger.verbose("Falling back to Python clone (config not enabled)");
        fallback!("clone.use-rust not set to True");
    }

    let source = match ctx.opts.source(config) {
        Err(_) => fallback!("invalid URL"),
        Ok(source) => {
            // Basically testing whether remote implements SaplingRemoteAPI.
            if source.scheme() == "mononoke" || EagerRepo::url_to_dir(&source).is_some() {
                source
            } else {
                fallback!("unsupported URL scheme");
            }
        }
    };

    if let Some(name) = source.repo_name() {
        // Re-load config now that we have repo name. This will include any per-repo
        // remote configs. Re-assign to ctx.core.config to make extra sure future code
        // does not get the "wrong" config when using ctx.config().
        ctx.core.config = Arc::new(configloader::hg::load(
            RepoInfo::Ephemeral(name),
            &PinnedConfig::from_cli_opts(&ctx.global_opts().config, &ctx.global_opts().configfile),
        )?);
    }

    let mut config = ConfigSet::wrap(ctx.config().clone());

    if ctx.opts.git
        // Allow Rust clone to handle --updaterev if experimental.rust-clone-updaterev is set.
        || (!ctx.opts.updaterev.is_empty() && !config.get_or_default("experimental", "rust-clone-updaterev")?)
    {
        abort_if!(
            use_eden,
            "some specified options are not compatible with --eden"
        );

        logger.verbose("Falling back to Python clone (incompatible options)");
        fallback!("one or more unsupported options in Rust clone");
    }

    config.set("paths", "default", Some(source.clean_str()), &"arg".into());

    let reponame = match config.get_opt::<String>("remotefilelog", "reponame")? {
        // This gets the reponame from the --configfile config.
        Some(c) => {
            logger.verbose(|| format!("Repo name is {c} from config"));
            c
        }
        None => match source.repo_name() {
            Some(name) => {
                logger.verbose(|| format!("Repo name is {} via URL {}", name, ctx.opts.source));
                config.set(
                    "remotefilelog",
                    "reponame",
                    Some(name),
                    &"clone source".into(),
                );
                name.to_string()
            }
            None => abort!("could not determine repo name"),
        },
    };

    let destination = match ctx.opts.args.pop() {
        Some(dest) => absolute(dest).with_context(|| "Cannot get absolute destination path")?,
        None => {
            abort_if!(
                hgplain::is_plain(Some("default_clone_dir")),
                "DEST must be specified because HGPLAIN is enabled",
            );

            // Change "some/repo" into "repo". There is an argument to
            // defaulting to "some_repo" or similar if the canonical repo name
            // contains a slash, but using just "repo" probably has less
            // friction with current workflows/expectations.
            let basename = match reponame.rsplit(&['/', '\\']).next() {
                Some(name) if !name.is_empty() => name,
                _ => abort!("invalid reponame {reponame}"),
            };

            clone::get_default_destination_directory(&config)?.join(basename)
        }
    };

    logger.info(format!(
        "Cloning {} into {}",
        reponame,
        destination.display(),
    ));

    if ctx.opts.enable_profile.len() > 1 {
        abort!("EdenFS only supports a single profile");
    }

    tracing::trace!("performing rust clone");

    if let Some(ident) = identity::sniff_dir(&destination)? {
        abort!(
            "{} directory already exists at clone destination {}",
            ident.dot_dir(),
            destination.display(),
        );
    }

    if use_eden {
        #[cfg(feature = "eden")]
        run_eden(reponame.as_str(), destination.as_path(), &ctx, config)?;
        #[cfg(not(feature = "eden"))]
        anyhow::bail!("eden is not enabled in this build")
    } else {
        run_non_eden(reponame.as_str(), destination.as_path(), &ctx, config)?;
    }

    Ok(0)
}

fn try_clone_metadata(
    ctx: &ReqCtx<CloneOpts>,
    logger: &TermLogger,
    config: &mut ConfigSet,
    reponame: &str,
    destination: &Path,
) -> Result<Repo> {
    // Register cleanup function to clean up incomplete clone at exit (i.e. on error or on ctrl-c,
    // etc.).
    let cleanup_on_error = {
        let logger = logger.clone();
        let dest_preexists = destination.exists();
        let destination = destination.to_owned();
        let debug = ctx.global_opts.debug;
        atexit::AtExit::new(
            "clone cleanup",
            Box::new(move || {
                let cleanup_res = (|| -> Result<()> {
                    let removal_dir = if dest_preexists {
                        let ident =
                            identity::sniff_dir(&destination)?.unwrap_or_else(identity::default);
                        destination.join(ident.dot_dir())
                    } else {
                        destination.to_path_buf()
                    };

                    if !debug {
                        // Give some retries to clean up the failed repo. If we are running async in
                        // another thread, the clone process could still be creating files while we are
                        // deleting them.
                        let mut attempt = 0;
                        loop {
                            attempt += 1;
                            let res = fs_err::remove_dir_all(&removal_dir);
                            if res.is_ok() || attempt >= 10 {
                                break res;
                            }
                        }?;
                    }

                    Ok(())
                })();

                if let Err(err) = cleanup_res {
                    logger.warn(format!(
                        "Error cleaning up incomplete clone {}: {err:?}",
                        destination.to_string_lossy()
                    ));
                }
            }),
        )
        .queued()
    };

    let repo = clone_metadata(ctx, logger, config, reponame, destination)?;

    // Clone was successful - cancel cleanup.
    cleanup_on_error.cancel();

    Ok(repo)
}

#[instrument(skip_all, fields(repo=reponame), err)]
fn clone_metadata(
    ctx: &ReqCtx<CloneOpts>,
    logger: &TermLogger,
    config: &mut ConfigSet,
    reponame: &str,
    destination: &Path,
) -> Result<Repo> {
    let mut includes = ctx.global_opts().configfile.clone();
    if let Some(mut repo_config) = config.get_opt::<PathBuf>("clone", "repo-specific-config-dir")? {
        repo_config.push(format!("{}.rc", encode_repo_name(reponame)));
        if repo_config.exists() {
            let repo_config = repo_config.into_os_string().into_string().unwrap();
            if !includes.contains(&repo_config) {
                includes.push(repo_config);
            }
        }
    }

    let mut repo_config_file_content = includes
        .into_iter()
        .map(|file| format_config_include(&file))
        .collect::<Result<Vec<String>, ConfigFormatError>>()?
        .concat();

    if !repo_config_file_content.is_empty() {
        repo_config_file_content.push('\n');
    }

    let source = ctx.opts.source(config)?;
    if let Some(bm) = source.default_bookmark() {
        config.set(
            "remotenames",
            "selectivepulldefault",
            Some(bm),
            &"clone source".into(),
        );
    }

    repo_config_file_content.push_str(&format_config_section(
        "paths",
        "default",
        source.clean_str(),
    )?);

    // Some config values are inherent to the repo and should be persisted if passed to clone.
    // This is analogous to persisting the --configfile args above.
    {
        let (section, name) = &("remotenames", "selectivepulldefault");
        if let Some(&ValueSource {
            ref source,
            value: Some(ref value),
            ..
        }) = config.get_sources(section, name).last()
        {
            if *source == "--config" || *source == "clone source" {
                repo_config_file_content.push('\n');
                repo_config_file_content.push_str(&format_config_section(section, name, value)?);
            }
        }
    }

    if ctx.opts.aws {
        repo_config_file_content.push_str("\n[experimental]\ndynamic-config-domain-override=aws\n");
    }

    config.set("format", "use-remotefilelog", Some("true"), &"clone".into());

    // Enabling segmented changelog too early breaks the revlog_clone that is needed below
    // in some cases, so make sure it isn't on.
    config.set(
        "format",
        "use-segmented-changelog",
        Some("false"),
        &"clone cmd".into(),
    );

    let mut repo = Repo::init(
        destination,
        config,
        Some(repo_config_file_content),
        &PinnedConfig::from_cli_opts(&ctx.global_opts().config, &[]),
    )?;

    let res = (|| {
        let edenapi = repo.eden_api().map_err(|err| err.tag_network())?;

        let mut capabilities: Vec<String> =
            block_on(edenapi.capabilities()).map_err(|e| e.tag_network())?;
        capabilities.sort_unstable();
        let has_capability = |name: &str| -> bool {
            capabilities
                .binary_search_by_key(&name, AsRef::as_ref)
                .is_ok()
        };

        if has_capability(GIT_FORMAT_CAPABILITY) {
            repo.add_store_requirement("git")?;
        }
        if has_capability("invalid-hash") {
            repo.add_store_requirement("invalid-hash")?;
        }

        let commit_graph_segments = has_capability(COMMIT_GRAPH_SEGMENTS_CAPABILITY)
            && repo
                .config()
                .get_or_default::<bool>("clone", "use-commit-graph")?;

        let mut repo_needs_reload = false;

        if commit_graph_segments {
            repo.add_store_requirement("lazychangelog")?;

            let bookmark_names: Vec<String> = get_selective_bookmarks(&repo)?;
            let metalog = repo.metalog()?;
            let commits = repo.dag_commits()?;
            tracing::trace!("fetching lazy commit data and bookmarks");
            let bookmark_ids = exchange::clone(
                repo.config(),
                edenapi,
                &mut metalog.write(),
                &mut commits.write(),
                bookmark_names,
            )?;
            logger.verbose(|| format!("Pulled bookmarks {bookmark_ids:?}"));

            if repo
                .config()
                .get_or_default("devel", "segmented-changelog-rev-compat")?
            {
                // "lazytext" (vs "lazy") is required for rev compat mode, so let's
                // migrate automatically. This migration only works for tests.
                migrate_to_lazytext(ctx, repo.config(), repo.path())?;
                repo_needs_reload = true;
            }
        } else {
            revlog_clone(repo.config(), logger, ctx, destination)?;
            // reload the repo to pick up any changes written out
            // such as metalog remotenames writes
            repo_needs_reload = true;
        }

        if repo_needs_reload {
            repo = Repo::load(
                destination,
                &PinnedConfig::from_cli_opts(
                    &ctx.global_opts().config,
                    &ctx.global_opts().configfile,
                ),
            )?;
        }

        Ok(())
    })();

    if let Err(err) = res {
        // Triage error using the new repo's config. This runs the network doctor against the
        // host we actually attempted cloning against.
        return Err(triage_error(repo.config(), err, Some("clone")));
    }

    ::fail::fail_point!("run::clone", |_| {
        abort!("Injected clone failure");
    });
    Ok(repo)
}

/// Clone using Python's debugrevlogclone command (legacy path).
fn revlog_clone(
    config: &Arc<dyn Config>,
    logger: &TermLogger,
    ctx: &ReqCtx<CloneOpts>,
    root: &Path,
) -> Result<()> {
    let mut args = vec![
        identity::cli_name().to_string(),
        "debugrevlogclone".to_string(),
        ctx.opts.source(config)?.clean_str().to_string(),
        "-R".to_string(),
        root.to_string_lossy().to_string(),
    ];

    for config in ctx.global_opts().config.iter() {
        args.push("--config".into());
        args.push(config.into());
    }
    if ctx.global_opts().quiet {
        args.push("-q".into());
    }
    if ctx.global_opts().verbose {
        args.push("-v".into());
    }
    if ctx.global_opts().debug {
        args.push("--debug".into());
    }

    logger.verbose(|| format!("Running {}", args.join(" ")));

    let hg_python = HgPython::new(&args);

    abort_if!(
        hg_python.run_hg(&args, &args, ctx.io(), config, false) != 0,
        "Cloning revlog failed"
    );
    Ok(())
}

fn migrate_to_lazytext(
    ctx: &ReqCtx<CloneOpts>,
    config: &Arc<dyn Config>,
    root: &Path,
) -> Result<()> {
    let args = vec![
        identity::cli_name().to_string(),
        "debugchangelog".to_string(),
        "--migrate".to_string(),
        "lazytext".to_string(),
        "-R".to_string(),
        root.to_str().unwrap().to_string(),
    ];

    let hg_python = HgPython::new(&args);
    abort_if!(
        hg_python.run_hg(&args, &args, ctx.io(), config, false) != 0,
        "changelog migration to lazytext failed"
    );

    Ok(())
}

fn get_selective_bookmarks(repo: &Repo) -> Result<Vec<String>> {
    Ok(repo
        .config()
        .must_get("remotenames", "selectivepulldefault")?)
}

#[instrument(skip_all, err, ret)]
fn get_update_target(
    logger: &TermLogger,
    repo: &Repo,
    clone_opts: &CloneOpts,
) -> Result<Option<(HgId, String)>> {
    if clone_opts.noupdate {
        return Ok(None);
    }

    if !clone_opts.updaterev.is_empty() {
        let id = repo.resolve_commit(&clone_opts.updaterev)?.local()?;
        return Ok(Some((id, clone_opts.updaterev.clone())));
    }

    let selective_bookmarks = get_selective_bookmarks(repo)?;
    let main_bookmark = selective_bookmarks
        .first()
        .ok_or_else(|| {
            errors::Abort("remotenames.selectivepulldefault config list is empty".into())
        })?
        .clone();

    match repo.resolve_commit_opt(&main_bookmark)? {
        Some(id) => Ok(Some((id, main_bookmark))),
        None => {
            logger.info(format!(
                "Server has no '{main_bookmark}' bookmark - trying tip.",
            ));

            if let Some(tip) = repo.resolve_commit_opt("tip")? {
                return Ok(Some((tip, "tip".to_string())));
            }

            logger.info("Skipping checkout - no commits available.".to_string());

            Ok(None)
        }
    }
}

pub fn aliases() -> &'static str {
    "clone"
}

pub fn doc() -> &'static str {
    r#"make a copy of an existing repository

    Create a copy of an existing repository in a new directory.

    If no destination directory name is specified, it defaults to the
    basename of the source.

    The location of the source is added to the new repository's
    config file as the default to be used for future pulls.

    Sources are typically URLs. The following URL schemes are assumed
    to be a Git repo: ``git``, ``git+file``, ``git+ftp``, ``git+ftps``,
    ``git+http``, ``git+https``, ``git+ssh``, ``ssh`` and ``https``.

    Scp-like URLs of the form ``user@host:path`` are converted to
    ``ssh://user@host/path``.

    Other URL schemes are assumed to point to an SaplingRemoteAPI capable repo.

    The ``--git`` option forces the source to be interpreted as a Git repo.

    To check out a particular version, use ``-u/--update``, or
    ``-U/--noupdate`` to create a clone with no working copy.

    If specified, the ``--enable-profile`` option should refer to a
    sparse profile within the source repo to filter the contents of
    the new working copy. See :prog:`help -e sparse` for details.

    .. container:: verbose

      Examples:

      - clone a remote repository to a new directory named some_repo::

          @prog@ clone https://example.com/some_repo

    .. container:: verbose

      As an experimental feature, if specified the source URL fragment
      is persisted as the repo's main bookmark.

    Returns 0 on success."#
}

pub fn synopsis() -> Option<&'static str> {
    Some("[OPTION]... SOURCE [DEST]")
}

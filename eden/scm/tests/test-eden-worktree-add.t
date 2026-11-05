
#require eden

  $ unset SCM_SAMPLING_FILEPATH
  $ setconfig sampling.key.command_info=worktree_command_info
  $ setconfig sampling.filepath=$TESTTMP/worktree-sampling

setup backing repo

  $ newclientrepo myrepo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"

test worktree telemetry - normal checkout

  >>> import json, os
  >>> sample_path = os.path.join(os.environ["TESTTMP"], "worktree-sampling")
  >>> sample_count = 0
  >>> def print_worktree_info():
  ...     global sample_count
  ...     with open(sample_path) as f:
  ...         records = [json.loads(item) for item in f.read().strip("\0").split("\0")]
  ...     data = {}
  ...     for record in records[sample_count:]:
  ...         data.update(record["data"])
  ...     sample_count = len(records)
  ...     is_linked_worktree = data["is_linked_worktree"]
  ...     worktree_count_group = data.get("worktree_count_group")
  ...     worktree_count = data.get("worktree_count")
  ...     assert isinstance(is_linked_worktree, bool)
  ...     assert worktree_count_group is None or type(worktree_count_group) is int
  ...     assert worktree_count is None or type(worktree_count) is int
  ...     print((is_linked_worktree, worktree_count_group, worktree_count))
  $ sl root > /dev/null
  >>> print_worktree_info()
  (False, None, None)

test worktree add - basic

  $ sl worktree add $TESTTMP/linked1
  created linked worktree at $TESTTMP/linked1

test worktree telemetry - main and linked checkouts

  $ sl root > /dev/null
  >>> print_worktree_info()
  (False, 2, 2)
  $ cd $TESTTMP/linked1
  $ sl root > /dev/null
  >>> print_worktree_info()
  (True, 2, 2)
  $ cd $TESTTMP/myrepo

test worktree add - with label

  $ sl worktree add $TESTTMP/linked2 --label "feature-x"
  created linked worktree at $TESTTMP/linked2

test worktree add - from a subdirectory of the repo

  $ mkdir -p subdir/nested
  $ cd subdir/nested
  $ sl worktree add $TESTTMP/linked_from_subdir
  created linked worktree at $TESTTMP/linked_from_subdir
  $ cd $TESTTMP/myrepo

test worktree add - missing PATH argument and no path-generator

  $ sl worktree add
  abort: worktree.path-generator is not configured; pass a PATH argument
  [255]

test worktree --rev is rejected for other subcommands

  $ sl worktree list --rev .
  abort: --rev can only be used with 'worktree add'
  [255]

test worktree add - require-generated-path without a generator

  $ sl worktree add --config worktree.require-generated-path=true
  abort: worktree.path-generator is required when worktree.require-generated-path=true
  [255]

test worktree add - destination exists

  $ mkdir $TESTTMP/existing
  $ sl worktree add $TESTTMP/existing
  abort: destination path '$TESTTMP/existing' already exists
  [255]
  $ rmdir $TESTTMP/existing

test worktree add - linked checkout has same files

  $ ls $TESTTMP/linked1/file.txt
  $TESTTMP/linked1/file.txt

test worktree add - linked checkout is on the same commit as main checkout

  $ main_hash=$(sl log -r . -T '{node}')
  $ linked_hash=$(cd $TESTTMP/linked1 && sl log -r . -T '{node}')
  $ test "$main_hash" = "$linked_hash"
  $ echo "main: $main_hash, linked: $linked_hash"
  main: *, linked: * (glob)

test worktree add - revision flag checks out requested revision and accepts bookmark names

  $ base_hash=$(sl log -r . -T '{node}')
  $ sl bookmark -i -r "$base_hash" base
  $ echo more >> file.txt
  $ sl commit -m "second"
  $ tip_hash=$(sl log -r . -T '{node}')
  $ test "$base_hash" != "$tip_hash"
  $ sl worktree add $TESTTMP/linked_revision --rev base
  created linked worktree at $TESTTMP/linked_revision
  $ linked_revision_hash=$(cd $TESTTMP/linked_revision && sl log -r . -T '{node}')
  $ test "$linked_revision_hash" = "$base_hash"
  $ test "$linked_revision_hash" != "$tip_hash"

test worktree add - revision flag accepts revset expressions and Phabricator diffs

  $ sl worktree add $TESTTMP/linked_parent --rev '.^'
  created linked worktree at $TESTTMP/linked_parent
  $ test "$(cd $TESTTMP/linked_parent && sl log -r . -T '{node}')" = "$base_hash"

test worktree add - multiple revisions do not create a worktree

  $ sl worktree add $TESTTMP/multiple_revs --rev '.^ + .'
  abort: revision set contains multiple commits; use first(...), last(...), or a commit hash
  [255]
  $ test -d $TESTTMP/multiple_revs
  [1]

test worktree add - invalid revision does not create a worktree

  $ sl worktree add $TESTTMP/invalid_rev --rev does-not-exist
  abort: unknown revision 'does'!
  [255]
  $ test -d $TESTTMP/invalid_rev
  [1]
  $ sl worktree add $TESTTMP/empty_rev --rev 'draft() and public()'
  abort: empty revision set
  [255]
  $ test -d $TESTTMP/empty_rev
  [1]

test worktree add - revision flag accepts remote-only commit hashes

  $ cd $TESTTMP
  $ newrepo remote_server
  $ echo base > file.txt
  $ sl commit -Aqm "base"
  $ sl clone -q --eden test:remote_server $TESTTMP/remote_client
  $ cd $TESTTMP/remote_server
  $ echo remote > file.txt
  $ sl commit -Aqm "remote-only"
  $ remote_hash=$(sl log -r . -T '{node}')
  $ cd $TESTTMP/remote_client
  $ sl worktree add $TESTTMP/remote_linked --rev "$remote_hash" --config clone.use-skip-commit-resolve=true
  created linked worktree at $TESTTMP/remote_linked
  $ remote_linked_hash=$(cd $TESTTMP/remote_linked && sl log -r . -T '{node}')
  $ test "$remote_linked_hash" = "$remote_hash"

test worktree add - revision flag pulls a Phabricator diff that isn't local

  $ cd $TESTTMP/remote_server
  $ echo diff > file.txt
  $ sl commit -Aqm "remote diff"
  $ diff_hash=$(sl log -r . -T '{node}')
  $ cd $TESTTMP/remote_client
  $ SL_LOG=clone=info sl worktree add $TESTTMP/remote_diff --rev D5678 --config extensions.fbcodereview= --config phrevset.callsign=CALLSIGN --config phrevset.mock-D5678=$diff_hash --config clone.use-skip-commit-resolve=true 2> $TESTTMP/remote_diff.log
  created linked worktree at $TESTTMP/remote_diff
  $ grep pulling $TESTTMP/remote_diff.log
  pulling D5678 (*) from 'test:remote_server' (glob)
  $ test "$(cd $TESTTMP/remote_diff && sl log -r . -T '{node}')" = "$diff_hash"

The pulled commit is in the commit graph by the time EdenFS clones it.

  $ grep -q -- --skip-commit-resolve $TESTTMP/remote_diff.log

test worktree add - an ambiguous Phabricator diff aborts for agents and picks the newest version otherwise

  $ enable amend fbcodereview
  $ setconfig mutation.record=true mutation.enabled=true phrevset.callsign=CALLSIGN
  $ echo local > file.txt
  $ sl commit -Aqm "local diff"
  $ local_diff=$(sl log -r . -T '{node}')
  $ HGPLAIN=1 sl amend -q -d "1000 0" -m "$(printf 'older successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')"
  $ sl goto -q --hidden --config commit.modify-obsolete-mode=ignore $local_diff
  $ HGPLAIN=1 sl amend -q -d "2000 0" -m "$(printf 'newer successor\n\nDifferential Revision: https://phabricator.intern.facebook.com/D1234')"
  $ setconfig phrevset.mock-local-D1234=$local_diff
  $ CODING_AGENT_METADATA=id=test_agent sl worktree add $TESTTMP/ambiguous_agent --rev D1234
  D1234 resolves ambiguously to multiple local commits:
    1: * 1970-01-01 00:33:20 +0000 newer successor (glob)
    2: * 1970-01-01 00:16:40 +0000 older successor (glob)
  abort: ambiguous commit for D1234
  (specify one of the commit hashes directly, or set 'phrevset.prompt-ambiguous-successors=false' to select the newest revision automatically)
  [255]
  $ test -d $TESTTMP/ambiguous_agent
  [1]
  $ sl worktree add $TESTTMP/ambiguous --rev D1234
  D1234 resolves ambiguously to multiple local commits:
    1: * 1970-01-01 00:33:20 +0000 newer successor (glob)
    2: * 1970-01-01 00:16:40 +0000 older successor (glob)
  warning: selected * for D1234 (glob)
  created linked worktree at $TESTTMP/ambiguous
  $ cd $TESTTMP/ambiguous && sl log -r . -T '{desc|firstline}\n'
  newer successor
  $ cd $TESTTMP/myrepo

test worktree add - writes .sl/worktreename marker (basename when no --label)

  $ cat $TESTTMP/linked1/.sl/worktreename
  linked1 (no-eol)

test worktree add - marker reflects --label when provided

  $ cat $TESTTMP/linked2/.sl/worktreename
  feature-x (no-eol)

test worktree add - revision flag sees hidden commits with --hidden

  $ newclientrepo hidden_repo
  $ enable amend
  $ setconfig mutation.record=true mutation.enabled=true
  $ echo hidden > file.txt
  $ sl commit -Aqm "hidden predecessor"
  $ sl amend -qm "amended"
  $ sl --hidden worktree add $TESTTMP/hidden_rev --rev 'obsolete()'
  created linked worktree at $TESTTMP/hidden_rev
  $ cd $TESTTMP/hidden_rev && sl log -r . -T '{desc}\n'
  hidden predecessor
  $ cd $TESTTMP/myrepo

test worktree add - edensparse filters are copied to linked checkout

  $ enable edensparse
  $ cd $TESTTMP
  $ newrepo sparse_server
  $ echo content > included.txt
  $ echo other > excluded.txt
  $ cat > my-filter <<EOF
  > [include]
  > included.txt
  > my-filter
  > [exclude]
  > excluded.txt
  > EOF
  $ sl commit -Aqm 'add files and filter'
  $ sl book master

  $ cd $TESTTMP
  $ sl clone -q --eden test:sparse_server sparse_client --config clone.eden-sparse-filter=my-filter
  $ cd sparse_client

verify main checkout has sparse config

  $ sl filteredfs show
  Enabled Profiles:
  
      ~ my-filter

create linked worktree and verify sparse config is copied

  $ sl worktree add $TESTTMP/sparse_linked
  created linked worktree at $TESTTMP/sparse_linked
  $ cd $TESTTMP/sparse_linked
  $ sl filteredfs show
  Enabled Profiles:
  
      ~ my-filter
  $ cd $TESTTMP/sparse_client

verify both checkouts have the same sparse config content

  $ cmp .sl/sparse $TESTTMP/sparse_linked/.sl/sparse

test worktree add - sl status works in linked filteredfs worktree

  $ cd $TESTTMP/sparse_client
  $ sl worktree add $TESTTMP/sparse_linked_status
  created linked worktree at $TESTTMP/sparse_linked_status
  $ cd $TESTTMP/sparse_linked_status
  $ sl status

  $ cd $TESTTMP/sparse_client

test worktree add - prefetch profiles are copied to linked checkout

  $ cd $TESTTMP
  $ newrepo prefetch_server
  $ echo content > file.txt
  $ sl commit -Aqm 'add file'
  $ sl book master

  $ cd $TESTTMP
  $ sl clone -q --eden test:prefetch_server prefetch_client
  $ cd prefetch_client

activate a prefetch profile in the main checkout

  $ eden prefetch-profile activate trees
  $ eden prefetch-profile list --checkout .
  trees

create linked worktree and verify prefetch profile is copied

  $ sl worktree add $TESTTMP/prefetch_linked
  created linked worktree at $TESTTMP/prefetch_linked
  $ eden prefetch-profile list --checkout $TESTTMP/prefetch_linked
  trees

test worktree add - redirections are copied to linked checkout

  $ cd $TESTTMP
  $ newrepo redirect_server
  $ echo content > file.txt
  $ sl commit -Aqm 'add file'
  $ sl book master

  $ cd $TESTTMP
  $ sl clone -q --eden test:redirect_server redirect_client
  $ cd redirect_client

add a symlink redirection in the main checkout

  $ mkdir build_output
  $ eden redirect add build_output symlink
  $ eden redirect list --json --mount . | grep -o '"repo_path":"build_output"'
  "repo_path":"build_output"

create linked worktree and verify redirection is copied

  $ sl worktree add $TESTTMP/redirect_linked
  created linked worktree at $TESTTMP/redirect_linked
  $ eden redirect list --json --mount $TESTTMP/redirect_linked | grep -o '"repo_path":"build_output"'
  "repo_path":"build_output"

clean up redirections

  $ eden redirect del build_output
  $ eden redirect del --mount $TESTTMP/redirect_linked build_output

test worktree add - post-worktree-add hook fires with correct env vars

  $ cd $TESTTMP
  $ newclientrepo hook_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
#if windows
  $ setconfig hooks.post-worktree-add="echo PATH:%HG_PATH% SOURCE:%HG_SOURCE%"
  $ sl worktree add $TESTTMP/hook_linked
  created linked worktree at $TESTTMP/hook_linked
  PATH:$TESTTMP?hook_linked SOURCE:$TESTTMP?hook_repo\r (esc) (glob)
#else
  $ setconfig hooks.post-worktree-add="echo PATH:\$HG_PATH SOURCE:\$HG_SOURCE"
  $ sl worktree add $TESTTMP/hook_linked
  created linked worktree at $TESTTMP/hook_linked
  PATH:$TESTTMP/hook_linked SOURCE:$TESTTMP/hook_repo
#endif

test worktree add - post-worktree-add hook failure does not abort command

#if windows
  $ setconfig "hooks.post-worktree-add=cmd /c exit 1"
  $ sl worktree add $TESTTMP/hook_linked_fail
  created linked worktree at $TESTTMP/hook_linked_fail
#else
  $ setconfig hooks.post-worktree-add=false
  $ sl worktree add $TESTTMP/hook_linked_fail
  created linked worktree at $TESTTMP/hook_linked_fail
#endif
  $ test -d $TESTTMP/hook_linked_fail

test worktree add - pre-worktree-add hook fires with correct env vars

#if windows
  $ setconfig hooks.pre-worktree-add="echo PATH:%HG_PATH% SOURCE:%HG_SOURCE%"
  $ sl worktree add $TESTTMP/pre_hook_linked
  PATH:$TESTTMP?pre_hook_linked SOURCE:$TESTTMP?hook_repo\r (esc) (glob)
  created linked worktree at $TESTTMP/pre_hook_linked
#else
  $ setconfig hooks.pre-worktree-add="echo PATH:\$HG_PATH SOURCE:\$HG_SOURCE"
  $ sl worktree add $TESTTMP/pre_hook_linked
  PATH:$TESTTMP/pre_hook_linked SOURCE:$TESTTMP/hook_repo
  created linked worktree at $TESTTMP/pre_hook_linked
#endif

test worktree add - pre-worktree-add hook failure aborts command

#if windows
  $ setconfig "hooks.pre-worktree-add=cmd /c exit 1"
  $ sl worktree add $TESTTMP/pre_hook_blocked
  abort: pre-worktree-add hook exited with status 1
  [255]
#else
  $ setconfig hooks.pre-worktree-add=false
  $ sl worktree add $TESTTMP/pre_hook_blocked
  abort: pre-worktree-add hook exited with status 1
  [255]
#endif
  $ test -d $TESTTMP/pre_hook_blocked
  [1]

test worktree add - path-generator produces path when PATH omitted

  $ cd $TESTTMP
  $ newclientrepo gen_repo
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m "init"
  $ setconfig worktree.path-generator="echo $TESTTMP/generated_wt"
  $ sl worktree add
  created linked worktree at $TESTTMP/generated_wt
  $ test -d $TESTTMP/generated_wt

test worktree add - specified revision with generated path

  $ echo generated-rev > generated-rev.txt
  $ sl commit -Aqm "generated rev"
  $ generated_rev=$(sl log -r . -T '{node}')
  $ sl goto -q .^
  $ setconfig worktree.path-generator="echo $TESTTMP/generated_rev_wt"
  $ sl worktree add --rev "$generated_rev"
  created linked worktree at $TESTTMP/generated_rev_wt
  $ linked_hash=$(cd $TESTTMP/generated_rev_wt && sl log -r . -T '{node}')
  $ test "$generated_rev" = "$linked_hash"

test worktree add - path-generator receives correct env vars

#if windows
  $ setconfig worktree.path-generator="echo SOURCE:%HG_SOURCE% LABEL:%HG_LABEL% SL_SOURCE:%SL_SOURCE% SL_LABEL:%SL_LABEL%>$TESTTMP/gen_env_out& echo $TESTTMP/gen_envcheck"
  $ sl worktree add --label my-feature
  created linked worktree at $TESTTMP/gen_envcheck
  $ cat $TESTTMP/gen_env_out
  SOURCE:$TESTTMP?gen_repo LABEL:my-feature SL_SOURCE:$TESTTMP?gen_repo SL_LABEL:my-feature\r (esc) (glob)
#else
  $ setconfig worktree.path-generator="echo SOURCE:\$HG_SOURCE LABEL:\$HG_LABEL SL_SOURCE:\$SL_SOURCE SL_LABEL:\$SL_LABEL > $TESTTMP/gen_env_out; echo $TESTTMP/gen_envcheck"
  $ sl worktree add --label my-feature
  created linked worktree at $TESTTMP/gen_envcheck
  $ cat $TESTTMP/gen_env_out
  SOURCE:$TESTTMP/gen_repo LABEL:my-feature SL_SOURCE:$TESTTMP/gen_repo SL_LABEL:my-feature
#endif

test worktree add - path-generator failure aborts command

#if windows
  $ setconfig "worktree.path-generator=cmd /c exit 1"
  $ sl worktree add
  abort: worktree.path-generator exited with exit code: 1* (glob)
  [255]
#else
  $ setconfig worktree.path-generator=false
  $ sl worktree add
  abort: worktree.path-generator exited with exit status: 1
  [255]
#endif

test worktree add - path-generator empty output aborts command

#if windows
  $ setconfig worktree.path-generator=echo.
  $ sl worktree add
  abort: worktree.path-generator returned empty output
  [255]
#else
  $ setconfig worktree.path-generator="echo ''"
  $ sl worktree add
  abort: worktree.path-generator returned empty output
  [255]
#endif

test worktree add - path-generator empty output aborts command (empty config)

  $ setconfig worktree.path-generator=
  $ sl worktree add
  abort: worktree.path-generator returned empty output
  [255]

test worktree add - path-generator extra stdout aborts command

#if windows
  $ setconfig worktree.path-generator="echo %TESTTMP%\\gen_multiline& echo noise"
  $ sl worktree add
  abort: worktree.path-generator must write exactly one path to stdout
  [255]
#else
  $ setconfig worktree.path-generator="printf '$TESTTMP/gen_multiline\nnoise\n'"
  $ sl worktree add
  abort: worktree.path-generator must write exactly one path to stdout
  [255]
#endif

test worktree add - path-generator relative output aborts command

  $ setconfig worktree.path-generator="echo relative_wt"
  $ sl worktree add
  abort: worktree.path-generator must return an absolute path, got 'relative_wt'
  [255]

test worktree add - path-generator invalid path aborts command

#if windows
  $ setconfig 'worktree.path-generator=python -c "import os,sys; sys.stdout.buffer.write((os.environ[\"TESTTMP\"].replace(\"\\\\\", \"/\") + \"/gen_bad_\" + chr(0) + \"path\").encode())"'
  $ sl worktree add
  abort: worktree.path-generator returned invalid path '$TESTTMP/gen_bad_\0path': contains NUL byte
  [255]
#else
  $ setconfig worktree.path-generator="printf '/tmp/gen_bad_\0path'"
  $ sl worktree add
  abort: worktree.path-generator returned invalid path '/tmp/gen_bad_\0path': contains NUL byte
  [255]
#endif

test worktree add - require-generated-path rejects user PATH

  $ setconfig worktree.require-generated-path=true worktree.path-generator="echo $TESTTMP/gen_required"
  $ sl worktree add $TESTTMP/custom_path
  abort: custom worktree paths are not allowed (worktree.require-generated-path is set); run without a path argument to use the configured path generator
  [255]
  $ test -d $TESTTMP/custom_path
  [1]

test worktree add - require-generated-path allows omitted PATH with generator

  $ sl worktree add
  created linked worktree at $TESTTMP/gen_required
  $ test -d $TESTTMP/gen_required

test worktree add - Windows ANSI path output is accepted

#if windows
  $ setconfig 'worktree.path-generator=python -c "import ctypes, os, sys; cp = ctypes.windll.kernel32.GetACP(); path = os.environ[\"TESTTMP\"].replace(\"\\\\\", \"/\") + \"/gen_\u00fc\"; sys.stdout.buffer.write(path.encode(f\"cp{cp}\"))"'
  $ sl worktree add
  created linked worktree at $TESTTMP/gen_* (glob)
  $ python -c "import os; expected = 'gen_' + chr(0x00fc); assert os.path.isdir(os.path.join(os.environ['TESTTMP'], expected)), os.listdir(os.environ['TESTTMP'])"
#endif

Keep the max-count block last: it leaves the working copy in limitrepo with
worktree.max-count set, which would otherwise leak into later tests (e.g. block
the path-generator `sl worktree add` above on Windows).

test worktree add - max-count limit enforced

  $ cd $TESTTMP
  $ newclientrepo limitrepo
  $ setconfig worktree.max-count=2
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m init
  $ sl worktree add $TESTTMP/limit1
  created linked worktree at $TESTTMP/limit1
  $ sl worktree add $TESTTMP/limit2
  created linked worktree at $TESTTMP/limit2
  $ sl worktree add $TESTTMP/limit3
  abort: cannot create worktree: limit of 2 linked worktrees reached for repository '*' (currently 2); set `worktree.max-count` in your user config (e.g. ~/.hgrc) to raise it, or 0 to disable the limit (glob)
  [255]
  $ test -d $TESTTMP/limit3
  [1]

test worktree add - max-count-message replaces the default limit message

  $ setconfig 'worktree.max-count-message=too many worktrees; see https://example.com/worktrees'
  $ sl worktree add $TESTTMP/limit3
  abort: too many worktrees; see https://example.com/worktrees
  [255]

test worktree add - a slot reservation counts against max-count

  $ cd $TESTTMP
  $ newclientrepo resvrepo
  $ setconfig worktree.max-count=2
  $ touch file.txt
  $ sl add file.txt
  $ sl commit -m init
  $ sl worktree add $TESTTMP/resv1
  created linked worktree at $TESTTMP/resv1

Plant a fresh slot reservation in the separate reservations file, as an in-flight
concurrent `worktree add` would. The group now holds 1 linked worktree + 1
reservation, filling both slots, so the next add fails fast at reservation time
rather than cloning and then aborting.

  $ sl debugshell -c "import json, os; reg = json.load(open(repo.svfs.join('worktrees.json'))); g = next(iter(reg['groups'].values())); json.dump({'reservations': {'test-res': {'group_main': g['main'], 'dest': os.path.join(os.environ['TESTTMP'], 'pending'), 'added': '2999-01-01T00:00:00+00:00'}}}, open(repo.svfs.join('worktree-reservations.json'), 'w'))"
  $ sl worktree add $TESTTMP/resv2
  abort: cannot create worktree: limit of 2 linked worktrees reached for repository '*' (currently 2); set `worktree.max-count` in your user config (e.g. ~/.hgrc) to raise it, or 0 to disable the limit (glob)
  [255]
  $ test -d $TESTTMP/resv2
  [1]

The reservations live in their own file, so `worktrees.json` never gains a
`reservations` key an older `sl` binary could drop.

  $ sl debugshell -c "print('reservation' in open(repo.svfs.join('worktrees.json')).read())"
  False

A stale reservation (older than the TTL, e.g. orphaned by a crashed add) is pruned
and does not keep consuming the slot.

  $ sl debugshell -c "import json, os; reg = json.load(open(repo.svfs.join('worktrees.json'))); g = next(iter(reg['groups'].values())); json.dump({'reservations': {'test-res': {'group_main': g['main'], 'dest': os.path.join(os.environ['TESTTMP'], 'pending'), 'added': '2000-01-01T00:00:00+00:00'}}}, open(repo.svfs.join('worktree-reservations.json'), 'w'))"
  $ sl worktree add $TESTTMP/resv2
  created linked worktree at $TESTTMP/resv2

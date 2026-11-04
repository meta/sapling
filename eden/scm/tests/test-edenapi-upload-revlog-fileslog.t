#require no-eden

  $ configure modernclient
  $ enable snapshot

The server is an eager repo; it serves SaplingRemoteAPI for the test.
  $ newserver server

A revlog-format local repo (no "remotefilelog" and no "eagerepo" requirement)
that points at that server. Its fileslog is the base filelog.fileslog, which
has no metadatastore. This is the repo kind seen in production.
  $ cd $TESTTMP
  $ sl init --config format.use-eager-repo=false localrepo
  $ cd localrepo
  $ setconfig paths.default=test:server remotefilelog.reponame=server
  $ echo a > a
  $ sl ci -Aqm A
  $ cat .sl/store/requires
  eagercompat
  invalidatelinkrev
  narrowheads
  segmentedchangelog
  visibleheads
  $ sl debugshell -c 'print(repo.storage_format(), type(repo.fileslog).__name__, hasattr(repo.fileslog, "metadatastore"))'
  revlog fileslog False

Unit level: `_uploadfilenodes` reads `repo.fileslog.metadatastore` first and
raises an AttributeError on this fileslog:

  $ sl debugshell -c '
  > from sapling import edenapi_upload
  > try:
  >     edenapi_upload._uploadfilenodes(repo, [repo["."]["a"]])
  > except Exception as e:
  >     print("%s: %s" % (type(e).__name__, e))
  > '
  AttributeError: 'fileslog' object has no attribute 'metadatastore'

BUG: uploading crashes with an AttributeError instead of a clean error.
  $ sl cloud upload 2>&1 | grep -E '^(AttributeError|abort)'
  AttributeError: 'fileslog' object has no attribute 'metadatastore'

  $ echo b > b
  $ sl snapshot createremote 2>&1 | grep -E '^(AttributeError|abort)'
  AttributeError: 'fileslog' object has no attribute 'metadatastore'

#require no-eden
  $ configure modernclient

  $ . "$TESTDIR/library.sh"

# Start up translation service.

  $ sl debugpython -- "$TESTDIR/conduithttp.py" --port-file conduit.port --pid conduit.pid
  $ cat conduit.pid >> $DAEMON_PIDS
  $ CONDUIT_PORT=`cat conduit.port`
  $ cat > ~/.arcrc <<EOF
  > {
  >   "hosts": {
  >     "https://phabricator.intern.facebook.com/api/": {
  >       "user": "testuser",
  >       "oauth": "testtoken"
  >     }
  >  }
  > }
  > EOF

# Test fastlog

  $ newclientrepo master
  $ echo x > x
  $ sl commit -qAm x
  $ echo x >> x
  $ sl commit -Aqm xx
  $ sl push -q --to master --create
  $ cd ..

  $ newclientrepo shallow master_server
  $ echo x >> x
  $ sl commit -Aqm xx2
  $ cd ../master
  $ echo y >> y
  $ sl commit -Aqm yy2
  $ echo x >> x
  $ sl commit -Aqm xx2-fake-rebased
  $ echo y >> y
  $ sl commit -Aqm yy3
  $ sl push -q --to master
  $ cd ../shallow
  $ sl pull -q
  $ sl goto master -q
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over *s (glob) (?)
  $ echo x > x
  $ sl commit -qAm xx3

Verfiy correct linkrev despite fastlog failures

Case 1: fastlog service calls fails or times out

  $ echo {} > .arcconfig
  $ cat >> .sl/config <<EOF
  > [extensions]
  > fbcodereview=
  > [fastlog]
  > enabled=True
  > [fbscmquery]
  > reponame = basic
  > host = localhost:$CONDUIT_PORT
  > path = /intern/conduit/
  > [phabricator]
  > arcrc_host = https://phabricator.intern.facebook.com/api/
  > use-unix-socket = False
  > graphql_host = http://none_such.intern.facebook.com:$CONDUIT_PORT
  > default_timeout = 60
  > graphql_app_id = 1234
  > graphql_app_token = TOKEN123
  > EOF
  $ sl log -f x -T '{node|short} {desc} {phase} {files}\n'
  a5957b6bf0bd xx3 draft x
  32e6611f6149 xx2-fake-rebased public x
  0632994590a8 xx public x
  b292c1e3311f x public x

Case 2: fastlog returns empty results

  $ clearcache
  $ cat >> .sl/config <<EOF
  > [phabricator]
  > graphql_host = http://localhost:$CONDUIT_PORT
  > EOF
  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/set_log_response/7200df4e0acad9339167ac526b0054b1bab32dee/
  $ sl log -f x -T '{node|short} {desc} {phase} {files}\n'
  a5957b6bf0bd xx3 draft x
  32e6611f6149 xx2-fake-rebased public x
  0632994590a8 xx public x
  b292c1e3311f x public x

Case 3: fastlog returns a bad hash

  $ clearcache
  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/set_log_response/7200df4e0acad9339167ac526b0054b1bab32dee/123456
  $ sl log -f x -T '{node|short} {desc} {phase} {files}\n'
  a5957b6bf0bd xx3 draft x
  32e6611f6149 xx2-fake-rebased public x
  0632994590a8 xx public x
  b292c1e3311f x public x

Fastlog succeeds and returns the correct results

  $ clearcache
  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/set_log_response/7200df4e0acad9339167ac526b0054b1bab32dee/32e6611f6149e85f58def77ee0c22549bb6953a2
  $ sl log -f x -T '{node|short} {desc} {phase} {files}\n'
  a5957b6bf0bd xx3 draft x
  32e6611f6149 xx2-fake-rebased public x
  0632994590a8 xx public x
  b292c1e3311f x public x

Fastlog should never get called on draft commits

  $ clearcache
  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/set_log_response/a5957b6bf0bdeb9b96368bddd2838004ad966b7d/crash
  $ sl log -f x > /dev/null
  1 files fetched over 1 fetches - (1 misses, 0.00% hit ratio) over 0.00s (?)

Fastlog extension when the fastlog query fails

  $ cat >> .sl/config <<EOF
  > [extensions]
  > fastlog=
  > [fastlog]
  > files=True
  > EOF
  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/fail_next/scmquery-unavailable
  $ sl log -f x -T '{node|short} {desc} {phase} {files}\n'
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  ...
  sapling.ext.extlib.phabricator.graphql.ClientError: scmquery-unavailable
  [1]

pathlog starting from a draft commit when the fastlog query fails

  $ curl -s -X PUT http://localhost:$CONDUIT_PORT/fail_next/scmquery-unavailable
  $ sl debugshell -c '
  > from sapling import pathlog
  > for node in pathlog.pathlog(repo, repo["."].node(), "x"):
  >     ctx = repo[node]
  >     ui.write(f"{ctx} {ctx.description()} {ctx.phasestr()}\n")
  > '
  a5957b6bf0bd xx3 draft
  Traceback (most recent call last):
  ...
  sapling.ext.extlib.phabricator.graphql.ClientError: scmquery-unavailable
  [1]

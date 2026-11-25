#inprocess-hg-incompatible
#require no-fsmonitor no-eden

Setup. SCM_SAMPLING_FILEPATH needs to be cleared as some environments may
have it set.

  $ unset SCM_SAMPLING_FILEPATH

  $ mkcommit() {
  >    echo "$1" > "$1"
  >    sl add "$1"
  >    echo "add $1" > msg
  >    echo "" >> msg
  >    sl ci -l msg
  > }
Init the repo
  $ configure modern
  $ newclientrepo testrepo
  $ mkcommit a
Create an extension that logs every commit and also call repo.revs twice

Create an extension that logs the call to commit
  $ cat > $TESTTMP/logcommit.py << EOF
  > from sapling import extensions, localrepo
  > def cb(sample):
  >   return len(sample)
  > def _commit(orig, repo, *args, **kwargs):
  >     repo.ui.log("commit", "match filter", k=1, a={"hi":"ho"})
  >     repo.ui.log("foo", "does not match filter", k=1, a={"hi":"ho"})
  >     repo.ui.log("commit", "message %s", "string", k=1, a={"hi":"ho"})
  >     return orig(repo, *args, **kwargs)
  > def extsetup(ui):
  >     extensions.wrapfunction(localrepo.localrepository, 'commit', _commit)
  >     @ui.atexit
  >     def handler():
  >       ui._measuredtimes['atexit_measured'] += 7
  >       ui.warn("atexit handler executed\n")
  > EOF


Set up the extension and set a log file
We include only the 'commit' key, only the events with that key will be
logged
  $ cat >> $HGRCPATH << EOF
  > [ui]
  > logmeasuredtimes=True
  > [sampling]
  > key.commit=commit_table
  > key.measuredtimes=measuredtimes
  > [extensions]
  > sampling=
  > EOF
  $ LOGDIR=$TESTTMP/logs
  $ mkdir $LOGDIR
  >>> import os
  >>> LOGDIR = os.path.join(os.environ["TESTTMP"], "logs")
  $ echo "logcommit=$TESTTMP/logcommit.py" >> $HGRCPATH
  $ echo "[sampling]" >> $HGRCPATH
  $ echo "filepath = $LOGDIR/samplingpath.txt" >> $HGRCPATH

Do a couple of commits.  We expect to log two messages per call to repo.commit.
  $ mkdir a_topdir && cd a_topdir
  $ mkcommit b
  atexit handler executed
  atexit handler executed
  $ mkcommit c
  atexit handler executed
  atexit handler executed
  >>> import json
  >>> sample_path = os.path.join(LOGDIR, "samplingpath.txt")
  >>> with open(sample_path) as f:
  ...     data = f.read()
  >>> for record in data.strip("\0").split("\0"):
  ...     parsedrecord = json.loads(record)
  ...     if parsedrecord['category'] == 'commit_table':
  ...         print(' '.join([parsedrecord["data"]["msg"], parsedrecord["category"]]))
  ...         assert len(parsedrecord["data"]) == 4
  ...     elif parsedrecord['category'] == 'measuredtimes':
  ...         print('atexit_measured: ', ", ".join(sorted(parsedrecord['data'])))
  atexit_measured:  atexit_measured, metrics_type
  atexit_measured:  command_duration
  match filter commit_table
  message string commit_table
  atexit_measured:  atexit_measured, metrics_type
  atexit_measured:  command_duration
  atexit_measured:  atexit_measured, metrics_type
  atexit_measured:  command_duration
  match filter commit_table
  message string commit_table
  atexit_measured:  atexit_measured, metrics_type
  atexit_measured:  command_duration

Test topdir logging:
  $ setconfig sampling.logtopdir=True
  $ setconfig sampling.key.command_info=command_info
  $ sl files c > /dev/null
  atexit handler executed
  >>> from sapling import json
  >>> sample_path = os.path.join(LOGDIR, "samplingpath.txt")
  >>> with open(sample_path) as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> print([json.loads(d)["data"]["topdir"] for d in data if "topdir" in d])
  ['a_topdir']

Test env-var logging:
  $ setconfig sampling.env_vars=TEST_VAR1,TEST_VAR2
  $ setconfig sampling.key.env_vars=env_vars
  $ export TEST_VAR1=abc
  $ export TEST_VAR2=def
  >>> import json, pprint
  >>> def env_vars_logged():
  ...     sample_path = os.path.join(LOGDIR, "samplingpath.txt")
  ...     with open(sample_path) as f:
  ...         data = f.read().strip("\0").split("\0")
  ...     for jsonstr in data:
  ...         entry = json.loads(jsonstr)
  ...         if entry["category"] == "env_vars":
  ...             for k in sorted(entry["data"].keys()):
  ...                 print("%s: %s" % (k, entry["data"][k]))

A Python command logs them once:
  $ rm $LOGDIR/samplingpath.txt
  $ sl files c > /dev/null
  atexit handler executed
  >>> env_vars_logged()
  env_test_var1: abc
  env_test_var2: def

So does a command implemented in Rust, which never reaches the Python hook:
  $ rm $LOGDIR/samplingpath.txt
  $ sl root > /dev/null
  >>> env_vars_logged()
  env_test_var1: abc
  env_test_var2: def

An unset variable is left out:
  $ unset TEST_VAR2
  $ rm $LOGDIR/samplingpath.txt
  $ sl root > /dev/null
  >>> env_vars_logged()
  env_test_var1: abc

Test rust traces make it to sampling file as well:
  $ rm $LOGDIR/samplingpath.txt
  $ setconfig sampling.key.from_rust=hello
  $ sl debugshell -c "from sapling import tracing; tracing.info('msg', target='from_rust', hi='there')"
  atexit handler executed
  >>> import json
  >>> sample_path = os.path.join(LOGDIR, "samplingpath.txt")
  >>> with open(sample_path) as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> for entry in data:
  ...     parsed = json.loads(entry)
  ...     if parsed['category'] == 'hello':
  ...         print(entry)
  {"category":"hello","data":{"message":"msg","hi":"there"}}

Test command_duration is logged when ctrl-c'd:
  $ rm $LOGDIR/samplingpath.txt
  $ cat > $TESTTMP/sleep.py <<EOF
  > from sapling import registrar
  > cmdtable = {}
  > command = registrar.command(cmdtable)
  > @command('sleep', norepo=True)
  > def sleep(ui):
  >     import os, time
  >     with open("sleep.pid", "w") as f:
  >         f.write(str(os.getpid()))
  >     time.sleep(3600)
  > EOF
  $ sl sleep --config extensions.sigint_self=$TESTTMP/sleep.py &
  $ sl debugpython <<EOF
  > import os, signal
  > while True:
  >     if os.path.exists("sleep.pid"):
  >         os.kill(int(open("sleep.pid").read()), signal.SIGINT)
  >         break
  > EOF
  $ wait
  >>> import json
  >>> sample_path = os.path.join(LOGDIR, "samplingpath.txt")
  >>> with open(sample_path) as f:
  ...     data = f.read().strip("\0").split("\0")
  >>> for entry in data:
  ...     parsed = json.loads(entry)
  ...     if parsed['category'] == 'measuredtimes' and "command_duration" in parsed["data"]:
  ...         print(entry)
  {"category":"measuredtimes","data":{"command_duration":*}} (glob)

Test ui.metrics.gauge API
  $ cat > $TESTTMP/a.py << EOF
  > def reposetup(ui, repo):
  >     ui.metrics.inc("test_foo_a", 1)
  >     ui.metrics.inc("test_foo_b", 2)
  >     ui.metrics.inc("test_foo_b", len(repo))
  >     ui.metrics.inc("test_bar")
  >     ui.metrics.inc("test_bar")
  > EOF
  $ SCM_SAMPLING_FILEPATH=$TESTTMP/a.txt sl log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py --config sampling.key.metrics=aaa
  .
  atexit handler executed
  >>> import os, json
  >>> with open(os.path.join(os.environ["TESTTMP"], "a.txt"), "r") as f:
  ...     lines = f.read().split("\0")
  ...     for line in lines:
  ...         if "foo" in line:
  ...             obj = json.loads(line)
  ...             category = obj["category"]
  ...             data = obj["data"]
  ...             print("category: %s" % category)
  ...             for k, v in sorted(data.items()):
  ...                 if "test" in k:
  ...                     print("  %s=%s" % (k, v))
  category: aaa
    test_bar=2
    test_foo_a=1
    test_foo_b=5

Counters get logged for native commands:
  $ SCM_SAMPLING_FILEPATH=$TESTTMP/native.txt sl debugmetrics --config sampling.key.metrics=aaa
  >>> import os, json
  >>> with open(os.path.join(os.environ["TESTTMP"], "native.txt"), "r") as f:
  ...     lines = filter(None, f.read().split("\0"))
  ...     for line in lines:
  ...         obj = json.loads(line)
  ...         if obj["category"] == "aaa":
  ...             print("  test_counter=%s" % obj["data"]["test_counter"])
    test_counter=1

Metrics can be printed if devel.print-metrics is set:
  $ sl log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py --config devel.print-metrics= --config devel.skip-metrics=scmstore,indexedlog,watchman
  .
  atexit handler executed
  test_bar: 2
  test_foo_a: 1
  test_foo_b: 5

Metrics is logged to blackbox:

  $ setconfig blackbox.track=metrics
  $ sl log -r null -T '.\n' --config extensions.gauge=$TESTTMP/a.py
  .
  atexit handler executed
  $ sl blackbox --no-timestamp --no-sid --pattern '{"legacy_log":{"service":"metrics"}}' | grep foo
  [legacy][metrics] {"metrics":*,"test":{"bar":2,"foo":{"a":1,"b":5}}}} (glob)
  [legacy][metrics] {"metrics":*,"test":{"bar":2,"foo":{"a":1,"b":5}}}} (glob)
  [legacy][metrics] {"metrics":*,"test":{"bar":2,"foo":{"a":1,"b":5}}}} (glob)
  atexit handler executed

Invalid format strings don't crash Mercurial

  $ SCM_SAMPLING_FILEPATH=$TESTTMP/invalid.txt sl --config sampling.key.invalid=invalid debugsh -c 'repo.ui.log("invalid", "invalid format %s %s", "single")'
  atexit handler executed
  >>> import os, json
  >>> with open(os.path.join(os.environ["TESTTMP"], "invalid.txt"), "r") as f:
  ...     lines = f.read().split("\0")
  ...     for line in lines:
  ...         if "invalid" in line:
  ...             obj = json.loads(line)
  ...             category = obj["category"]
  ...             data = obj["data"]
  ...             print("category: %s" % category)
  ...             for k, v in sorted(data.items()):
  ...                 print("  %s=%s" % (k, v))
  category: command_info
    metrics_type=command_info
    option_names=['config', 'command']
    option_values=[['sampling.key.invalid=invalid'], 'repo.ui.log("invalid", "invalid format %s %s", "single")']
    positional_args=['debugsh']
  category: invalid
    metrics_type=invalid
    msg=invalid format %s %s single

Test command name:
  $ newclientrepo
  $ enable sparse
Both Rust and Python use canonical "update" name:
  $ SL_LOG=command_info=debug sl go --config checkout.use-rust=false . 2>&1 | grep command=
  DEBUG command_info: command="update"
  DEBUG command_info: command="update"

  $ SL_IDENTITY=sl SL_LOG=command_info=debug sl go --config checkout.use-rust=false . 2>&1 | grep command=
  DEBUG command_info: command="update"
  DEBUG command_info: command="update"

Sub commands work:
  $ SL_LOG=command_info=debug sl sparse 2>&1 | grep command=
  DEBUG command_info: command="sparse"
  $ SL_LOG=command_info=debug sl sparse refresh 2>&1 | grep command=
  DEBUG command_info: command="sparse refresh"

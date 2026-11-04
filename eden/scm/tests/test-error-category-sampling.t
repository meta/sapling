#inprocess-hg-incompatible

  $ configure modernclient
  $ enable amend
  $ setconfig sampling.filepath=$TESTTMP/samples sampling.key.command_info=dct
  $ newclientrepo

  $ cat > $TESTTMP/category.py <<'PY'
  > import json, os
  > path = os.path.join(os.environ["TESTTMP"], "samples")
  > categories = []
  > if os.path.exists(path):
  >     with open(path, "rb") as f:
  >         for record in f.read().split(b"\0"):
  >             if record:
  >                 data = json.loads(record)["data"]
  >                 if "error_category" in data:
  >                     categories.append(data["error_category"])
  >     os.unlink(path)
  > print(categories)
  > PY

Caller errors retain exit 255 and report their category through sampling:

  $ sl hide
  abort: nothing to hide
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  ['usage']
  $ sl bogus
  unknown command 'bogus'
  (use 'sl help' to get help)
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  ['usage']

Both dispatchers report the category outside a repository:

  $ cd $TESTTMP
  $ sl status --config commands.force-rust=status
  abort: '$TESTTMP' is not inside a repository, but this command requires a repository!
  (use 'cd' to go to a directory inside a repository and try again)
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  ['usage']
  $ sl add a
  abort: '$TESTTMP' is not inside a repository, but this command requires a repository!
  (use 'cd' to go to a directory inside a repository and try again)
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  ['usage']

A custom exit mask does not affect the category:

  $ sl add a --config ui.exitcodemask=63
  abort: '$TESTTMP' is not inside a repository, but this command requires a repository!
  (use 'cd' to go to a directory inside a repository and try again)
  [63]
  $ sl debugpython -- $TESTTMP/category.py
  ['usage']

Direct certificate errors and network doctor diagnoses keep their exit codes:

  $ cat > $TESTTMP/categoryext.py <<'PY'
  > import bindings
  > from sapling import error, registrar
  > cmdtable = {}
  > command = registrar.command(cmdtable)
  > @command("testerrorcategory", [], "MODE", norepo=True)
  > def testerrorcategory(ui, mode):
  >     if mode == "direct":
  >         raise error.CertificateError("invalid certificate")
  >     if mode == "return":
  >         return 64
  >     bindings.doctor.diagnose_network = lambda cfg: ("renew certificate", "details", True)
  >     raise error.HttpError("network failure")
  > PY
  $ setconfig extensions.errorcategory=$TESTTMP/categoryext.py experimental.network-doctor=true
  $ sl testerrorcategory direct
  certificate error: invalid certificate!
  
  (run 'sl config auth' to see configured certificates)
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  ['certificate']
  $ sl testerrorcategory diagnosed
  abort: command failed due to network error (see * for details) (glob)
  
  renew certificate
  [1]
  $ sl debugpython -- $TESTTMP/category.py
  ['certificate']

Unclassified errors and numeric return codes do not get a category:

  $ newclientrepo
  $ sl log -r doesnotexist
  abort: unknown revision 'doesnotexist'!
  [255]
  $ sl debugpython -- $TESTTMP/category.py
  []
  $ sl testerrorcategory return
  [64]
  $ sl debugpython -- $TESTTMP/category.py
  []

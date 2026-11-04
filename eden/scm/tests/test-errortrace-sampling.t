#inprocess-hg-incompatible

  $ configure modernclient
  $ enable amend
  $ setconfig sampling.filepath=$TESTTMP/samples
  $ setconfig sampling.key.errortracekey=dct sampling.key.error_prefix=dct sampling.key.errortrace=manifold:scm_traces
  $ newclientrepo

  $ cat > $TESTTMP/uploads.py <<EOF
  > import json, os
  > path = os.path.join(os.environ["TESTTMP"], "samples")
  > uploads = 0
  > if os.path.exists(path):
  >     with open(path, "rb") as f:
  >         for record in f.read().split(b"\0"):
  >             if not record:
  >                 continue
  >             record = json.loads(record)
  >             if record["category"] == "dct" and "errortracekey" in record["data"]:
  >                 uploads += 1
  >     os.unlink(path)
  > print("uploads:", uploads)
  > EOF

An unexpected failure uploads a traceback:

  $ sl log -r doesnotexist
  abort: unknown revision 'doesnotexist'!
  [255]
  $ sl debugpython -- $TESTTMP/uploads.py
  uploads: 1

Failures that are the caller's problem do not, whether raised by a command,
by the dispatcher, or by the Rust dispatcher outside a repository:

  $ sl hide
  abort: nothing to hide
  [255]
  $ sl debugpython -- $TESTTMP/uploads.py
  uploads: 0

  $ sl bogus
  unknown command 'bogus'
  (use 'sl help' to get help)
  [255]
  $ sl debugpython -- $TESTTMP/uploads.py
  uploads: 0

  $ cd $TESTTMP
  $ sl status
  abort: '$TESTTMP' is not inside a repository, but this command requires a repository!
  (use 'cd' to go to a directory inside a repository and try again)
  [255]
  $ sl debugpython -- $TESTTMP/uploads.py
  uploads: 0

An outer exception does not determine whether the command's traceback is logged.
Logging failures preserve the command's original exception. Certificate diagnoses
suppress uploads, while unclassified errors still upload when their exit status
is 64 or 77:

  $ cat > $TESTTMP/callcatch.py <<'PY'
  > from types import SimpleNamespace
  > from unittest.mock import patch
  > from sapling import error, scmutil, ui as uimod
  > ui = uimod.ui.load()
  > req = SimpleNamespace(cmdrepo=None)
  > def callererror():
  >     raise error.Abort("caller error", error_category=error.USAGE_ERROR)
  > with patch.object(scmutil, "_uploadtraceback") as upload:
  >     try:
  >         raise RuntimeError("outer error")
  >     except RuntimeError:
  >         assert scmutil.callcatch(ui, req, callererror) == 255
  >     assert upload.call_count == 0
  > def unexpected():
  >     raise RuntimeError("command error")
  > with patch.object(scmutil, "_uploadtraceback", side_effect=OSError("logging failed")):
  >     try:
  >         scmutil.callcatch(ui, req, unexpected)
  >     except RuntimeError as ex:
  >         assert str(ex) == "command error"
  >     else:
  >         raise AssertionError("command error was not propagated")
  > def raiseerror(ex):
  >     raise ex
  > with patch.object(scmutil, "_uploadtraceback") as upload, patch.object(ui, "warn"):
  >     for code in (64, 77):
  >         ex = error.Abort("uncategorized error")
  >         ex.exitcode = code
  >         assert scmutil.callcatch(ui, req, lambda: raiseerror(ex)) == code
  >     assert upload.call_count == 2
  >     upload.reset_mock()
  >     ex = error.CertificateError("invalid certificate")
  >     assert scmutil.callcatch(ui, req, lambda: raiseerror(ex)) == -1
  >     assert upload.call_count == 0
  >     ui.setconfig("experimental", "network-doctor", True)
  >     diagnosis = ("renew certificate", "details", True)
  >     with patch.object(scmutil.bindings.doctor, "diagnose_network", return_value=diagnosis):
  >         ex = error.HttpError("network failure")
  >         assert scmutil.callcatch(ui, req, lambda: raiseerror(ex)) == 1
  >     assert upload.call_count == 0
  > PY
  $ sl debugpython -- $TESTTMP/callcatch.py
  abort: caller error
  warning: could not log command traceback: logging failed

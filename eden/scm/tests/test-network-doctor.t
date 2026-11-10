
#require no-eden


  $ eagerepo
  $ touch $TESTTMP/stub

  $ setconfig experimental.network-doctor=True paths.default=mononoke://169.254.1.2/foo
Set up fake cert paths so we don't hit "missing certs" error.
  $ setconfig auth.test.cert=$TESTTMP/stub auth.test.key=$TESTTMP/stub auth.test.priority=1 auth.test.prefix=mononoke://*

  $ sl init repo && cd repo

  $ sl pull --config edenapi.url=https://test_fail/foo --config doctor.external-host-check-url=https://test_succeed
  pulling from mononoke://169.254.1.2/foo
  abort: command failed due to network error (see * for details) (glob)
  
  Please check your VPN or proxy (internet okay, but can't reach server).
  [1]


With edenapi.url unset, the doctor checks the SLAPI server the client derives
from the mononoke:// path.
  $ sl pull --config edenapi.url= --config paths.default=mononoke://test_fail/foo --config doctor.external-host-check-url=https://test_succeed --verbose
  pulling from mononoke://test_fail/foo
  abort: command failed due to network error (see * for details) (glob)
  
  Please check your VPN or proxy (internet okay, but can't reach server).
    no server connectivity: TCP error: test
  [1]


  $ sl pull --config edenapi.url=https://test_fail/foo --config doctor.external-host-check-url=https://test_succeed --verbose
  pulling from mononoke://169.254.1.2/foo
  abort: command failed due to network error (see * for details) (glob)
  
  Please check your VPN or proxy (internet okay, but can't reach server).
    no server connectivity: TCP error: test
  [1]


  $ sl pull --config edenapi.url=https://test_fail/foo --config doctor.external-host-check-url=https://test_succeed --debug
  pulling from mononoke://169.254.1.2/foo
  abort: command failed due to network error (see * for details) (glob)
  
  Please check your VPN or proxy (internet okay, but can't reach server).
    no server connectivity: TCP error: test
  
  Original error:
  \[6\] (Could not|Couldn't) resolve (hostname|host name) \(Could not resolve host: test_fail\) (re)
  [1]


Works for native rust commands as well.
  $ sl clone mononoke://169.254.1.2/banana --config commands.force-rust=clone --config edenapi.url=https://test_fail/foo --config doctor.external-host-check-url=https://test_succeed
  Cloning banana into $TESTTMP/repo/banana
  abort: command failed due to network error
  
  Please check your VPN or proxy (internet okay, but can't reach server).
  
  Details:
  
  NoServer(TCP(Custom { kind: Other, error: "test" }))
  
  Original error:
  
  Network Error: \[6\] (Could not|Couldn't) resolve (hostname|host name) \(Could not resolve host: test_fail\) (re)
  
  Caused by:
      \[6\] (Could not|Couldn't) resolve (hostname|host name) \(Could not resolve host: test_fail\) (re)
  
  [255]

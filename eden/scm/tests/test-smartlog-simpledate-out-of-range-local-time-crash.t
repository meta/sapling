#require no-eden no-windows

`sl commit -d` accepts dates up to 9999-12-31 23:59:59 UTC. `simpledate()`, which
the default smartlog template uses for commit dates, converts the date to the
viewer's time zone with `datetime`, which only covers years 1 to 9999.

  $ setconfig commitcloud.enablestatus=false
  $ enable smartlog
  $ newclientrepo
  $ echo a > a
  $ sl commit -Aqm A -d '253402300799 0'

At UTC and west of it, the local time is still in year 9999:

  $ TZ=UTC sl log -r . -T '{simpledate(date)}\n'
  9999-12-31 23:59
  $ TZ=America/Los_Angeles sl log -r . -T '{simpledate(date)}\n'
  9999-12-31 15:59
  $ TZ=UTC sl log -r . -T '{simpledate(date, "America/Los_Angeles")}\n'
  9999-12-31 15:59

FIXME(T297593457): east of UTC, by local time zone or by the time zone argument,
the local time is in year 10000 and `simpledate()` crashes.

  $ TZ=Asia/Kolkata sl log -r . -T '{simpledate(date)}\n'
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  ...
  ValueError: year 10000 is out of range
  [1]
  $ TZ=UTC sl log -r . -T '{simpledate(date, "Pacific/Kiritimati")}\n'
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  ...
  OverflowError: date value out of range
  [1]

So does smartlog with the date template of the default `sl_date` alias:

  $ TZ=Asia/Kolkata sl smartlog -T '{smartdate(date, 5400, age(date), simpledate(date, ""))}\n'
  ** Sapling SCM (version *) has crashed: (glob)
  Traceback (most recent call last):
  ...
  ValueError: year 10000 is out of range
  [1]

FIXME(T297593457): on Windows, `datetime.fromtimestamp()` fails with `OSError`
before 1970 and after 3000 in every zone, so `simpledate()` aborts there.
Simulated here:

  $ cat > $TESTTMP/winlimits.py << 'EOF'
  > import datetime
  > class winlike(datetime.datetime):
  >     @classmethod
  >     def fromtimestamp(cls, t, tz=None):
  >         if t < 0 or t > 32535215999:
  >             raise OSError(22, "Invalid argument")
  >         return super().fromtimestamp(t, tz)
  > def reposetup(ui, repo):
  >     ui.atexit(setattr, datetime, "datetime", datetime.datetime)
  >     datetime.datetime = winlike
  > EOF
  $ echo b > b
  $ sl commit -Aqm B -d '-2208988800 0'
  $ TZ=Asia/Kolkata sl log -r 'all()' -T '{simpledate(date)}\n' --config extensions.winlimits=$TESTTMP/winlimits.py
  abort: Invalid argument
  [255]

#require no-eden

FIXME(T296818104): an index outside the undo log crashes the `olddraft()` and
`oldworkingcopyparent()` revsets, the undo template functions and `sl undo -n 0`
with a bare `IndexError`.

  $ newclientrepo repo
  $ echo >> file
  $ sl commit -Aqm first
  $ enable undo
  $ sl bookmark book
  $ echo >> file
  $ sl commit -Aqm second

The undo log has three entries: 0 (the second commit), 1 (the bookmark) and 2
(the gap before undo was enabled). Indexes inside it work:

  $ sl log -r 'olddraft(1)' -T '{desc}\n'
  first
  $ sl log -r 'olddraft(2)' -T '{desc}\n'
  first
  $ sl log -r 'oldworkingcopyparent(2)' -T '{desc}\n'
  first
  $ sl log -r 'draft()' -T '{desc}={undonecommits(1)}\n'
  first=
  second=644678d0e99f59d264f258dd03628364f6222f71
  $ sl log -r 'draft()' -T '{desc}={donecommits(1)}\n'
  first=7e4a437248600da59440a0522c9312c8ae938a2a
  second=
  $ sl log -r 'draft()' -T '{desc}={oldworkingcopyparent(1)}\n'
  first=7e4a437248600da59440a0522c9312c8ae938a2a
  second=
  $ sl log -r 'draft()' -T '{desc}={oldbookmarks(1)}\n'
  first=book
  second=
  $ sl log -r 'draft()' -T '{desc}={removedbookmarks(1)}\n'
  first=
  second=book

Indexes outside it crash: one past the oldest entry and -1, plus 2**64 for
`olddraft()`.

  $ sl log -r 'olddraft(3)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r 'olddraft(-1)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r 'olddraft(18446744073709551616)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r 'oldworkingcopyparent(3)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r 'oldworkingcopyparent(-1)' -T '{desc}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{undonecommits(3)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{undonecommits(-1)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{donecommits(3)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{donecommits(-1)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{oldworkingcopyparent(3)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{oldworkingcopyparent(-1)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{oldbookmarks(3)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{oldbookmarks(-1)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{removedbookmarks(3)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{removedbookmarks(-1)}\n' 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError

`sl undo -n 0` crashes too. With no undo or redo since the last change, a step of
0 targets index 0, the current state, and the command reads index -1 to name the
command it undid:

  $ sl undo -n 0 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl undo -a -n 0 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl undo -k -n 0 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{desc} {bookmarks}\n'
  second book
  $ sl status

After an `sl undo`, a step counts from the restored entry instead. `sl undo -n 0`
then works, but `sl undo -n -2` reaches index 0 and crashes the same way:

  $ sl undo -q --config hint.ack=undo-uncommit-unamend
  $ sl undo -n 0
  undone to *, before commit -Aqm second (glob)
  $ sl undo -n -2 2>&1 | egrep -v '^  '
  ** * has crashed: (glob)
  Traceback (most recent call last):
  IndexError
  $ sl log -r . -T '{desc} {bookmarks}\n'
  first book

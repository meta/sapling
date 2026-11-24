Test shortest() in a template of a command whose formatter has no "ctx"

  $ enable journal
  $ newclientrepo
  $ drawdag <<'EOS'
  > B
  > |
  > A
  > EOS
  $ sl go -q $A
  $ sl go -q $B

shortest() with a changeset context works as usual:

  $ sl log -r $B -T '{shortest(node)} {shortest(node, 8)}\n'
  1124 11247896

BUG: the journal formatter has no changeset context, and shortest() crashes
with a Python traceback:

  $ sl journal -l 2 -T '{shortest(newhashes)} {command}\n' 2>&1 | grep -E '^KeyError'
  KeyError: 'ctx'

BUG: so does the formatter of `sl files`:

  $ sl files -r $B -T '{shortest("112478962961")}\n' 2>&1 | grep -E '^KeyError'
  KeyError: 'ctx'

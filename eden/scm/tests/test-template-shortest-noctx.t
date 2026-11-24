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

The journal formatter has no changeset context, but `sl journal` gives its
template the repository, which is all shortest() needs:

  $ sl journal -l 2 -T '{shortest(newhashes)} {command}\n'
  1124 go -q 112478962961147124edd43549aedd1a335e44bf
  426b go -q 426bada5c67598ca65036d57d9e4b64b0c1ce7a0

The formatter of `sl files` has no changeset context and no repository:

  $ sl files -r $B -T '{shortest("112478962961")}\n'
  abort: shortest() is not supported in the template of this command
  [255]

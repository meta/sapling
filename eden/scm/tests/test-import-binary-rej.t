#require no-eden

A rejected GIT binary hunk must be saved to the .rej file, like a text hunk.

  $ newclientrepo
  $ echo text > text
  $ hg ci -Aqm text
  $ printf '\0\1\2\3binary\n' > bin
  $ hg ci -Aqm bin
  $ hg export --git -r . > $TESTTMP/bin.patch
  $ grep -A 3 'GIT binary' $TESTTMP/bin.patch
  GIT binary patch
  literal 14
  Vc${O1F^n;aF;2?NODwA70stK11Udi!

The patch creates "bin", but an untracked "bin" already exists: the binary
hunk is rejected and written to bin.rej.

  $ hg up -q 'desc(text)'
  $ printf 'other' > bin
  $ hg import --no-commit $TESTTMP/bin.patch
  applying $TESTTMP/bin.patch
  file bin already exists
  1 out of 1 hunks FAILED -- saving rejects to file bin.rej
  abort: patch failed to apply
  [255]
  $ cat bin.rej
  --- bin
  +++ bin
  GIT binary patch
  literal 14
  Vc${O1F^n;aF;2?NODwA70stK11Udi!
  $ cat bin
  other (no-eol)

The .rej is a valid patch again: once the conflicting file is gone it applies.

  $ rm bin bin.rej
  $ hg import --no-commit $TESTTMP/bin.patch
  applying $TESTTMP/bin.patch
  $ hg st
  A bin

Same with --partial: exit 1, partial commit, bin.rej written.

  $ hg revert -qa
  $ rm bin
  $ printf 'other' > bin
  $ hg import --partial $TESTTMP/bin.patch
  applying $TESTTMP/bin.patch
  file bin already exists
  1 out of 1 hunks FAILED -- saving rejects to file bin.rej
  patch applied partially
  (fix the .rej files and run `hg commit --amend`)
  [1]
  $ cat bin.rej
  --- bin
  +++ bin
  GIT binary patch
  literal 14
  Vc${O1F^n;aF;2?NODwA70stK11Udi!

The other reject path for a binary hunk: the target file is missing (-p 2
strips the directory, so the patch names a file that does not exist).

  $ newclientrepo
  $ mkdir dir
  $ printf '\0\1\2\3binary\n' > dir/bin
  $ hg ci -Aqm bin
  $ printf '\0\1\2\3changed\n' > dir/bin
  $ hg ci -qm change
  $ hg export --git -r . > $TESTTMP/change.patch
  $ hg up -q 'desc(bin)'
  $ hg import --no-commit -p 2 $TESTTMP/change.patch
  applying $TESTTMP/change.patch
  unable to find 'bin' for patching
  (use '--prefix' to apply patch relative to the current directory)
  1 out of 1 hunks FAILED -- saving rejects to file bin.rej
  abort: patch failed to apply
  [255]
  $ cat bin.rej
  --- bin
  +++ bin
  GIT binary patch
  literal 15
  Wc${O1F^n;aF;31%%u7#A;Q|05W(0o#

A GIT binary delta only applies to the file it was made against. It starts with
the sizes of that file and of the result, then builds the result by copying
ranges of that file and inserting new bytes. `writedelta` writes one as a patch
to f:

  >>> import base64, os, zlib
  >>> def header(srcsize, dstsize):
  ...     return bytes([srcsize, dstsize])
  >>> def copy(offset, size):
  ...     return bytes([0x80 | 0x01 | 0x10, offset, size])
  >>> def insert(data):
  ...     return bytes([len(data)]) + data
  >>> def writedelta(name, delta, githeader=b"diff --git a/f b/f\n"):
  ...     data = zlib.compress(delta)
  ...     # A base85 line starts with its byte count: A is 1, Z is 26.
  ...     assert len(data) <= 26
  ...     line = b"%c%s" % (ord("A") + len(data) - 1, base64.b85encode(data, True))
  ...     with open(os.path.join(os.environ["TESTTMP"], name), "wb") as f:
  ...         f.write(githeader + b"GIT binary patch\n")
  ...         f.write(b"delta %d\n%s\n\n" % (len(delta), line))

This one turns the 35-byte "hello world, this is the base file" into
"... BASE file":

  >>> writedelta("delta.patch", header(35, 35) + copy(0, 25) + insert(b"BASE") + copy(29, 6))

  $ newclientrepo
  $ printf 'hello world, this is the base file\n' > f
  $ hg ci -Aqm base
  $ hg import --no-commit $TESTTMP/delta.patch
  applying $TESTTMP/delta.patch
  $ cat f
  hello world, this is the BASE file

A delta whose copy runs past the end of its base is rejected, and f is left
alone.

  $ hg revert -q f
  >>> writedelta("past-end.patch", header(35, 8) + copy(32, 8))
# BUG: the delta is applied, and f is replaced with the last 3 bytes of the base,
# fixed in the next diff
  $ hg import --no-commit $TESTTMP/past-end.patch
  applying $TESTTMP/past-end.patch
  $ cat f
  le

On a different file the delta is rejected too, and f is left alone.

  $ newclientrepo
  $ printf 'short\n' > f
  $ hg ci -Aqm base
# BUG: the delta is applied, and f becomes neither its old nor its new content,
# fixed in the next diff
  $ hg import --no-commit $TESTTMP/delta.patch
  applying $TESTTMP/delta.patch
  $ cat f
  short
  BASE (no-eol)
  $ cat f.rej
  cat: f.rej: $ENOENT$
  [1]

The same delta can also rename f to g. On its own base, f is renamed and
patched:

  >>> writedelta("rename.patch", header(35, 35) + copy(0, 25) + insert(b"BASE") + copy(29, 6), b"diff --git a/f b/g\nrename from f\nrename to g\n")

  $ newclientrepo
  $ printf 'hello world, this is the base file\n' > f
  $ hg ci -Aqm base
  $ hg import -m rename $TESTTMP/rename.patch
  applying $TESTTMP/rename.patch
  $ hg st -C --change .
  A g
    f
  R f
  $ hg st
  $ cat g
  hello world, this is the BASE file

On a different file the rename is rejected, f is left alone, and g is not
created.

  $ newclientrepo
  $ printf 'short\n' > f
  $ hg ci -Aqm base
# BUG: f is deleted, and g gets neither the old nor the new content,
# fixed in the next diff
  $ hg import --no-commit $TESTTMP/rename.patch
  applying $TESTTMP/rename.patch
  $ hg st -C
  A g
    f
  R f
  $ cat f
  cat: f: $ENOENT$
  [1]
  $ cat g
  short
  BASE (no-eol)

Without --no-commit, nothing is committed.

  $ hg revert -qa
  $ rm -f g g.rej
# BUG: the rename is committed, fixed in the next diff
  $ hg import -m rename $TESTTMP/rename.patch
  applying $TESTTMP/rename.patch
  $ hg log -r . -T '{desc}\n'
  rename
  $ hg st -C

With --partial, the commit does not delete f.

  $ hg up -q 'desc(base)'
  $ rm -f g.rej
# BUG: the commit deletes f and adds the wrong g, fixed in the next diff
  $ hg import --partial -m rename $TESTTMP/rename.patch
  applying $TESTTMP/rename.patch
  $ hg st -C --change .
  A g
    f
  R f
  $ hg st -C

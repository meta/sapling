

  $ cat > makepatch.py <<'EOF'
  > f = open('eol.diff', 'wb')
  > w = f.write
  > _ = w(b'test message\n')
  > _ = w(b'diff --git a/a b/a\n')
  > _ = w(b'--- a/a\n')
  > _ = w(b'+++ b/a\n')
  > _ = w(b'@@ -1,5 +1,5 @@\n')
  > _ = w(b' a\n')
  > _ = w(b'-bbb\r\n')
  > _ = w(b'+yyyy\r\n')
  > _ = w(b' cc\r\n')
  > _ = w(b' \n')
  > _ = w(b' d\n')
  > _ = w(b'-e\n')
  > _ = w(b' No newline at end of file\n')
  > _ = w(b'+z\r\n')
  > _ = w(b' No newline at end of file\r\n')
  > EOF

  $ configure modern
  $ newclientrepo repo
  $ echo '*\.diff' > .gitignore


Test different --eol values

  $ printf "a\nbbb\ncc\n\nd\ne" > a
  $ sl ci -Am adda
  adding .gitignore
  adding a
  $ $PYTHON ../makepatch.py


invalid eol

  $ sl --config patch.eol='LFCR' import eol.diff
  applying eol.diff
  abort: unsupported line endings type: LFCR
  [255]
  $ sl revert -a


force LF

  $ sl --traceback --config patch.eol='LF' import eol.diff
  applying eol.diff
  $ cat a
  a
  yyyy
  cc
  
  d
  e (no-eol)
  $ sl st


force CRLF

  $ sl up -C 'desc(adda)'
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl --traceback --config patch.eol='CRLF' import eol.diff
  applying eol.diff
  $ cat a
  a\r (esc)
  yyyy\r (esc)
  cc\r (esc)
  \r (esc)
  d\r (esc)
  e (no-eol)
  $ sl st


auto EOL on LF file

  $ sl up -C 'desc(adda)'
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl --traceback --config patch.eol='auto' import eol.diff
  applying eol.diff
  $ cat a
  a
  yyyy
  cc
  
  d
  e (no-eol)
  $ sl st


auto EOL on CRLF file

  $ printf "a\r\nbbb\r\ncc\r\n\r\nd\r\ne" > a
  $ sl commit -m 'switch EOLs in a'
  $ sl --traceback --config patch.eol='auto' import eol.diff
  applying eol.diff
  $ cat a
  a\r (esc)
  yyyy\r (esc)
  cc\r (esc)
  \r (esc)
  d\r (esc)
  e (no-eol)
  $ sl st


auto EOL on new file or source without any EOL

  $ printf "noeol" > noeol
  $ sl add noeol
  $ sl commit -m 'add noeol'
  $ printf "noeol\r\nnoeol\n" > noeol
  $ printf "neweol\nneweol\r\n" > neweol
  $ sl add neweol
  $ sl diff --git > noeol.diff
  $ sl revert --no-backup noeol neweol
  $ rm neweol
  $ sl --traceback --config patch.eol='auto' import -m noeol noeol.diff
  applying noeol.diff
  $ cat noeol
  noeol\r (esc)
  noeol
  $ cat neweol
  neweol
  neweol\r (esc)
  $ sl st


Test --eol and binary patches

  $ printf "a\0\nb\r\nd" > b
  $ sl ci -Am addb
  adding b
  $ printf "a\0\nc\r\nd" > b
  $ sl diff --git > bin.diff
  $ sl revert --no-backup b

binary patch with --eol

  $ sl import --config patch.eol='CRLF' -m changeb bin.diff
  applying bin.diff
  $ cat b
  a\x00 (esc)
  c\r (esc)
  d (no-eol)
  $ sl st

a binary file ending in CRLF is written back unchanged

  $ printf "a\0\nc\r\n" > b
  $ sl diff --git > bin2.diff
  $ sl revert --no-backup b
  $ sl import --config patch.eol='CRLF' -m changeb2 bin2.diff
  applying bin2.diff
  $ $PYTHON -c "print(repr(open('b', 'rb').read()))"
  b'a\x00\nc\r\n'
  $ sl st

a binary delta applies to the exact bytes, CRLFs included: this one turns
"za\r\nb\r\n" into "za\r\nXb\r\n"

  $ printf "za\r\nb\r\n" > c
  $ sl ci -Aqm addc
  $ cat > delta.diff <<'EOF'
  > diff --git a/c b/c
  > GIT binary patch
  > delta 9
  > Qc-mv<n83mqF_DEC00*-Hl>h($
  > 
  > EOF
  $ sl import --config patch.eol='auto' -m changec delta.diff
  applying delta.diff
  $ $PYTHON -c "print(repr(open('c', 'rb').read()))"
  b'za\r\nXb\r\n'
  $ sl st
  $ cd ..

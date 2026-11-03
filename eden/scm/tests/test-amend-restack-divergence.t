
#require no-eden



  $ eagerepo
  $ configure mutation-norecord
  $ enable amend rebase
  $ mkcommit() {
  >   echo "$1" > "$1"
  >   hg add "$1"
  >   hg ci -m "add $1"
  > }

Test situation with divergence. Restack should rebase unstable children
onto the newest successor of their parent.
  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ sl prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add b (glob)
  $ sl amend -m "successor 1" --no-rebase
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'sl restack' to rebase them
  hint[hint-ack]: use 'sl hint --ack amend-restack' to silence these hints
  $ sl up --config checkout.obsolete-mode=ignore --config commit.modify-obsolete-mode=ignore 7c3bad9141dcb46ff89abf5f61856facd56e476c
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ sl amend -m "successor 2" --no-rebase
  warning: changing an old version of a commit will diverge your stack:
  - 7c3bad9141dc -> f60c1f15a70e (amend)
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'sl restack' to rebase them
  hint[hint-ack]: use 'sl hint --ack amend-restack' to silence these hints
  $ sl up --config checkout.obsolete-mode=ignore --config commit.modify-obsolete-mode=ignore 7c3bad9141dcb46ff89abf5f61856facd56e476c
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  cef323f40828 successor 2
  │
  │ o  f60c1f15a70e successor 1
  ├─╯
  │ o  4538525df7e2 add c
  │ │
  │ @  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ sl rebase --restack
  rebasing 4538525df7e2 "add c"
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  o  b0a0bc953ac3 add c
  │
  @  cef323f40828 successor 2
  │
  │ o  f60c1f15a70e successor 1
  ├─╯
  o  1f0dee641bb7 add a

Test situation with divergence due to an unamend. This should actually succeed
since the successor is obsolete.
  $ newrepo
  $ mkcommit a
  $ mkcommit b
  $ mkcommit c
  $ sl prev
  0 files updated, 0 files merged, 1 files removed, 0 files unresolved
  [*] add b (glob)
  $ echo b >> b
  $ sl amend
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'sl restack' to rebase them
  hint[hint-ack]: use 'sl hint --ack amend-restack' to silence these hints
  $ showgraph
  @  c54ee8acf83d add b
  │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ sl up --config checkout.obsolete-mode=ignore --config commit.modify-obsolete-mode=ignore 7c3bad9141dcb46ff89abf5f61856facd56e476c
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ echo c >> b
  $ sl amend
  warning: changing an old version of a commit will diverge your stack:
  - 7c3bad9141dc -> c54ee8acf83d (amend)
  hint[amend-restack]: descendants of 7c3bad9141dc are left behind - use 'sl restack' to rebase them
  hint[hint-ack]: use 'sl hint --ack amend-restack' to silence these hints
  $ showgraph
  @  2c965323ca2a add b
  │
  │ o  c54ee8acf83d add b
  ├─╯
  │ o  4538525df7e2 add c
  │ │
  │ v  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a
  $ sl unamend
  $ sl up -C c54ee8acf83d47ec674bca5bb6ba7be56227bd89
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  @  c54ee8acf83d add b
  │
  │ o  4538525df7e2 add c
  │ │
  │ x  7c3bad9141dc add b
  ├─╯
  o  1f0dee641bb7 add a

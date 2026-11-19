---
name: client-config-rollout
description: >
  How to ramp and measure a Source Control client rollout: an EdenFS config
  rollout (`edenfs_config.cconf`), a Sapling dynamic-config rollout
  (`hgclient.cinc`), or a GK-gated client feature. Use when planning,
  advancing, watching, or rolling back a client ramp, or when asked whether a
  rollout caused a crash, error, or command-failure regression.
metadata:
  oncalls:
    - 'scm_client_infra'
    - 'sapling'
  strict: true
  apply_to_user_prompt: '(?i)(edenfs_rollouts|loggedconfigs|hgclient\.cinc|edenfs_config\.cconf|(ramp|rollout|roll(ed|ing)? ?back).*(eden|sapling|hgrc|gatekeeper|\bgk\b)|(eden|sapling|hgrc|gatekeeper|\bgk\b).*(ramp|rollout|roll(ed|ing)? ?back))'
---

# Client config rollouts: ramp and measure

EdenFS rollouts live in configerator `scm/eden/config/edenfs_config.cconf`
(filters in `edenfs_config_filters.cinc`); Sapling rollouts in
`scm/hg/hgclientconf/hgclient.cinc`. Baselines and open ramps go stale, so they
are not here: keep them in a dated doc and re-run the queries before applying a
threshold. Thresholds marked *(proposal)* are not yet agreed by the team.

When measuring a stage, read [references/queries.md](references/queries.md)
for the exposure and control-relative Scuba queries.

## Before stage 1

- **EdenFS:** name the rollout (`.rollout(expr, rollout_name=...)`); an unnamed
  rollout never reaches `edenfs_rollouts`. Check whether the daemon reads the
  key live (it re-reads config every `config:reload-interval`, 5 min) or only
  at startup; a startup-only key reaches a host at its next `eden restart`.
- **Sapling:** add the key to `[loggedconfigs]` (`<column> = <section>.<key>`,
  under `shard(100)`) in `hgclient.cinc`, land it, and confirm the column with
  `meta scuba.dataset info --dataset=dev_command_timers --columns=<column>`.
  Without it the key has no exposure log. A value that is not exactly
  `section.key` is skipped (`log_logged_configs` in
  `fbcode/eden/scm/lib/commands/src/run.rs`).
- **GK:** prefer a `gks=[...]` leg on a named EdenFS rollout (only `cpe_` GKs
  work there) so exposure lands in `edenfs_rollouts`. A GK applied by Chef
  (`cpe_gk/recipes/eden.rb`) or checked in product code (ISL, VS Code, `pull`)
  has no per-host exposure log the team can read; `meta experimentation.gk
  pass-rate` is an aggregate ratio, not exposure.
- **Arms:** exposed = hosts or users the gate's own log shows on the new value,
  never group membership. Control = a random sample of the rest of the same
  eligible cohort, never the top-N by activity. If the control arm is
  near-empty, the metric is unmeasurable for this ramp: say so.
- **Team legs:** `hg_dev`, `scm_beta`, and `team_devservers` are not random
  samples: they crash more and share incidents. Judge them apart from random
  legs. For EdenFS, pick a named rollout whose filter is only those team
  cohorts as `<TEAM>` in the queries.
- **At-risk group:** write the query for hosts or users that used the old path
  in the prior week (platform, mount protocol, repo, command) and report it on
  its own at every check.
- **Feature signals:** list the counters that show the feature doing its job
  (how often it runs, how much it does, how often it fails) and, for a switch
  on the data path, a correctness check. If neither exists, say so now.

## Stages and waits

Usual order: team (`hg_dev`, team devservers or Macs), `scm_beta`, then
10/25/50/75/100% steps 2-5 days apart or a TimeShard; Sandcastle on its own
(random seed, 10% first). A stage starts at its first exposure row, not at the
commit time; hosts on the new value before the land are canaries or an earlier
leg, so drop them from the arm. Judge a stage only after propagation:

| area | propagation | read exposure no earlier than |
|---|---|---|
| EdenFS | config manager every 15 min (devservers, laptops, OD, ASIC), daemon reload within 5 min; Sandcastle and TW only at job or task start; startup-only keys at restart | 20 min after land |
| Sapling | a host refreshes only after a command runs on config older than 900 s: roughly 70% in 1.5 h, 94% in 10 h; `machine_tier = 'tw'` hosts stay stale for a day or more | 10 h, and the hourly new-value host count has stopped growing; report `tw` apart |
| GK | Chef `cpe_gk` run (about 30 min), then the EdenFS path | 1 h |

## Before leaving the team stage

Crash, error, and exit-code metrics do not see silent data corruption or a
feature that runs more often than intended. Before moving past the team stage:

- Read the feature's own counters against what it should do. For example, for
  cgroup file-cache reclaim: how often reclaim ran and how much it freed.
- For a switch on the data path (tree or file fetch, decoding, overlay format),
  run a correctness check on a sample of team hosts: compare content from the
  new path with the old one, or count accepted hash mismatches.
- If the feature has neither, name the gap in the stage report: the hold rules
  below cannot see these failures.

## Hold rules

Always: an error or crash hold needs at least 3 users and must still trip after
removing the top user and their hosts or commands (a `user` of `unknown`
counts as one user). Check the at-risk group alone. Compare when the rise began
with the land time: a rise that began before the land is not the rollout's.
Before calling old-value traffic a regression, check how long the process has
run (EdenFS daemons on startup-only keys, long-running `sl`). Rule out test
traffic, probes, and one heavy user before tagging anyone.

- **EdenFS crashes** *(proposal)*: count only random legs as exposed and report
  team legs on their own. To advance, judge a window of at least 3 days: hold
  when the exposed crash-host rate is at least 2x control's and was under 1.5x
  in the prior window. In the first 24 h, hold early when the random legs have
  at least 5 crash hosts from at least 3 users, at a rate at least 5x
  control's, and the first of those crashes came after land plus propagation.
  At the team stage there is no random leg: rely on the feature's signals.
  Investigate any core with frames in the changed code before advancing,
  whatever the count (stacks: the crash plumber in
  `fbcode/eden/fs/scripts/facebook/crash/`).
- **EdenFS errors** *(proposal)*: per error type, hold when at least 10 exposed
  hosts have it in the window and the change ratio (exposed hosts now /
  exposed hosts prior) / (control hosts now / control hosts prior) is at least
  1.5; use 1 for any zero count, which lets a type that is new in the exposed
  arm trip too. Each arm is compared with its own prior window, so arm sizes
  cancel. Report a type on fewer than 10 exposed hosts, but do not hold on it.
- **Sapling** *(proposal)*: hold when the share of exposed users whose failure
  rate rose 5 pp is at least 1.5x control's and at least 2 pp above it, or the
  median per-user change is 0.5 pp above control's. Compare the same weekdays.
- **GK:** apply the EdenFS or Sapling rule to what the GK changes. Without an
  exposure log, stop at `scm_beta` *(proposal)*.

Not covered: Sandcastle legs (barely present in `edenfs_rollouts`), macOS and
Windows EdenFS crashes, Sapling crashes, and latency. Name the gap instead of
judging those.

## Kill switch

- **EdenFS:** land a cconf change so the rollout matches no host (reviewers
  #scm_client). Live keys revert within one config-manager run plus a reload;
  startup-only keys need `eden restart`; Sandcastle and TW at their next job or
  task. SEV push to hosts: wiki "Authoring and Testing Configs". Check one host
  with `edenfsctl fsconfig --all`.
- **Sapling:** append `.back("<window>", shard(100))` to the `Roll` (never edit
  an old leg) and expect the propagation tail above. `sl debugrefreshconfig`
  forces a refresh on one host; `sl config <section>.<key>` shows the value.
- **GK:** set the GK to 0% (review required); allow an hour for Chef and the
  EdenFS config manager.

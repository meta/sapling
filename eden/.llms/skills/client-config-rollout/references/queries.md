# Rollout queries

Run with `meta scuba.dataset sql --dataset=<first table> --sql-file=file://<path>`.
Peregrine has no nested subqueries or window functions: fetch rows, then
compute arms, medians, and the top-user drop locally.

Every query, and each side of a join, bounds `time < NOW()`. To replay a past
decision, add `--test-datetime=<YYYY-MM-DDTHH:MM:SS-0700>` (for example
`2026-10-19T12:40:00-0700`): it moves only `NOW()`, so without that bound the
query also reads rows from after the decision.

Placeholders: `<R>` the rollout being ramped; `<REF>` a rollout at 100% of the
cohort `<R>` ramps in (same platform; for `edenfs_errors`, the rollout that
enables error logging), so control = REF minus R; `<TEAM>` a named rollout
whose filter is only the team cohorts (`hg_dev`, `scm_beta`,
`team_devservers`) that `<R>` has legs in; `<W>` the judged window in seconds,
at most 604800. The prior window is the same span a week earlier. Check Q-E5
first: if the prior window holds a crash spike (a day at 3x the median or
more), an incident, or a release, use two weeks back or the median of three
prior weeks, and say so. Arms use the last 30 min of membership, so keep `<W>`
inside one stage.

## EdenFS

**Q-E1 exposure and propagation.** One row per host per config-manager run per
matched rollout: the filter matched, not proof the daemon applied the value.
The stage starts at the first bucket with `<R>` rows after the land.

```sql
SELECT `time` - (`time` % 900) AS t, `rollout`, APPROX_COUNT_DISTINCT_HLL(`host`) AS hosts,
  APPROX_COUNT_DISTINCT_HLL(`user`) AS users
FROM edenfs_rollouts WHERE `time` >= NOW()-86400 AND `time` < NOW()
  AND `rollout` IN ('<R>', '<REF>', '<TEAM>')
GROUP BY `time` - (`time` % 900), `rollout`
```

**Q-E2 crash hosts per arm** (Linux `coredumper`). Count hosts, never cores: a
crash-looping host dominates core counts. Locally: random legs = `exposed`
and not `team`; team legs = `exposed` and `team`; control = `in_ref` and not
`exposed`. Denominators come from the newest complete Q-E1 bucket (the last
one is usually partial; random legs: R minus TEAM); rate = crash hosts per 10k
hosts per arm and window. For the
first-24h rule, compare `first_crash` with land plus propagation.

```sql
SELECT c.host AS h, MAX(r.user) AS usr, MAX(IF(r.rollout = '<R>', 1, 0)) AS exposed,
  MAX(IF(r.rollout = '<TEAM>', 1, 0)) AS team, MAX(IF(r.rollout = '<REF>', 1, 0)) AS in_ref,
  MAX(IF(c.time >= NOW()-<W>, 1, 0)) AS crashed_now, MAX(IF(c.time < NOW()-604800, 1, 0)) AS crashed_prior,
  MIN(IF(c.time >= NOW()-<W>, c.time, NOW())) AS first_crash
FROM coredumper AS c JOIN edenfs_rollouts AS r ON c.host = r.host
WHERE c.time >= NOW()-604800-<W> AND c.time < NOW() AND (c.time >= NOW()-<W> OR c.time < NOW()-604800)
  AND c.binary = 'edenfs' AND r.time >= NOW()-1800 AND r.time < NOW()
  AND r.rollout IN ('<R>', '<REF>', '<TEAM>')
GROUP BY h
```

**Q-E3 error hosts per arm and type.** The 30-min `edenfs_rollouts` side keeps
the join small; a day-long side fails. Count hosts, never weighted rows: a few
hosts log most errors. Control per type = REF row minus R row.

```sql
SELECT r.rollout AS arm, e.component AS comp, e.error_type AS etype,
  APPROX_COUNT_DISTINCT_HLL(IF(e.time >= NOW()-<W>, e.host, NULL)) AS hosts_now,
  APPROX_COUNT_DISTINCT_HLL(IF(e.time < NOW()-604800, e.host, NULL)) AS hosts_prior,
  APPROX_COUNT_DISTINCT_HLL(IF(e.time >= NOW()-<W>, e.user, NULL)) AS users_now
FROM edenfs_errors AS e JOIN edenfs_rollouts AS r ON e.host = r.host
WHERE e.time >= NOW()-604800-<W> AND e.time < NOW() AND (e.time >= NOW()-<W> OR e.time < NOW()-604800)
  AND r.time >= NOW()-1800 AND r.time < NOW() AND r.rollout IN ('<R>', '<REF>')
GROUP BY arm, comp, etype
```

If Q-E3 fails with "Exceeded Maximum Join Memory Limit" (it has on a
50k-host rollout, even at W = 1 day), run Q-E3a and Q-E3b and join them
locally on host: exposed = `exposed` and `in_ref`, control = `in_ref` and not
`exposed`.

```sql
SELECT `host`, MAX(IF(`rollout` = '<R>', 1, 0)) AS exposed, MAX(IF(`rollout` = '<REF>', 1, 0)) AS in_ref
FROM edenfs_rollouts WHERE `time` >= NOW()-1800 AND `time` < NOW() AND `rollout` IN ('<R>', '<REF>')
GROUP BY `host`
```

```sql
SELECT `host`, `component` AS comp, `error_type` AS etype, MAX(`user`) AS usr,
  MAX(IF(`time` >= NOW()-<W>, 1, 0)) AS seen_now, MAX(IF(`time` < NOW()-604800, 1, 0)) AS seen_prior
FROM edenfs_errors
WHERE `time` >= NOW()-604800-<W> AND `time` < NOW() AND (`time` >= NOW()-<W> OR `time` < NOW()-604800)
GROUP BY `host`, `component`, `error_type`
```

**Q-E4 top-user drop** for a type that trips Q-E3:

```sql
SELECT e.user AS u, APPROX_COUNT_DISTINCT_HLL(e.host) AS hosts, SUM(1, e.weight) AS errors
FROM edenfs_errors AS e JOIN edenfs_rollouts AS r ON e.host = r.host
WHERE e.time >= NOW()-<W> AND e.time < NOW() AND e.error_type = '<etype>'
  AND r.time >= NOW()-1800 AND r.time < NOW() AND r.rollout = '<R>'
GROUP BY u ORDER BY hosts DESC
```

**Q-E5 fleet crash hosts per day** (UTC days), to vet a prior window:

```sql
SELECT `time` - (`time` % 86400) AS d, APPROX_COUNT_DISTINCT_HLL(`host`) AS crash_hosts
FROM coredumper WHERE `time` >= NOW()-2419200 AND `time` < NOW() AND `binary` = 'edenfs'
GROUP BY `time` - (`time` % 86400)
```

## Sapling

**Q-S1 exposure and propagation.** `<column>` is the key's `[loggedconfigs]`
name.

```sql
SELECT `time` - (`time` % 3600) AS t, `<column>`, `machine_tier` = 'tw' AS tw,
  APPROX_COUNT_DISTINCT_HLL(`host`) AS hosts, APPROX_COUNT_DISTINCT_HLL(`user`) AS users
FROM dev_command_timers WHERE `time` >= NOW()-86400 AND `time` < NOW() AND `source` = 'hg'
GROUP BY `time` - (`time` % 3600), `<column>`, `machine_tier` = 'tw'
```

**Q-S2 per-user failure rate against the user's own prior week.** Per user
because `host` is kept only about 7 days. `is_bot` and `hg_time` are
UI-derived columns and come back empty in SQL, so bots are filtered by name.

```sql
SELECT `user`, SUM(IF(`time` >= NOW()-<W> AND `<column>` = '<new>', 1, 0), `sample_rate`) AS new_cmds,
  SUM(IF(`time` >= NOW()-<W>, 1, 0), `sample_rate`) AS cmds_now,
  SUM(IF(`time` >= NOW()-<W> AND `exit_code` <> 0, 1, 0), `sample_rate`) AS fails_now,
  SUM(IF(`time` < NOW()-604800, 1, 0), `sample_rate`) AS cmds_prior,
  SUM(IF(`time` < NOW()-604800 AND `exit_code` <> 0, 1, 0), `sample_rate`) AS fails_prior
FROM dev_command_timers
WHERE `time` >= NOW()-604800-<W> AND `time` < NOW() AND (`time` >= NOW()-<W> OR `time` < NOW()-604800)
  AND `source` = 'hg' AND `repo` = '<repo>'
  AND `user` NOT IN ('root', 'svcscm', 'twsvcscm', 'facebook') AND `user` NOT LIKE 'chronos_%'
GROUP BY `user` HAVING new_cmds > 0 OR MIN(ABS(HASH(`user`)) % 10) = 0
```

Then, locally:

- Exposed = users with `new_cmds` at least 95% of `cmds_now`. Host-hash shards
  split most users across arms, so drop mixed users. Control = `new_cmds` = 0,
  which the `HAVING` clause keeps as a random 10% sample.
- Keep users with at least 20 weighted commands in both windows.
- Per-user change = `fails_now/cmds_now - fails_prior/cmds_prior`. Report the
  median change per arm and the share of users whose rate rose at least 5 pp;
  the median is usually 0 for failure rate, so the share is what moves.

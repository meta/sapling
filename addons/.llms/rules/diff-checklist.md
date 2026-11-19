---
name: isl-diff-checklist
metadata:
  oncalls: ['scm_client_infra']
  strict: true
  apply_to_path: 'eden/addons/(isl|isl-server|shared|components|vscode)/.*\.(ts|tsx|css)$'
---

# ISL Diff Checklist

These are the changes ISL reviewers ask for most often. Check them before requesting review. They add to the Code Review Guidelines in `llms.md`.

- **Exported code:** everything outside a `facebook/` directory is exported to GitHub. Internal-only features go under `facebook/` and are reached through `Internal`; code that uses `Internal.x` must still work when it is `undefined`. Keep internal product names out of exported files. A feature flag does not hide a feature from OSS users.
- **Reuse first:** check `shared/`, `components/` and `isl/src/ComponentUtils.tsx` before adding a helper or component.
- **Commits in commands:** pass `succeedableRevset` / `exactRevset` objects (see `latestSuccessorUnlessExplicitlyObsolete`), not hash strings or `.`. On the server, mutating commands go through `runOrQueueOperation`, not `runCommand`.
- **Fetch cost:** don't add `sl` calls, awaits or per-commit fields to the commit fetch when existing data can answer.
- **Stored state:** don't rename a `LocalStorageName` key or change its stored shape without migrating old values.
- **Old `sl`:** ISL often runs against an older `sl`. New flags, template keywords and revset functions need a fallback.
- **Text and theme:** user-facing text goes in `<T>` / `t()`. Colors and spacing come from theme tokens; check a light and a dark theme.
- **Types:** no `any`, and no sentinel values (`0`, `''`) for "missing"; use `undefined` or `null`.
- **UI scope:** disable with a tooltip instead of hiding. Don't add top-bar or settings entries for a niche feature.
- **Verify:** run `./verify-addons-folder.js`. For a visible change, attach a screenshot or video to the test plan and say how you ran it. Resolve AI review findings before asking a person to review.

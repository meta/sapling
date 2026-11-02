---
name: llms
metadata:
  oncalls: ['nuclide']
  strict: true
---

# Interactive Smartlog (ISL)

ISL is a web-based UI for the [Sapling](https://sapling-scm.com/) source control system. It runs as a React client communicating with a Node.js server that wraps the `sl` CLI. ISL is embedded in VS Code, Android Studio, Visual Studio, Obsidian, and runs standalone in the browser.

## Repository Structure

```
eden/addons/
├── isl/             # React client (Vite + TypeScript + Jotai)
├── isl-server/      # Node.js server (Rollup + TypeScript)
├── shared/          # Utilities shared between client and server
├── components/      # Reusable UI component library
├── vscode/          # VS Code extension that hosts ISL
├── textmate/        # TextMate grammar files
└── scripts/         # Build and sync scripts
```

## Development Commands

| Task                     | Command                              |
| ------------------------ | ------------------------------------ |
| Install dependencies     | `yarn install` (from `eden/addons/`) |
| Run dev server (browser) | `yarn dev`                           |
| Run client tests         | `cd isl && yarn test`                |
| Run server tests         | `cd isl-server && yarn test`         |
| Run integration tests    | `cd isl && yarn integration`         |
| Format code              | `arc f -a`                           |
| Lint                     | `yarn lint`                          |
| Type check (per package) | `yarn tsc --noEmit`                  |

**TypeScript:** packages type-check with TypeScript 7 (`yarn tsc`). TypeScript 7 has no JavaScript API, so tools that need one (ts-jest, `prettier-plugin-organize-imports`) use the root `typescript` 5.x or `@typescript/typescript6`.

**Prettier config:** single quotes, 2-space indent, 100 print width, trailing commas, no bracket spacing. Imports are auto-organized via `prettier-plugin-organize-imports`.

---

## Architecture

### Client-Server Communication

ISL uses a **typed message bus** for bidirectional communication:

- **Message types** are defined as discriminated unions (with a `type` field) in `isl/src/types.ts` (`ClientToServerMessage` and `ServerToClientMessage`).
- **Client side:** `ClientToServerAPI` (`isl/src/ClientToServerAPI.ts`) wraps the `MessageBus` interface. Use `serverAPI.postMessage()` to send and `serverAPI.onMessageOfType()` to listen.
- **Server side:** `ServerToClientAPI` (`isl-server/src/ServerToClientAPI.ts`) handles incoming messages and sends responses.
- **Serialization:** Custom `serialize.ts` / `deserialize` handles `Map`, `Set`, `Date`, and `Error` types over the wire.

**Subscription pattern:** Long-lived data streams (uncommitted changes, smartlog commits, merge conflicts) use a subscription model:

```typescript
// Client subscribes
{type: 'subscribe', kind: 'smartlogCommits', subscriptionID: '...'}
// Server pushes updates
{type: 'subscriptionResult', kind: 'smartlogCommits', subscriptionID: '...', data: {...}}
```

When adding new message types, add them to the union types in `isl/src/types.ts`. The server imports these types from `isl` via path aliases, ensuring type safety across the boundary.

### Platform Abstraction

ISL supports multiple host environments via the `Platform` interface (`isl/src/platform.ts`). Each platform provides implementations for file opening, clipboard, theming, persistence, and the message transport.

| Platform       | Client Platform                            | Server Platform                  |
| -------------- | ------------------------------------------ | -------------------------------- |
| Browser        | `BrowserPlatform.ts`                       | `chromelikeAppServerPlatform.ts` |
| VS Code        | `vscode/webview/vscodeWebviewPlatform.tsx` | `webviewServerPlatform.ts`       |
| Android Studio | Entry via `androidStudio.html`             | `androidstudioServerPlatform.ts` |
| Visual Studio  | Entry via `visualStudio.html`              | `visualStudioServerPlatform.ts`  |
| Obsidian       | Entry via `obsidian.html`                  | `obsidianServerPlatform.ts`      |
| Agent Home     | Entry via `agentHome.html`                 | `agentHomeServerPlatform.ts`     |
| Agent Cloud    | Entry via `agentCloud.html`                | `agentCloudServerPlatform.ts`    |

Platform-specific code must go through the `Platform` interface. Do not import platform implementations directly—use `import platform from './platform'`.

---

## State Management (Jotai)

ISL uses **Jotai** for all client-side state. Key conventions:

### Custom Atom Utilities (`jotaiUtils.ts`)

| Utility                  | Purpose                                             |
| ------------------------ | --------------------------------------------------- |
| `configBackedAtom`       | Atom synced with `sl` config via server messages    |
| `localStorageBackedAtom` | Atom persisted to `localStorage`                    |
| `atomWithOnChange`       | Atom with side-effect callback on value change      |
| `atomFamilyWeak`         | WeakRef-cached atom family to prevent memory leaks  |
| `lazyAtom`               | Lazily initialized async atom                       |
| `atomResetOnCwdChange`   | Atom that resets when the working directory changes |

### Rules

- Use `readAtom()` and `writeAtom()` helpers instead of accessing `store.get`/`store.set` directly.
- Connect atoms to server data via `serverAPI.onMessageOfType()` and `registerDisposable()`.
- Derived state should be computed via Jotai derived atoms, not React `useMemo`.
- Do not store derived/computed values in writable atoms—use read-only derived atoms instead.

---

## Operations Pattern

Repository-mutating commands (commit, amend, rebase, goto, etc.) are modeled as `Operation` subclasses in `isl/src/operations/`.

```typescript
import {Operation} from './Operation';

class MyOperation extends Operation {
  constructor(private args: string[]) {
    super('MyOperationEvent'); // TrackEventName for analytics
  }

  getArgs(): Array<CommandArg> {
    return ['my-command', ...this.args];
  }

  // Optional: show optimistic UI while operation runs
  optimisticDag(dag: Dag): Dag {
    return dag.replaceWith(/* ... */);
  }
}
```

**Key methods on `Operation`:**

- `getArgs()` — (required) returns the `sl` CLI arguments
- `getStdin()` — optional stdin to pipe
- `previewDag(dag)` — modifies the DAG for pre-confirmation preview
- `optimisticDag(dag)` — modifies the DAG for post-confirmation optimistic state
- `makeOptimisticUncommittedChangesApplier()` — optimistic changes to file status

**Running operations:** Use the `useRunOperation()` hook:

```typescript
const runOperation = useRunOperation();
runOperation(new RebaseOperation(source, dest));
```

Operations run one at a time via a queue managed in `operationsState.ts`.

---

## Styling

ISL uses **CSS Modules** for component styling.

```typescript
import * as styles from './MyComponent.module.css';

<div className={styles.container} />
```

- Use `cn` to join multiple classNames
- CSS layers are used to manage specificity for the component library. e.g. `components/` should use `@layer components`.
- Theme tokens are defined in `components/theme/tokens.css`.
- Use CSS custom properties (e.g., `var(--foreground)`, `var(--pad)`) for theming.
- The `components/` package provides reusable UI primitives (`Button`, `Tooltip`, `TextField`, `Dropdown`, etc.)—use them instead of raw HTML elements.

---

## Testing

### Test Framework

- **Jest** with `ts-jest` preset, `jsdom` environment for client tests.
- **React Testing Library** for component tests (`@testing-library/react`).
- Mocks are reset between tests (`resetMocks: true` in jest config).

### Client Test Helpers (`isl/src/testUtils.tsx`)

| Helper                               | Purpose                                      |
| ------------------------------------ | -------------------------------------------- |
| `simulateMessageFromServer(msg)`     | Simulate a server→client message             |
| `simulateCommits(commits)`           | Simulate smartlog commit data                |
| `simulateRepoConnected()`            | Simulate successful repo connection          |
| `expectMessageSentToServer(msg)`     | Assert a specific message was sent to server |
| `expectMessageNOTSentToServer(msg)`  | Assert a message was NOT sent                |
| `simulateServerDisconnected()`       | Simulate server disconnection                |
| `COMMIT(hash, title, parent, info?)` | Create a commit fixture                      |
| `TEST_COMMIT_HISTORY`                | Standard commit tree fixture for tests       |

### Server Test Patterns

- Mock shell execution with `mockEjeca` to simulate `sl` CLI responses:

  ```typescript
  mockEjeca([
    [/^sl root/, {stdout: '/repo'}],
    [/^sl log/, {stdout: '...'}],
  ]);
  ```

- Mock `WatchForChanges` to avoid filesystem/watchman dependencies.

### Shared Test Utilities (`shared/testUtils.ts`)

- `MockLogger` — logger that captures output for assertions
- `nextTick()` — wait for microtask queue to flush
- `clone(obj)` — deep clone for test isolation

---

## Key Files Reference

| File                               | Purpose                                                   |
| ---------------------------------- | --------------------------------------------------------- |
| `isl/src/types.ts`                 | All core types: message types, commit types, config names |
| `isl/src/ClientToServerAPI.ts`     | Client-side typed message API                             |
| `isl/src/serverAPIState.ts`        | Jotai atoms synced with server subscriptions              |
| `isl/src/jotaiUtils.ts`            | Custom Jotai atom utilities                               |
| `isl/src/operationsState.ts`       | Operation queue and execution state                       |
| `isl/src/previews.ts`              | Optimistic UI / preview state                             |
| `isl/src/platform.ts`              | Platform abstraction interface                            |
| `isl/src/serialize.ts`             | Custom serialization for message bus                      |
| `isl/src/dag/`                     | Commit DAG data structure and rendering                   |
| `isl/src/operations/`              | All Operation subclasses                                  |
| `isl/src/codeReview/`              | Code review integration (GitHub, Phabricator)             |
| `isl/src/stackEdit/`               | Interactive stack editing                                 |
| `isl/src/CommitInfoView/`          | Commit details sidebar                                    |
| `isl-server/src/Repository.ts`     | Core server-side repo abstraction                         |
| `isl-server/src/OperationQueue.ts` | Server-side operation execution                           |
| `shared/types/common.ts`           | Shared types: `Hash`, `RepoPath`, `Author`                |
| `shared/utils.ts`                  | Shared utilities: `nullthrows`, `randomId`, `defer`       |

---

## Code Review Guidelines

When reviewing ISL changes, flag these issues:

- **New message types** not added to the discriminated unions in `types.ts`
- **Direct `store.get`/`store.set`** instead of `readAtom`/`writeAtom`
- **Operations that don't implement `optimisticDag`** when the result is predictable
- **Platform-specific code** outside the `Platform` interface
- **Missing test helpers** — using raw message simulation instead of `testUtils.tsx` helpers
- **Synchronous I/O** in server code — all file and process operations should be async
- **Raw HTML elements** instead of `components/` primitives (Button, Tooltip, etc.)

---

## Diff Conventions

- **Diff titles** for ISL changes must start with the `[isl]` prefix. For example: `[isl] Fix optimistic state for rebase operations`.
- **Commit messages are exported to GitHub.** Keep the title and summary about the ISL codebase only. Put anything about internal-only embeddings, features, or tools in an `Internal:` section at the end of the summary, which is not exported.

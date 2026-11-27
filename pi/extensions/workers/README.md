# Workers Extension

Lets the agent ask piton's daemon to create, list, and delete workers (copy-on-write clones) of its session's project.
Registered only in a session piton's daemon launched.

| Tool | Input | Asks the user |
| --- | --- | --- |
| `create_worker` | `name`, `label?`, `item_id?` | as `[workers.permissions] create_by_agent` says |
| `list_workers` | none | never |
| `delete_worker` | `path` | as `[workers.permissions] delete_by_agent` says |

- **The daemon decides, never pi.** The tools send `worker_create`, `worker_list`, and `worker_delete` on the session's
  channel (`__lib/agent.ts`). The daemon reads `[workers.permissions]` (`"ask"`, `"allow"`, or `"deny"`; unset asks) and
  asks the user in the session: once, for this session, always (saves `"allow"`), or deny. The permissions extension
  leaves these tools alone, so `[tools.permissions]` rules don't apply to them and the user is never asked twice.
- **Its own project only:** the session's source checkout and that source's workers.
- **Follow-ups:** a source update that changes protected files, and, on delete, uncommitted or unpushed work ("Delete
  anyway", every time).
- **Deleting the worker the session runs in** ends the session; the worker is still deleted.
- Results are short text for the model, with the typed outcome (`done`, `refused` with the daemon's `WorkerRefusal`, or
  `notDone` with why) in `details`.

See "Workers" in `docs/design.md` and `docs/daemon.md`.

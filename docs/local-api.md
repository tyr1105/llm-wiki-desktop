# Local automation API v1

The local API runs inside the desktop backend process, using the same AppState, authority checks, import engine, workflow queue, candidate review, Git checkpoints, and consumption records as the UI. `--serve-api --headless` starts it without creating a main window. This is a background Tauri host, not a separate copy of the workflow services.

## Security and ownership

- The listener is opt-in and binds only `127.0.0.1`. Every endpoint, including health and the OpenAPI document, requires a Bearer token. The default API-enabled build holds this lease; older builds that do not implement it must be closed before starting an API host. Tokens are loaded from the OS credential store, or an explicit environment override for ephemeral/test instances; never logged or written to a vault.
- Reject all browser Origin headers, unexpected Host authorities, oversized bodies, and unauthenticated requests before business dispatch. No CORS or arbitrary command endpoint is provided.
- Startup project-root and import-root allowlists freeze canonical directory identities. Request preflight and asynchronous discovery recheck these identities and each path before/after fingerprinting. Replacing or redirecting a startup root revokes that scope. Project open follows existing native/compatible authority policy: existing strict native projects receive native trust; compatible vaults require their prior durable trust grant. API cannot grant trust or repair/reorganize ordinary directories. Temporary preview sessions are excluded; create uses the existing write permit without UI preview fallback.
- The API-enabled build holds an OS instance lease before initializing backend state or recovering tasks. GUI and API share one authoritative process; another instance fails busy. This avoids independent queues and incomplete cross-process trust revocation.
- Import, external model execution, candidate confirmation, and writes retain their existing permission, source-version, revision, and checkpoint checks. API does not manufacture consumption records or implicitly acknowledge remote providers/restricted content.

## Contract

Base URL: `http://127.0.0.1:<port>/api/v1`. JSON uses the existing camelCase DTO fields. Responses carry `{data, requestId}` or `{error, requestId}` with the original structured BackendError. Long operations return HTTP 202 and an existing task identity; completion is queried separately. Polling never means completion.

| Method | Resource | Behavior |
| --- | --- | --- |
| GET | `/health` | Protocol/version and running process readiness |
| GET | `/openapi.json` | Machine-readable API schema |
| POST | `/projects/open` | Open an explicitly allowed existing vault |
| GET | `/projects/{projectId}/sources` | Paged current Source/version/consumption metadata |
| GET | `/projects/{projectId}/sources/{sourceId}` | Read Source detail and immutable version/asset bindings |
| POST | `/projects/{projectId}/imports` | Create an import session |
| GET | `/projects/{projectId}/imports/{sessionId}` | Read session and item state |
| GET | `/projects/{projectId}/imports/{sessionId}/overview` | Read selection revision and confirmation digest for commit |
| POST | `/projects/{projectId}/imports/{sessionId}/paths` | Start native file/folder discovery with immutable fingerprints |
| GET | `/projects/{projectId}/imports/{sessionId}/scans/{taskId}` | Read discovered files and required confirmation |
| POST | `/projects/{projectId}/imports/{sessionId}/scans/{taskId}/accept` | Accept the exact native scan token |
| POST | `/projects/{projectId}/imports/{sessionId}/inputs` | Add URL inputs within native import policy |
| POST | `/projects/{projectId}/imports/{sessionId}/text` | Add supplied text |
| POST | `/projects/{projectId}/imports/{sessionId}/parse` | Start cancellable batch parsing |
| POST | `/projects/{projectId}/imports/{sessionId}/commit` | Commit selected ready items using original revision/digest checks |
| GET | `/projects/{projectId}/tasks/{taskId}` | Read actual task result/error |
| POST | `/projects/{projectId}/tasks/{taskId}/cancel` | Cancel a native task through its authority checks |
| POST | `/projects/{projectId}/workflows/update-wiki` | Queue Update Wiki with explicit intent/requestId |
| GET | `/projects/{projectId}/workflows/{taskId}` | Status, stages, pending review and result |
| POST | `/projects/{projectId}/workflows/{taskId}/diff` | Page a candidate's concrete diff |
| POST | `/projects/{projectId}/workflows/{taskId}/confirm` | Confirm the exact pending action after review |
| POST | `/projects/{projectId}/workflows/{taskId}/cancel` | Cancel/discard through the existing coordinator |

All mutation requests require `Idempotency-Key`. A key is bound to method, resource and body digest; reuse with changed input returns 409. Transport retries return the original result in the same API process. Update Wiki also uses its durable native requestId. After process restart, query actual import/task/source receipts before issuing a new write: import transport deduplication is not claimed durable across restart.

Project root paths and project IDs are bound by the server after open; request bodies cannot override the route's ownership. HTTP does not accept arbitrary manifests, compile consumption, capability permits, or shell commands.

## Acceptance

Use disposable knowledge bases for automated tests. Verify authentication/Origin/Host/body limits, scope/path escape, concurrent instance exclusion, unchanged-input retry, changed-key conflict, untrusted writes, stale confirmation and actual import/compile receipts. Verify a production build starts headless without desktop automation before using it on real pilot data. A prepared source, running task, or model answer is not proof of successful native compilation.

## Launch and client example

```sh
# Close older desktop builds before starting an API-enabled backend.
/path/to/llm-wiki-desktop --serve-api --headless --api-port 34343 \
  --project-root /absolute/path/to/wiki \
  --import-root /absolute/path/to/prepared-sources
```

The default token is stored under OS credential service `com.llmwiki.desktop.local-api`, account `default`. An automation client captures `/path/to/llm-wiki-desktop --api-token` locally into memory (this explicitly prints the token to its caller; never run it into logs); never put it in a URL, project artifact, or log. An explicit 64-hex-character `LLM_WIKI_API_TOKEN` override supports disposable/test hosts; the host removes it from its environment before starting workers so child agents do not inherit it.

Use `Authorization: Bearer <local token>` and an `Idempotency-Key` for POST. Open `{"path":"/absolute/path/to/wiki"}`, then use the returned projectId in subsequent resources. To update selected sources, POST `workflows/update-wiki` with `{"requestId":"f40f24c9-9ac0-4d88-b916-121b1d95f2c0","mode":"changed_sources","selection":{"kind":"selected","sourceVersions":[{"sourceId":"...","versionId":"..."}]},"routeSelection":{"kind":"agent","agent":"codex"}}`. Preserve the returned task identity, read its pending decision/diff, and confirm the exact actionId only after reviewing the generated change.

Body arrival has a 15-second timeout and 2 MiB limit. Selected local inputs are checked before API parsing and again by the actual worker, including inputs added through GUI/history. Sync filesystem/Git commands use the existing blocking-work coordinator; model processing remains a native background task. An in-flight write is owned by the backend and registered before execution, so a disconnected HTTP waiter cannot cause the same key to write twice.

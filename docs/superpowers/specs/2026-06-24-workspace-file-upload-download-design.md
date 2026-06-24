# Workspace File Upload / Download — Design

- **Date:** 2026-06-24
- **Branch:** `mr/c515-research-file-upload-download`
- **Status:** Proposed (awaiting spec review)

## 1. Goal

Let a user **upload files into** and **download files from** any workspace's
worktree directory directly from the web UI, on **local** deployments. The
feature is surfaced as a collapsible **"Files"** section in the workspace right
sidebar.

## 2. Background — what already exists

The codebase already provides ~80% of the primitives:

- **Worktree on disk:** each workspace's directory is stored as
  `Workspace.container_ref` (absolute path, e.g.
  `/var/tmp/vibe-kanban/worktrees/vk-<uuid>/`), containing one subdir per repo.
  `ContainerService::ensure_container_exists(workspace)` returns it.
- **Streaming file download** is an established pattern:
  `crates/server/src/routes/workspaces/attachments.rs::serve_file` streams via
  `Body::from_stream(ReaderStream::new(file))` and guards traversal (reject
  `..`, `canonicalize` + `starts_with(base)`).
- **Upload-into-worktree** exists for attachments (multipart → copied into the
  worktree's `.vibe-attachments/`), with a per-route `DefaultBodyLimit`.
- **Directory listing** exists: `GET /api/filesystem/directory?path=`.
- **Relay signing middleware** (`require_relay_request_signature`) buffers
  request bodies to 50 MB **only for relay requests** — it short-circuits
  (`if !is_relay_request(&request) { return next.run(...) }`) for local,
  non-relay requests. Local in-browser uploads use `makeLocalApiRequest`, so
  they never hit the buffer.
- **Frontend:** central API client `packages/web-core/src/shared/lib/api.ts`;
  `react-dropzone` already wired for image drops; right sidebar sections defined
  in `packages/web-core/src/pages/workspaces/RightSidebar.tsx`.

**Gap:** there is no general endpoint to browse the worktree's working tree or
to read/write arbitrary files in it. The attachment flow is special-cased to
`.vibe-attachments/`; the directory lister cannot read or write file contents.

## 3. Resolved decisions

| Decision | Choice | Rationale |
| --- | --- | --- |
| Interface | In-browser UI only | Lowest friction; lands where the workspace UI already is. |
| Deployment | Local only (for now) | Endpoints route through the `Deployment` abstraction so remote can be added later, but remote is **not** implemented or tested in v1. |
| File size | Stream both directions; raise/disable upload body limit | Confirmed cheap: local requests bypass the 50 MB relay buffer, and streaming download is already the codebase pattern. |
| Browse scope | Workspace root (`container_ref`) | Reach **any repo** in the workspace from one tree. |
| Zip download | Included in v1 | "Grab the results" in one click. |
| UI placement | New collapsible "Files" section in `RightSidebar`, **collapsed by default** | Consistent with Git / Notes / Terminal sections. |
| Sidebar order | **Git → Notes → Terminal → Files** | Per request: move Terminal below Notes, append Files. |

## 4. Non-goals (v1, YAGNI)

Delete / rename / mkdir / move; inline file editing; multi-select bulk
operations; resumable / chunked (tus-style) uploads; **remote/cloud** worktree
file access; a dedicated mobile tab (sidebar section only); any new auth/RBAC
beyond the existing workspace-scoped routes.

## 5. Backend

### 5.1 Module & mounting

New module `crates/server/src/routes/workspaces/files.rs`, nested at
`/api/workspaces/{id}/files`, behind the existing `load_workspace_middleware`
(so `Workspace` is in request extensions and `{id}` is validated). Paths are
passed as a **`?path=` query param** (not a wildcard segment) to keep middleware
simple and mirror `filesystem.rs`.

### 5.2 Base directory

`base = ensure_container_exists(workspace)` → the workspace root
(`container_ref`). All `?path=` values are interpreted relative to this root.
Empty/absent `path` = the root itself.

### 5.3 Path-safety helper

Add `resolve_safe_path(base: &Path, rel: &str, require: PathExistence)` to
`crates/utils/src/path.rs` (shared + unit-testable):

1. Reject if any component of `rel` is `..` (or `rel` is absolute).
2. Join `base.join(rel)`.
3. `canonicalize` the target (for read) or its **parent** (for upload, since the
   file doesn't exist yet) and assert it `starts_with(canonicalize(base))`.

`canonicalize` resolves symlinks, so a symlink inside the worktree pointing
outside the base is rejected by the `starts_with` check. Optionally refactor the
inline guard in `attachments.rs::serve_file` to call this helper (low priority).

### 5.4 Endpoints

| Method & path | Behavior |
| --- | --- |
| `GET /list?path=<rel>` | `read_dir` the directory; return `WorkspaceDirListing`. Dirs and files with name, relative path, `is_dir`, size, modified time. |
| `GET /download?path=<rel>` | Stream the file (`Body::from_stream(ReaderStream::new(..))`), `Content-Type` via `MimeGuess` (octet-stream fallback), `Content-Disposition: attachment; filename=...`, `Content-Length`. |
| `GET /download-zip?path=<rel>` | Stream a zip of the subtree at `path` (default = root). Prefer a streaming zip writer (`async_zip`); a build-to-tempfile-then-stream fallback is acceptable for v1. Skip symlinks that escape base. |
| `POST /upload?path=<rel>` (multipart) | For each multipart field: take the original filename, reduce to **basename** (strip dir components, reject `..`), stream the field body to a temp file in the target dir, then atomically rename into place. Target dir must already exist and pass `resolve_safe_path`. Layer `DefaultBodyLimit::disable()` (or a high cap, e.g. 2 GiB) on this route only. |

### 5.5 Errors

Reuse `ApiError` / `FileError`: `404` for not-found / outside-base, `400` for
malformed path, `413` only reachable on the relay path (out of scope). Log and
skip individual unreadable entries during zip streaming rather than failing the
whole archive.

## 6. Shared types

Add to `crates/api-types` (with `#[derive(..., TS)]`) and register in
`crates/server/src/bin/generate_types.rs`, then `pnpm run generate-types`:

```rust
pub struct WorkspaceFileEntry {
    pub name: String,
    pub path: String,          // relative to workspace root
    pub is_dir: bool,
    pub size_bytes: i64,
    pub modified: Option<DateTime<Utc>>,
}
pub struct WorkspaceDirListing {
    pub path: String,          // the listed dir, relative to root
    pub entries: Vec<WorkspaceFileEntry>,
}
```

## 7. Frontend

### 7.1 API client (`packages/web-core/src/shared/lib/api.ts`)

New `filesApi` using `makeLocalApiRequest`:

- `list(workspaceId, path)` → `GET …/files/list?path=` → `WorkspaceDirListing`.
- `downloadUrl(workspaceId, path)` / `downloadZipUrl(workspaceId, path)` →
  return the URL string; downloads are plain `GET`s triggered by an `<a download>`
  / `window.open` (no signing needed locally).
- `uploadFiles(workspaceId, path, files)` → `FormData` `POST …/files/upload?path=`.

### 7.2 Component

`WorkspaceFilesPanel.tsx` (+ a thin container following the existing
`*Container` pattern, fetching via React Query):

- Breadcrumb path navigation (click a crumb to go up; click a dir row to descend).
- Directory listing: dirs first, then files, showing size + modified.
- Per-file **download** link/button.
- **Drag-drop upload zone** (`react-dropzone`) targeting the current dir, with a
  progress indicator; refresh the listing on success.
- **"Download folder as zip"** button for the current dir.

### 7.3 Sidebar integration (`RightSidebar.tsx`)

- Reorder the base `sections` array to **`[Git, Notes, Terminal]`** (move the
  Terminal object below Notes; its `visible`/action logic is unchanged).
- Append a **Files** section: `visible: true`, `content: <WorkspaceFilesPanel
  workspaceId={selectedWorkspace.id} />`.
- Add `const [filesExpanded] = usePersistedExpanded(PERSIST_KEYS.filesSection,
  false)` → **collapsed by default**; thread it into the section + `useMemo` deps.
- Add `filesSection` to `PERSIST_KEYS` in
  `@/shared/stores/useUiPreferencesStore`.

(Active Changes/Logs/Preview tabs continue to `unshift` on top — unaffected.)

## 8. Data flow

1. Section expands → `filesApi.list(id, "")` → render root entries.
2. Click dir → `list(id, dirPath)` → re-render breadcrumb + entries.
3. Click file → browser navigates to `downloadUrl` → server streams it.
4. Drop files → `uploadFiles(id, currentPath, files)` → server streams to disk →
   client refreshes the listing.
5. "Download as zip" → browser navigates to `downloadZipUrl(id, currentPath)`.

## 9. Security

- **Path traversal:** every endpoint runs `resolve_safe_path` against the
  canonicalized workspace root.
- **Symlink escape:** caught by `canonicalize` + `starts_with`.
- **Upload names:** reduced to basename; no caller-controlled directories.
- **Body limit:** raised only on the local upload route; the relay path keeps
  its 50 MB cap (remote is out of scope).
- **No new auth surface:** same workspace-scoped path + middleware as existing
  endpoints.

## 10. Testing

- **Rust unit tests** for `resolve_safe_path`: rejects `..` and absolute paths,
  rejects symlink-escape, accepts valid nested paths, handles the upload
  "parent must exist" case.
- **Handler round-trip** tests (upload → list → download) if the server test
  harness supports it.
- **Frontend:** Vitest for any pure breadcrumb/path helper; manual QA for
  drag-drop upload, single-file download, and zip download.

## 11. Sequencing

1. `api-types` structs + `generate_types` registration + `pnpm run generate-types`.
2. `utils::path::resolve_safe_path` + unit tests.
3. `files.rs` endpoints + nest under `workspaces/mod.rs`.
4. `filesApi` in `api.ts`.
5. `WorkspaceFilesPanel` + container.
6. `RightSidebar` reorder + Files section + `PERSIST_KEYS.filesSection`.
7. `pnpm run format` / `pnpm run check` / `pnpm run lint`; manual QA.

**Effort:** Medium — one backend module and one frontend panel, both reusing
established patterns.

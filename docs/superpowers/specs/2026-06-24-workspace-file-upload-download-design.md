# Workspace File Upload / Download — Design (v2, council-hardened)

- **Date:** 2026-06-24
- **Branch:** `mr/c515-research-file-upload-download`
- **Status:** Approved for implementation (council round applied)

## 1. Goal

Let a user **browse + download** files from any workspace's worktree, and
**upload** files into it, from the web UI on **local** deployments. Surfaced as a
collapsible "Files" section in the workspace right sidebar.

## 2. Background — what exists

- `Workspace.container_ref` = absolute worktree root; `ensure_container_exists(ws)` returns it.
- Streaming download + traversal guard pattern: `crates/server/src/routes/workspaces/attachments.rs::serve_file` (canonicalize + `starts_with`), and the hardened header helper `content_type_and_disposition_for_attachment` in `crates/server/src/routes/attachments.rs` (static `attachment`, octet-stream + `nosniff`).
- `utils::path::ALWAYS_SKIP_DIRS` already denylists `.git`, `node_modules`, etc.
- Relay signing middleware buffers bodies to 50 MB **only for relay requests** (`is_relay_request` short-circuit); the **host-relay proxy** (`routes/host_relay/proxy.rs`) buffers at `usize::MAX`. Local *direct* requests bypass both.
- Frontend: central `api.ts` (`makeLocalApiRequest`), `react-dropzone`, sidebar sections in `RightSidebar.tsx`. `localApiTransport.ts` rewrites `/api/...` → `/api/host/{host_id}/...` when a remote host is selected.

## 3. Resolved decisions (incl. council round)

| Decision | Choice |
| --- | --- |
| Interface | In-browser UI only. |
| Deployment | **Local only, ENFORCED** — backend rejects relay/host-proxied requests on these routes; frontend hides the panel when a remote host is selected. |
| Browse / download scope | Full worktree root (read-only), hiding hidden + `ALWAYS_SKIP_DIRS` by default; `.git` always denied. |
| **Upload destination** | **Drop folder + opt-in path.** Default target = git-ignored `.vibe-uploads/` (auto-created). User may opt to upload into the directory they're browsing. Both paths: deny `.git`/`ALWAYS_SKIP_DIRS`, no-overwrite (409) unless `overwrite=true`, hard caps. |
| Upload size | Concrete caps (NOT `disable()`): per-file + per-request byte caps, file-count cap, filename-length cap, streaming byte-count abort, temp cleanup. |
| Zip download | Kept, but **bounded**: `follow_links(false)`, skip `.git`/`ALWAYS_SKIP_DIRS`/hidden, max entries + max uncompressed bytes + wall-time, sanitized relative names, **fail-loud** (no silent partial archive). |
| Policy | One `WorkspaceFilePolicy` helper centralizes path-safety + denylist + caps; unit-tested. |

## 4. Non-goals (v1)

Delete/rename/mkdir/move; inline editing; resumable/chunked uploads; **remote/cloud** worktree access; a new app-wide CSRF-token/secret scheme (these routes match the **existing** local origin posture of comparable mutating endpoints — attachment upload, git ops — plus relay rejection; an app-wide auth upgrade is out of scope); mobile tab.

## 5. Backend

### 5.1 Module & mounting

New module `crates/server/src/routes/workspaces/files.rs`, nested at
`/api/workspaces/{id}/files`, behind `load_workspace_middleware` **and** a
`reject_relay_requests` guard (403 if the `x-vibe-relay` header is present) so the
surface is local-direct only. Paths passed as `?path=` query params.

### 5.2 Policy helper (`crates/server/src/routes/workspaces/file_policy.rs`)

- `resolve_safe_path(base, rel) -> Result<PathBuf, FilePolicyError>` — reject `..`/absolute components; reject any component in `ALWAYS_SKIP_DIRS` or equal to `.git`; join; `canonicalize`; assert `starts_with(canonicalize(base))`. **Any `canonicalize` error = hard reject** (never fall back to the raw join — guards against the existing `path.rs` raw-fallback antipattern).
- `safe_basename(name) -> Result<String, _>` — strip directory components, reject empty / `..` / path separators / over-long (>255).
- Listing uses `symlink_metadata` (no-follow). Upload-target resolution validates the parent dir exists, is a real directory (not a symlink), and passes `resolve_safe_path`.

### 5.3 Endpoints

| Method & path | Behavior |
| --- | --- |
| `GET /list?path=<rel>` | `read_dir`; `symlink_metadata` per entry; skip hidden + `ALWAYS_SKIP_DIRS`; entries `{name, path, is_dir, is_symlink, size_bytes, modified}`; cap at `MAX_LIST_ENTRIES` with a `truncated` flag. |
| `GET /download?path=<rel>` | `resolve_safe_path` (must be a file); stream `Body::from_stream(ReaderStream::new(..))`; headers via the existing `content_type_and_disposition_for_attachment` (static `attachment`, octet-stream + `nosniff`) — **no attacker-controlled `filename`**. |
| `GET /download-zip?path=<rel>` | `walkdir(follow_links=false)` under the resolved subtree; skip `.git`/`ALWAYS_SKIP_DIRS`/hidden/symlinks; enforce `MAX_ZIP_ENTRIES`, `MAX_ZIP_UNCOMPRESSED_BYTES`, `ZIP_WALL_TIME`; sanitized relative entry names; **exceeding a cap → 4xx/5xx error, not a partial zip.** Build via sync `zip` crate in `spawn_blocking` → `NamedTempFile` → stream → drop. |
| `POST /upload?path=<rel>&overwrite=<bool>` (multipart) | Target dir = `.vibe-uploads/` when `path` absent/empty (auto-created + gitignored), else the validated browsed dir. Per file: `safe_basename`; stream to a temp file (`create_new`) in the **canonical target dir** with a byte counter (abort at `MAX_UPLOAD_FILE_BYTES` → 413 + cleanup); if final exists and not `overwrite` → 409 + cleanup; revalidate target under base; atomic rename. Enforce `MAX_UPLOAD_FILES` + filename length. Route layers `DefaultBodyLimit::max(MAX_UPLOAD_REQUEST_BYTES)`. |

### 5.4 Constants (tunable)

`MAX_LIST_ENTRIES = 2000`; `MAX_UPLOAD_FILE_BYTES = 2 GiB`;
`MAX_UPLOAD_REQUEST_BYTES = 5 GiB`; `MAX_UPLOAD_FILES = 50`; filename ≤ 255;
`MAX_ZIP_ENTRIES = 10_000`; `MAX_ZIP_UNCOMPRESSED_BYTES = 2 GiB`; `ZIP_WALL_TIME = 120s`.

### 5.5 Errors

Reuse `ApiError`/`FileError`: `404` not-found/outside-base, `400` malformed path,
`409` overwrite conflict, `413` too large, `403` relay-rejected. No silent partials.

## 6. Shared types

Defined in `crates/server/src/routes/workspaces/files.rs`
(`#[derive(Debug, Clone, Serialize, Deserialize, TS)]`, mirroring the existing
`AttachmentResponse` pattern) and registered in
`crates/server/src/bin/generate_types.rs` (ts-rs maps `i64` → `bigint`):

```rust
pub struct WorkspaceFileEntry { name: String, path: String, is_dir: bool, is_symlink: bool, size_bytes: i64, modified: Option<DateTime<Utc>> }
pub struct WorkspaceDirListing { path: String, entries: Vec<WorkspaceFileEntry>, truncated: bool }
```

## 7. Frontend

### 7.1 API client (`packages/web-core/src/shared/lib/api.ts`)

`workspaceFilesApi` via `makeLocalApiRequest`: `list(workspaceId, path)`;
`downloadUrl/downloadZipUrl(workspaceId, path)` (plain GET, `<a download>`);
`upload(workspaceId, files, { path?, overwrite? })` (FormData).

### 7.2 Component

`WorkspaceFilesPanel.tsx` (+ container): breadcrumb + listing (dirs first, sizes,
modified, symlink/`truncated` indicators), per-file download, **drag-drop upload
with a target toggle** ("Drop folder `.vibe-uploads/`" default vs "This folder"),
overwrite confirmation on 409, progress, and "Download folder as zip".

### 7.3 Visibility & sidebar

- Gate: render only when `selectedWorkspace` is set **and** no remote host is
  selected (`useHostId()` null) — the panel is local-only.
- `RightSidebar.tsx`: reorder base sections to **Git → Notes → Terminal → Files**;
  Files **collapsed by default** via `usePersistedExpanded(PERSIST_KEYS.filesSection, false)`.
- Add `filesSection` to `PERSIST_KEYS` + `PersistKey` in `useUiPreferencesStore`.

## 8. Security (council-driven)

Local-only enforced (relay reject + host-gated UI); `WorkspaceFilePolicy` on every
endpoint (canonicalize + `starts_with`, `.git`/`ALWAYS_SKIP_DIRS` denied, canonicalize-error
= hard reject); symlink-safe (`symlink_metadata`/no-follow, `create_new` temp in canonical
parent, revalidate before rename — TOCTOU mitigation); no-overwrite-by-default; hardened
download headers (no attacker filename); bounded uploads + zip (DoS); no silent partial archives.

## 9. Testing (test it hard)

- **Rust unit** (`WorkspaceFilePolicy`): rejects `..`/absolute/`.git`/`node_modules`/symlink-escape/canonicalize-error; accepts valid nested; `safe_basename` strips dirs + rejects `..`/separators/over-long.
- **Rust handler**: upload→list→download round-trip (default drop folder + opt-in path); upload to `.git/hooks/post-checkout` → 4xx; overwrite without flag → 409; over-cap upload → 413; relay header → 403; zip excludes `.git`/`node_modules` and over-cap → error.
- **Backend**: `cargo test` touched crates; `pnpm run backend:check`; `pnpm run generate-types:check`.
- **Frontend**: `pnpm run check` + `pnpm run lint`; Vitest for any pure helper.
- **Manual/browser**: expand Files, upload (small + large) to drop folder + opt-in dir, download back byte-identical, zip a subtree + verify no `.git`, attempt `../` + `.git` writes → rejected, confirm panel hidden when a remote host is selected.

## 10. Sequencing

1. `WorkspaceFilePolicy` + unit tests. 2. `api-types` + generate-types. 3. `files.rs` endpoints + relay guard + nest. 4. backend handler tests. 5. fmt/check/test. 6. frontend api + panel + sidebar + gating + store. 7. frontend check/lint. 8. draft PR + review rounds + babysit.

---

## Council review — applied (2026-06-24)

`/council:council` (mixed: 4 Codex personas + 1 Claude penetration-tester seat) reviewed v1 → **BLOCK 5/5** ("directionally fine, too broad as written"). Applied P0/P1:

- **P0 local-only not enforced** → backend `reject_relay_requests` guard + frontend host-gating.
- **P0 unbounded upload (`disable()`)** → concrete per-file/per-request/file-count/filename caps + streaming abort + temp cleanup.
- **P0/P1 arbitrary worktree write** → default to git-ignored `.vibe-uploads/` drop folder (user opt-in for browsed dir); `.git`/`ALWAYS_SKIP_DIRS` denied; no-overwrite → 409.
- **P1 canonicalize+starts_with TOCTOU/symlink** → `symlink_metadata`/no-follow, `create_new` temp in canonical parent + revalidate before rename, canonicalize-error = hard reject.
- **P1 zip-slip/zip-bomb/partial** → `follow_links(false)`, skip denylist, caps, sanitized names, fail-loud.
- **P1 Content-Disposition injection/XSS** → reuse existing hardened `content_type_and_disposition_for_attachment`.
- **P1 listing cost/exposure** → skip hidden + `ALWAYS_SKIP_DIRS`, entry cap + `truncated`.
- **P1 frontend visibility** → gate on `selectedWorkspace` + local capability.
- **Maintainability** → single `WorkspaceFilePolicy` helper + tests.
- **Deferred (noted non-goals):** app-wide CSRF-token/secret scheme (match existing local posture instead), resumable uploads, remote support.

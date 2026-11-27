# Reader Sync Server: v1 design (SYNC_SPEC)

Status: **rev 3.1** (2026-11-22; changelog in §14). **Spec gate cleared as of rev 3.1:** no open question blocks the build. This is a design for a cloud coding agent to build. Nothing is committed.
Repo: one repo, `arturo-kuang/reader` (Origin):
```
App/                    Reader for macOS                    (pdf reader)
Packages/ReaderCore/    shared Swift core, unchanged by us  (pdf reader)
Packages/SyncServer/    this server                         (us)
apps/mobile/            React Native phone app (Android first)
spec/                   shared contract: this spec, openapi.yaml, schemas/*.schema.json, fixtures/, tokens.json
```
We work on a branch and open a PR that touches only `Packages/SyncServer/**` and our files under `spec/` (§2.1). pdf reader's agent commits to `main`.
Companion files, next to this one: `spec/openapi.yaml` and `spec/schemas/{envelope,book,highlight,note,word,lookup,conflict,common}.schema.json` (`common` holds shared leaf types; validated by `tools/validate.py`). pdf reader's record↔Markdown mapping is `spec/markdown-mapping.md` (a copy of their v2, `markdown-mapping.v2.md`), referenced in §10. Word-id vectors: `spec/fixtures/word-id/normalize.json` (pdf reader's).

---

## 0. Summary

- A small Swift server runs on the Mac mini (`arturos-mac-mini`, Tailscale `100.97.87.111`) as a **LaunchAgent**. The mini auto-logs in. The server listens only on the Tailscale IP and on 127.0.0.1, over **plain HTTP** (the tailnet's WireGuard already encrypts traffic; Android needs a cleartext exception for that one host).
- Records: `book`, `highlight`, `note`, `word`, `lookup`, `position` (each device's last reading position in a book). Each has an envelope (`bookId`, `kind`, `id`, `version`, `baseVersion`, `deleted`, …) and a `payload` the server stores as opaque JSON. Every payload carries `extras`, which is passed through untouched.
- Push is batched and idempotent (client `batchId` and `changeId`). Pull is one feed ordered by a single monotonic server `seq`, so a device that was offline for days pulls only what changed.
- Conflicts follow a base-version rule. **Content that is identical on both sides is never a conflict.** When both sides changed a record, the server keeps all sides, and the conflict shows up in the pull feed. Any client can resolve it.
- Books have a permanent `bookId`: the first-synced fingerprint, `sha256:<hex>`. A book also has a `docId` (pdfID or epubID) and a list of file hashes. Registering a file whose `docId` matches an existing book attaches it to that book. Files are content-addressed, uploads are resumable and hash-checked, and downloads support `Range`. The phone sees the full book list and downloads a book when it is opened.
- The server never writes Markdown and never touches the vault. Each Reader on a Mac projects records into its own files (`spec/markdown-mapping.md`). Conflicts live on the server: Reader **stops writing** `Also changed outside Reader` callouts.

---

## 1. Reader's data model today (ReaderCore, read at 9fc048f; `main` is now 2163c4d, a UI-only commit)

### 1.1 Book identity
- `SHA256.fingerprint(data)` = `"sha256:" + lowercase hex` of the whole file (pure-Swift, `SHA256.swift`). The Application Support sidecar folder uses the bare hex.
- `pdfID` comes from the last trailer `/ID [<hex>…]`: the first entry, hex digits only, lowercased (`PDFIdentity.scan`). `epubID` is the first `dc:identifier` in the OPF.
- Matching (`matchIdentity`): an equal fingerprint is `.exact`. An equal non-empty pdfID or epubID is `.stableID`, and Reader rewrites the stored fingerprint. Anything else is `.different`, which means re-anchor by quote, prefix and suffix.
- **The SHA-256 of a book is not stable for its lifetime** (for example, a PDF edited and saved in Preview). Vault books currently record `pdfID` and `epubID` as nil (mapping §4.3).

### 1.2 Storage
- **Sidecar mode:** `<stem>.reader/` holds `notes.md`, `highlights.md`, `words.md` and `marks.md` (YAML front matter plus one `<!-- reader {sorted compact JSON} -->` per entry; unknown keys become `extras`), and `state.json` (`ViewState`, which includes bookmarks).
- **Vault mode:** highlights and notes are owned blocks `<!-- reader:begin id=… -->…<!-- reader:end -->` in chapter notes files. Vocabulary goes to `learner/vocabulary.md` or `words.md`, and settings to `.reader-state.json`. This projection is lossy: no prefix, suffix, rects, extras, timestamps, scope, or separate note id.

### 1.3 Types (quoted, trimmed)
```swift
struct HighlightEntry { id /*h_+32hex*/; color: Int /*1…6*/; page: Int? /*1-based*/; cfi: String?
  quote, prefix, suffix: String; section: String?; rects: [[Double]] /*[minX,minY,maxX,maxY]*/
  noteID: String? /*md key "note"*/; extras: [String: JSONValue] }
struct NoteEntry { id /*n_…*/; page; cfi; quote, prefix, suffix; section; created, updated: String? /*ISO-8601*/
  ai: Bool; provider: String?; scope: String /*"passage"*/; body: String; extras }
struct WordEntry { id /*"w_"+WordStore.normalize(word)*/; word, definition; explanation, explanationProvider,
  pronunciation, partOfSpeech: String?; learned: Bool; lookups: [LookupEntry]; extras }
struct LookupEntry { id /*l_…, md key "lookup"*/; page; cfi; section; quote, prefix, suffix; sentence; date: String; extras }
// WordStore.normalize(w) = w.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()   (no Unicode normalization)
// Reanchorer.locate(quote:prefix:suffix:in:) -> ReanchorHit{…, confidence: .exactContext|.quoteOnly}; context capped at 80 chars
```
- Highlights and words have no timestamps, so sync never relies on clock time. Deletes are hard removes today.
- `VaultSync.applyExternal` already does a 3-way merge per block id: if one side changed, that side wins; if both changed, both are kept. The server applies that rule per record.

---

## 2. Decisions

### 2.1 Layout of our code
```
Packages/SyncServer/
  Package.swift            // tools 6.0, macOS 14; deps: ../ReaderCore (path), hummingbird 2.x, GRDB 7.x
  Sources/SyncWire/        // Codable wire types GENERATED from spec/schemas + hand-written MergeRule (Foundation only)
  Sources/SyncStore/       // GRDB schema, RecordStore, BookRegistry, BlobStore
  Sources/SyncServer/      // executable `reader-sync`: routes, auth, pairing, config, admin CLI
  Tests/…  Harness/        // fake-client harness (§12.3)
  launchd/com.arturokuang.reader-sync.plist
spec/                      // ours: SYNC_SPEC.md, openapi.yaml, schemas/*.schema.json, fixtures/sync/**
                           // pdf reader's: markdown-mapping.md, fixtures/{fingerprint,reanchor,markdown,blocks,word-id}/**
                           // Designer's: tokens.json
```
- There is no new shared Swift package. `SyncWire` is a target inside SyncServer, and Reader can depend on that product later. SyncServer imports ReaderCore unchanged. At runtime it uses only `JSONValue`; the tests also use `SHA256`, `WordStore.normalize` and the Markdown parsers.
- **Codegen:** `spec/schemas/*.schema.json` is the source of truth. A script, `spec/gen.sh`, runs quicktype and generates `Packages/SyncServer/Sources/SyncWire/Generated.swift` and `apps/mobile/src/sync/generated.ts`. Generated files are committed. CI runs `gen.sh && git diff --exit-code`. The server does **not** decode payloads with the generated types, because it treats them as opaque. Clients use them.
- Forward compatibility: the payload schemas use `additionalProperties: false`, so every unknown field has to travel inside `extras`. Generated types keep `extras` as a free-form JSON object, so round trips are lossless. A new top-level field needs a `protocol` bump (§5.1).

### 2.2 Framework and storage
- **Hummingbird 2** (SwiftNIO, async, streamed bodies for blob PUT, binds to multiple addresses). Vapor is more than this needs, and raw NIO means hand-writing HTTP.
- **SQLite via GRDB 7**, in WAL mode with `synchronous=FULL` and a single writer queue. Each push batch runs in one transaction.
- **Blobs** go in `~/Library/Application Support/ReaderSync/blobs/sha256/ab/cd/<hex>` (read-only once written), with uploads in progress at `uploads/<hex>.part`. The database is `sync.sqlite`, config is `config.json`, and logs go to `~/Library/Logs/ReaderSync/`. Whether a blob exists is decided by the filesystem.

---

## 3. Records

### 3.1 Envelope (`spec/schemas/envelope.schema.json`)
```json
{ "bookId": "sha256:3f…", "kind": "highlight", "id": "h_9c1e…",
  "version": 4, "baseVersion": 3, "deleted": false,
  "deviceId": "dev_…", "changeId": "c_…", "clientTime": "2026-11-22T08:31:00Z",
  "seq": 1042, "payload": { …, "extras": {} }, "conflict": null }
```
- **Key:** `(bookId, kind, id)`, compared as exact UTF-8 bytes. The server never normalizes ids. Because `kind` is part of the key, a vault note can reuse its highlight's id (mapping §2, "noteID").
- `id` is chosen by the client. Allowed: 1–512 UTF-8 bytes, no control characters. Word ids can contain spaces and any Unicode (`w_ice cream`, `w_café`). The other kinds use `IDs.make` (`h_`/`n_`/`l_` + 32 hex). The server does not enforce a prefix for each kind.
- `version` is per record and assigned by the server. `baseVersion` is the version the edit was based on, with 0 meaning create. `deviceId` is stamped by the server. `clientTime` is informational only. `seq` and `conflict` appear only in the feed.
- `payload` is a JSON object of at most 1 MiB, stored and returned as the **exact bytes received**. For a tombstone, it is the last live payload or `null`. The only payload fields the server reads are those of `kind=book`, which it writes itself (§3.3), and `bookId`/`deviceId` of `kind=position` (owner check, §3.4).
- **Hash fields:** `bookId` and Reader's `fingerprint` use the prefixed form `sha256:<hex>`. Every field named `sha256`, and blob URL paths, use the bare 64-char lowercase hex.

### 3.2 Payloads: field names follow `spec/markdown-mapping.md` §2 ("Wire (suggested)")
Anchor fields are **flat** in the payload, exactly as in Reader's entry types and the mapping doc.
- The **anchor of record is `quote` + `prefix` + `suffix`**. `page` (1-based), `cfi` and `rects` are hints only.
- Clients re-anchor by text when a hint fails: `Reanchorer` on the Mac, the same algorithm on the phone. A client must not push a re-anchor result unless the user edits the record.
- `rects`: one `[minX, minY, maxX, maxY]` per line, in **unrotated PDF page space with the origin at the bottom left** (PDF user space, the same in PDFKit and PDF.js). `[]` or absent for EPUB.

| kind | payload fields (all also have `extras`) | Reader type |
|---|---|---|
| `highlight` | `color` 1–6, `page?`, `cfi?`, `quote`, `prefix`, `suffix`, `section?`, `rects?`, `noteId?` | `HighlightEntry` (`noteID`, md key `note`) |
| `note` | `page?`, `cfi?`, `quote`, `prefix`, `suffix`, `section?`, `created?`, `updated?`, `ai`, `provider?`, `scope`, `body`, `highlightId?` | `NoteEntry` |
| `word` | `word`, `definition`, `explanation?`, `explanationProvider?`, `pronunciation?`, `partOfSpeech?`, `learned`. **No `lookups` on the wire.** | `WordEntry` without `lookups` |
| `lookup` | `wordId`, `page?`, `cfi?`, `section?`, `quote`, `prefix`, `suffix`, `sentence`, `date` | `LookupEntry` (md key `lookup` = `id`) |
| `position` | `bookId`, `deviceId`, `locator`: PDF `{format:"pdf", page, point?:{x,y}}` (1-based page as elsewhere; point in unrotated page space, bottom-left origin) or EPUB `{format:"epub", cfi, progression?: 0…1}` | `ViewState.page`/`scrollY` (PDF), CFI (EPUB); see §3.4 |
| `book` | server-written: `title`, `filename`, `format` (`pdf`\|`epub`), `docId?`, `files: [{sha256, size, addedAt}]`, `currentSha256` | `FrontMatter` + `LibraryEntry`, without paths |

- **Highlight and note** are two records. The link of record is `highlight.noteId`, which is Reader's `noteID`. `note.highlightId` is an optional back-reference that writers set too. If the two disagree, `highlight.noteId` wins. Deleting a highlight that has a note means two changes in one batch.
- **Word and lookup:** the word id is `"w_" + WordStore.normalize(word)`, so every device derives the same id for the same word, within one book. Every lookup is its own record (`l_…`, with `bookId` and `wordId`). Reader rebuilds `WordEntry.lookups` by grouping lookups by `wordId` and sorting them by `date`, then `id`.
  - When two devices look up the same new word, both create the same word record with `baseVersion` 0. If the payloads are equal, the second create is a `noop` (§6). Each device's lookups are separate creates that never collide.
  - A lookup whose `wordId` has no live word is kept, and clients hide it until the word arrives. Deleting a word should come with deletes for its lookups. The server does not cascade deletes.
- **Links** (`reader://`): a link resolves by fingerprint first, meaning `bookId` or any `files[].sha256`, and falls back to the path. Note that this is client behavior only, and the server is not involved. Today's link has no fingerprint parameter (mapping §5), so this is a request to pdf reader (§11).
- Not synced in v1 (Q2, settled): marks, panes and window or sidebar state, reading settings (zoom, fit, theme, font), Ask AI history; also the rest of `ViewState`, bookmarks, AI settings, curriculum, tutor and mastery files, and hand-written vocabulary (`hand-…` ids).

### 3.3 Books and the register rule (`POST /v1/books`)
Book records are **written only by the server**. Clients never push `kind=book`; the server rejects it with `use_register`. A client registers a file:
```json
POST /v1/books  { "sha256": "<hex>", "size": 123, "format": "pdf", "docId": "4b1e77c0", "title": "…", "filename": "Book.pdf" }
```
The server applies these rules in order, inside one transaction, so the same request always gives the same answer:
1. The `sha256` is already in some book's `files`: return that book, `status: "existing"`.
2. `docId` is not empty and equals the `docId` of a live book with the same `format`: append `{sha256,size,addedAt}` to that book's `files`, set `currentSha256`, write a new version with a new seq, and return `status: "attached"`. This is the server-side version of Reader's `.stableID`.
3. Otherwise: create a book with `bookId = "sha256:" + sha256`, `version 1`, and return `status: "created"`.

The response is `{status, bookId, book: <envelope>}`. Clients map their local book to the returned `bookId`. If a Mac had already used a different bookId locally, before syncing, it re-keys its local records to the returned one before pushing.
- `docId` is `pdfID` for PDFs and `epubID` for EPUBs. Empty or missing values are ignored. Different formats never match each other.
- Registering does not require the blob to be uploaded yet. `GET /v1/books` shows `hasBlob` for each file.
- Books are never deleted in v1. Hiding a book on one device is a local decision.

### 3.4 Reading position (`kind: position`)
- One record per (book, device): `id = "p_" + bookId + "_" + deviceId`, for example `p_sha256:3f…_dev_7a…`. Only the device named in the id writes it.
- The server's only checks for this kind: the `id` must equal `"p_" + bookId + "_" + <authenticated deviceId>`, and `payload.bookId`/`payload.deviceId` must match. Otherwise the change is `rejected not_owner`. `deleted: true` is rejected (`bad_request`); positions are never deleted in v1.
- **The base-version check is skipped:** every push of a position that differs from the head is a fast-forward to head+1 with a new seq, whatever `baseVersion` says. An identical push is a `noop`. So a position never opens a conflict (§6).
- **Newest wins = highest server `seq`, never the device clock.** A client opening a book reads all `position` records with that `bookId` and opens at the one with the highest `seq` (its own included). Clients push their position debounced, for example 5 s after scrolling stops and when the book closes.
- **EPUB accuracy in v1 (settled by Reader PM):** landing at the start of the chapter is acceptable for v1. The phone pushes its full CFI and `progression`. Reader on the Mac tracks only the chapter for now, so it pushes a chapter-start CFI with no `progression`, and on apply it uses only the chapter (spine index) of a CFI. An in-chapter position in Reader is a later item (mapping v2, "Position"). `progression` stays optional in the schema.
- **"Where you left off" toast:** for EPUB, two positions in the same chapter (same spine index) count as the same spot, so no toast is shown for them. For PDF, the page is compared.
- **Clients apply a remote position only when a book is opened**, never during a reading session: a position that arrives in the feed while the book is open is stored and used at the next open. (Client behavior; the server does not enforce it.)
- **Source device name:** the payload does not carry a device name. To show "Opened where you left off on your phone · Back to p. 40", the client looks up the position's `deviceId` (envelope `deviceId`, equal to `payload.deviceId`) in `GET /v1/devices`, which any paired device can read and which returns each device's `name` and `kind` (`mac`|`phone`) as registered at pairing (§5.2). Clients cache the list and refresh it when they see an unknown deviceId. "Back to p. 40" is the client's own previous position, kept locally.

---

## 4. Auth and pairing
- `Authorization: Bearer <credential>` (32 random bytes, base64url, stored as a SHA-256 hash). Every device has a `kind` of `mac` or `phone`. Only `mac` devices can issue pairing tokens and revoke devices. Any paired device can list devices (names and kinds only, no credentials), so it can name the source of a position.
- Bootstrap: requests from loopback (Reader on the mini) can issue tokens without auth. On the mini there is also `reader-sync pair --kind mac`, which prints a QR for other Macs over ssh.
- Pairing token: 24 random bytes in base32, valid for 10 minutes, for one device only, stored hashed. The QR encodes `readersync://pair?v=1&host=100.97.87.111&port=8788&token=<t>&server=<serverId>`.
- Transport is plain HTTP. `apps/mobile` needs `android:networkSecurityConfig` with a cleartext `domain-config` for `100.97.87.111` (and `arturos-mac-mini` if MagicDNS is used), and nothing broader.

---

## 5. HTTP API (full contract in `spec/openapi.yaml`)
Errors use `{"error":{"code","message"}}`. The codes are `unauthorized` 401, `forbidden` 403, `not_found` 404, `bad_request` 400, `change_id_reused` 422, `hash_mismatch` 422, `range_not_satisfiable` 416, `too_large` 413, `already_resolved`/`stale_base`/`upload_offset_mismatch` 409, and `server_busy` 503.

| Method | Path | Auth | Purpose |
|---|---|---|---|
| GET | `/v1/health` | none | liveness, `serverId`, `protocol`, `seq` |
| POST | `/v1/pairing/tokens` | mac or loopback | issue pairing token and QR string |
| POST | `/v1/pairing/redeem` | token in body | token → device credential |
| GET | `/v1/devices` | device | list devices: `deviceId`, `name`, `platform`, `kind` (any device; used to name a position's source) |
| DELETE | `/v1/devices/{deviceId}` | mac | revoke a device |
| POST | `/v1/changes` | device | push a batch (idempotent) |
| GET | `/v1/changes?since=&limit=` | device | pull the feed since a cursor |
| GET | `/v1/conflicts?bookId=` | device | open conflicts |
| POST | `/v1/conflicts/{conflictId}/resolve` | device | resolve (idempotent by changeId) |
| GET | `/v1/books` | device | book list |
| POST | `/v1/books` | device | register a file (create, attach or existing) |
| HEAD/PUT/GET | `/v1/blobs/{sha256}` | device | exists or partial offset, resumable verified upload, Range download |

### 5.1 Health
`{"ok":true,"serverId":"srv_…","serverVersion":"1.0.0","protocol":1,"seq":1042}`. If `serverId` changes, the server was reset, and clients re-push from base 0 (§7.3).

### 5.2 Pairing
- `POST /v1/pairing/tokens {"kind":"phone"}` returns `{token, expiresAt, qr}`.
- `POST /v1/pairing/redeem {token, deviceId (client UUID), name, platform, appVersion}` returns `{deviceId, credential, serverId, kind}`.
- Redeeming again with the same token and `deviceId` before the token expires issues a new credential and revokes the earlier one, which makes a lost response safe to retry. A different `deviceId` or an expired token gets 403. Failed redeems are limited to 10 per minute per IP.

### 5.3 Push: `POST /v1/changes`
`{batchId, changes:[{changeId, bookId, kind, id, baseVersion, deleted, clientTime, payload}]}`. Limits: 500 changes and 8 MiB per batch, at most one change per key per batch (otherwise 400), and `kind` must not be `book`. `position` changes follow §3.4 (owner check, no base check).
- The whole batch runs in one transaction, with changes applied in order.
- **Idempotency:** the server stores `batchId` and every `changeId` with a request hash and the result.
  - A replayed batch returns the stored response, byte for byte.
  - The same `batchId` with a different hash gets 422.
  - A `changeId` that was already applied inside a new batch returns its stored result.
  - A reused `changeId` with a different body is `rejected change_id_reused`.
- A change whose `bookId` is not a registered book is `rejected unknown_book`.
- Response: `{batchId, seq, results:[{changeId, status, version?, seq?, conflictId?, error?}]}`, where `status` is `applied`, `noop`, `conflict` or `rejected`.

### 5.4 Pull: `GET /v1/changes?since=<seq>&limit=<≤1000>`
- The feed contains the latest state of every record whose last seq is greater than `since`, in increasing seq order. Book records and tombstones are included, and open conflicts are embedded.
- Every accepted change, register attach, conflict open and resolve gets the next value of one global counter.
- Response: `{changes:[envelope], cursor, hasMore, serverId}`.
- Clients save the `cursor` only after the page is applied. Applying a page twice gives the same result.

### 5.5 Conflicts (`spec/schemas/conflict.schema.json`)
A conflict object looks like `{conflictId, bookId, kind, id, baseVersion, base: {deleted, payload}|null, sides: [{changeId, deviceId, version|null, deleted, payload, clientTime}], openedSeq}`. It appears in the feed as `envelope.conflict` and also through `GET /v1/conflicts`. The phone can show two versions; in v1, Reader is the client that resolves them.

`POST /v1/conflicts/{id}/resolve {changeId, baseVersion: <head>, deleted, payload}`: the server writes the new head version (head+1), closes the conflict and assigns a new seq. It is idempotent by `changeId`. If the conflict is already closed, the response is 409 `already_resolved`. If the base is not the current head, it is 409 `stale_base`.

### 5.6 Books: `GET /v1/books`
`{books:[{bookId, title, filename, format, docId?, sha256: <currentSha256>, size, hasBlob, files:[{sha256,size,hasBlob}], version, seq}]}` lists every book. The phone shows all of them and downloads `sha256` (with `Range`) the first time a book is opened. If the current file has no blob, the phone falls back to the newest file in `files` that does have one.

### 5.7 Blobs (path is bare 64-hex)
- **HEAD:** a complete blob returns 200 with `Content-Length`. A partial upload returns 404 with `Upload-Offset: <n>`. Nothing at all returns 404 with `Upload-Offset: 0`.
- **PUT:** send either the whole body with `Content-Length`, or a resume with `Content-Range: bytes start-end/total`, where `start` must equal the current `.part` size (otherwise 409 with `Upload-Offset`).
  - The server streams the body to `.part`, calling fsync every 8 MiB and at the end. Once the file reaches `total`, it computes a streaming SHA-256 (CryptoKit). On a match it renames the file into `blobs/` and fsyncs the directory, returning 201. On a mismatch it deletes `.part` and returns 422.
  - If the blob already exists, the server returns 200 without reading the body. An upload that stops early gets 202 with `Upload-Offset`.
  - There is one lock per sha: a second concurrent uploader gets 409 with the offset. The size limit is 4 GiB. Stale `.part` files are deleted after 7 days.
- **GET:** supports `Range` (206), `ETag: "<sha>"`, `If-Range` and `Accept-Ranges: bytes`. Clients verify the hash after the download. Blobs are never deleted in v1.

---

## 6. Conflict semantics
Each key has a head (`version`, `deleted`, `payload`) and at most one open conflict with N≥2 sides. Every head version is appended to `record_versions` and never deleted.

**General rule: identical content is never a conflict.** `same(x,y)` holds when `deleted` is equal and, for live records, the payloads are equal as JSON: parsed, keys sorted, numbers compared as numbers, `extras` included. Two tombstones are always `same`. Identical byte sequences short-circuit to `same`.

Let `h` be the head version (0 if the record is absent) and `b` the incoming `baseVersion`:

| Case | Result |
|---|---|
| record absent, `b=0` | **create**, version 1 |
| record absent, `b>0`; or `b>h` | `rejected bad_base` |
| `same(incoming, head)` (any `b`, including create/create with `b=0`) | `noop` (no new version or seq; returns the head version) |
| `b=h` | **fast-forward** to h+1 (only one side changed, so it wins). An open conflict stays open. |
| `b<h`, `same(incoming, some open side)` | `noop`, returns that conflict |
| `b<h`, otherwise | **conflict**: the head is unchanged. Open a conflict (base = stored version `b`, or null if `b=0`, sides = [head snapshot, incoming]) or add incoming as a new side. Assign a new seq. |

- The first accepted writer becomes the head, and every other side is kept. The server never merges silently.
- **Edit vs delete:** a conflict with a tombstone side. Reader suggests keeping the edit.
- **Delete vs delete:** noop.
- **Undelete:** an edit with `b` equal to the tombstone's version is a fast-forward.
- **Position (`kind: position`):** exempt from this table. It never conflicts: the owner check plus a skipped base check means every differing push fast-forwards, and an identical one is a noop. Across devices, "newest" means the record with the highest `seq` (§3.4), never `clientTime`.
- **Word create/create:** equal payloads are a noop. Different payloads (for example, two different definitions) are a conflict with `base: null`. Lookups never collide, because each has its own `l_` id.
- **Resolution:** writes head+1 from the resolver's payload and closes the conflict for all sides. The head is still usable while a conflict is open, and clients show a "2 versions" badge.

---

## 7. Client duties
1. **Bookkeeping per record:** `serverVersion`, `localState`, `dirty`. A push sends `baseVersion = serverVersion`. After `applied` or `noop`, the client stores the returned version and clears `dirty` if nothing changed since.
2. **Outbox:** write the changes and their ids to a durable outbox before sending. Retry with the same ids. Merge several offline edits of one record into one change per batch. Register books before pushing their records.
3. **Pull:** on foreground, after every push, and every 30 s while active. Save the cursor per page. Never overwrite a record that is dirty locally; push it and let the server decide. If `serverId` changes, clear the cursor and versions, re-register the books, and re-push everything with `b=0`. Identical records become noops, so nothing is lost.

---

## 8. Server internals

### 8.1 SQLite (GRDB migration `v1`)
```sql
meta(key PK, value)                                      -- serverId, seq, schema
devices(id PK, name, platform, kind, credential_hash UNIQUE, created_at, last_seen_at, revoked_at)
pairing_tokens(token_hash PK, kind, issued_by, expires_at, redeemed_device_id, redeemed_at)
records(book_id, kind, id, version, deleted, payload BLOB, payload_canon_hash, device_id, change_id,
        client_time, seq UNIQUE, conflict_id, PRIMARY KEY(book_id, kind, id))     -- index on seq
book_files(sha256 PK, book_id, size, added_at)           -- register rule 1; index on book_id
book_doc_ids(format, doc_id, book_id, PRIMARY KEY(format, doc_id))                 -- register rule 2
record_versions(book_id, kind, id, version, deleted, payload, device_id, change_id, seq, PK(book_id,kind,id,version))
conflicts(id PK, book_id, kind, record_id, base_version, opened_seq, resolved_seq, resolved_change_id)
conflict_sides(conflict_id, side, change_id, device_id, version, deleted, payload, payload_canon_hash, client_time, PK(conflict_id, side))
applied_changes(change_id PK, device_id, request_hash, result)
batches(batch_id PK, device_id, request_hash, response, created_at)
```
`book_files` and `book_doc_ids` are indexes over the book records, written in the same transaction. The `book` payload is still the record clients see.

### 8.2 `SyncWire.MergeRule`
This is a pure function, `decide(head, openSides, incoming) -> .create | .fastForward | .noop(ref) | .conflict(open|append) | .reject(code)`, plus `canonicalEqual(a,b)`. It has exhaustive tests, and Reader can reuse it.

### 8.3 Binding and launchd
- `config.json` contains `{"bind":["100.97.87.111","127.0.0.1"],"port":8788}`. The server refuses to start with `0.0.0.0`, `::`, or anything outside `100.64.0.0/10`, loopback, or `fd7a:115c:a1e0::/48`. If Tailscale isn't up yet, the server retries the bind every 5 s instead of exiting.
- **LaunchAgent** `~/Library/LaunchAgents/com.arturokuang.reader-sync.plist` (the mini auto-logs in): `ProgramArguments=[~/.local/bin/reader-sync, serve]`, `RunAtLoad`, `KeepAlive`, `ProcessType=Background`, logs in `~/Library/Logs/ReaderSync/`. Install with `launchctl bootstrap gui/$(id -u) <plist>`.
- On SIGTERM the server drains requests, finishes the current transaction and closes the database. Partial uploads stay resumable.
- Admin CLI: `pair`, `devices`, `revoke`, `backup <dir>` (SQLite online backup plus the blobs), `fsck` (re-hash blobs, check every `files[].sha256`).

### 8.4 Crash safety
- Each batch and each register runs as one transaction, and the idempotency rows are written in that same transaction.
- Blobs follow `.part` → fsync → hash → rename → fsync the directory. On startup, a full-size `.part` whose hash matches is finished; one whose hash does not match is deleted.
- WAL with `synchronous=FULL`.

## 9. Security
- Access is limited to the tailnet, and each device has its own credential. Only hashes are stored. Tokens, credentials and payloads never appear in logs.
- The server never reads or writes `/Users/arturokuang/Downloads/SecondBrain`.

---

## 10. Records ↔ Markdown: see `spec/markdown-mapping.md` (pdf reader owns it)
That doc (v2, copied next to this spec from pdf reader's `markdown-mapping.v2.md`) is the authority for how Reader projects records into sidecar and vault files: field mapping (§2), block ids (§3), the delete rule (§3.3), the callout migration (§3.4), book identity (§4) and `reader://` links (§5). This spec takes its wire field names from mapping §2. **All of D1–D12 are resolved as of rev 3.1:**

| # | Resolution |
|---|---|
| D1 | Adopted. Lookups are `kind: lookup` records with `wordId`; the word has no `lookups`; Reader rebuilds them by `wordId`, sorted by `date` then `id`. |
| D2 | Adopted. `spec/schemas/` (plural). |
| D3 | Adopted with a change. Wire types stay in `SyncServer/Sources/SyncWire`. The adapter between them and Reader's types lives in a new Reader-side `ReaderSync` target or in App, **never in ReaderCore**: SyncServer already depends on ReaderCore, so ReaderCore → SyncWire would be a package cycle. |
| D4 | Adopted. FrontMatter is not a wire record; Reader maps `pdfID`/`epubID` to `docId` + `format` at register. |
| D5 | Adopted (closes Q-M3). The vault note keeps the highlight's id. This works because the key is `(bookId, kind, id)`. `highlight.noteId` stays the link of record; on vault reload Reader takes `noteID` from its local sync store before falling back to the block id. |
| D6 | Adopted (closes Q-M1); see §13. |
| D7 | Adopted (closes Q-M2). Reader stops writing callouts; existing ones migrate to server conflicts (§13). |
| D8 | Adopted. New `reader://` links add `fp=<bookId>` and keep `path` for older Readers. Resolve by `fp` (the bookId or any `files[].sha256`), then `path`. |
| D9 | Resolved, verified by pdf reader: PDFKit bounds don't change with `PDFPage.rotation`, so stored rects are already in unrotated page space. No de-rotation needed. |
| D10 | Adopted. The vault open path fills `pdfID`/`epubID`. EPUBs prefer the identifier named by the OPF `<package unique-identifier=…>`, falling back to the first `dc:identifier`. PDFs fall back to `CGPDFDocument.fileIdentifier`. |
| D11 | Confirmed. No prefix/suffix cap on the wire. |
| D12 | Confirmed. `hand-…` vocabulary is not synced in v1. |

Handled on the Reader side (mapping doc): sidecar and vault projection, re-anchoring after a hand-edited quote, ignoring diffs that only reorder, and where Reader keeps `serverVersion`, `dirty`, the outbox and the cursor (outside the vault, for example `Application Support/Reader/Sync/`).

## 11. Requests to pdf reader (we don't edit ReaderCore or App)
1. Done in mapping v2: D1–D12 (§10).
2. Make lookups standalone records in the sync layer (Reader rebuilds `WordEntry.lookups`).
3. Optional: add timestamps to highlights and words through `extras` (only for the conflict UI).
4. Own `spec/fixtures/word-id/` (§12.1), which the server tests and `apps/mobile` both consume. It has 30 cases, including typed ς (`λόγος`, `ΛΌΓΟς`, lone `ς`), all of which keep ς.
5. Later: Reader depends on the `SyncWire` product.

## 12. Tests and QA

### 12.1 Fixtures (`spec/fixtures/`)
- Ours, in `sync/`: request and response goldens for every endpoint, a case for every row of the §6 table, register rules 1–3, and blob HEAD/PUT/Range cases.
- **`word-id/normalize.json`, owned by pdf reader and consumed by the SyncServer tests and `apps/mobile`.** It was generated by running Reader's `WordStore.normalize` on the Mac (Swift 6.2.3, reader@2163c4d). Shape:
  ```json
  {"wordIdRule": "w_ + word.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()", "reader": "...", "swiftVersion": "...",
   "cases": [{"name": "final sigma ΟΔΟΣ", "input": "ΟΔΟΣ", "inputCodepoints": ["U+039F", "U+0394", "U+039F", "U+03A3"],
              "expected": "οδοσ", "expectedCodepoints": ["U+03BF", "U+03B4", "U+03BF", "U+03C3"], "wordId": "w_οδοσ"}]}
  ```
  There are 30 cases (sha256 `6dd305b0…131b8`). `wordId` is `"w_" + expected`, or `null` when the input is empty after trimming (Reader creates no word). **Tests compare the codepoint arrays**, not the `input`/`expected` strings, because Foundation's JSON decoding drops a leading U+FEFF.
  - **The rule:** trim Swift's `CharacterSet.whitespacesAndNewlines` (Unicode `Z*` plus U+0009–U+000D and U+0085; **not** U+FEFF) from both ends. Then lowercase **each character on its own**, with no context-sensitive final-sigma rule: Σ becomes σ wherever it appears, and an already-lowercase ς stays ς. No Unicode normalization is applied, so NFC and NFD `café` are two ids.
  - JS `s.trim().toLowerCase()` fails 4 of the 30 cases (checked with node on the box):
    - `final sigma ΟΔΟΣ`: `toLowerCase()` on a whole string applies the final-sigma rule and gives ς.
    - `leading U+FEFF`: JS `trim()` strips U+FEFF and Swift does not.
    - `trailing U+0085 NEL`: JS `trim()` keeps U+0085.
    - `only whitespace`: JS `trim()` keeps U+0085.
  - A JS normalizer that trims Swift's set and then lowercases each code point on its own (`[...s].map(c => c.toLowerCase()).join("")`) passes all 30. `apps/mobile` uses that and runs this fixture in its tests.
  - **The SyncServer tests must load this file.** They compute the id from `inputCodepoints` through `ReaderCore.WordStore.normalize` and check it against `expectedCodepoints` and `wordId`. They also check that ids are stored and compared byte-exact (NFC and NFD stay two records) and that ids with spaces, tabs, emoji or Unicode survive JSON and URL encoding.
  - Typed ς is covered by 3 cases (added by pdf reader in rev 3.1); a fold to σ fails them.
- pdf reader's: `fingerprint/`, `reanchor/`, `markdown/`, `blocks/` (mapping §7b).

### 12.2 Unit tests (`swift test`, Linux and the mini)
- **SyncWire:** every fixture validates against its schema and round-trips through Codable. `extras` survives byte for byte. `canonicalEqual` ignores key order and number formatting. Every MergeRule row is covered, including create/create equal (noop), create/create different (conflict), adding a side, matching an open side (noop), and tombstone equality, plus position: any `baseVersion` fast-forwards, wrong owner is rejected, a delete is rejected, and no conflict is ever opened.
- **SyncStore:** a batch is atomic (injected failure means nothing commits). A replayed batch returns identical bytes. A reused changeId is rejected. Seq strictly increases. Pull paging returns each record exactly once at its latest state. Register rules 1–3 hold, including two concurrent registers with the same docId producing one book. Resolve: stale base, already resolved, retry. `record_versions` stays complete.
- **BlobStore:** whole and resumed uploads, hash mismatch, wrong offset (409), concurrent uploads of the same sha, startup cleanup, the CryptoKit hash equal to `ReaderCore.SHA256.fingerprint`, and Range `a-b`, `a-`, `-n` and 416.
- **Server:** the bind guard; auth with missing, revoked or wrong-kind credentials; loopback issuing tokens; redeem expiry, reuse and rotation; `kind=book` push rejected.
- **ReaderCore interop:** parse pdf reader's `markdown/` fixtures, convert them to payloads, and check they validate against the schemas and rebuild the same entries (including lookups grouped back under their word).

### 12.3 Harness (`Packages/SyncServer/Harness`)
`swift run sync-harness <scenario|all|soak>` runs N fake devices in one process, each implementing §7. It can use a throwaway server or target the mini.
- Fault injection: drop the response after commit, close the socket mid-body, `kill -9` at debug-only `SYNC_CRASH_AT=<hook>` points, and added latency.
- Invariants checked after each scenario: each device's state equals the server head; every non-head side pushed is still in its conflict; no changeId was applied twice; seq values are unique and increasing; every `files[].sha256` with `hasBlob` re-hashes correctly; and no conflict exists whose sides are all `same`.
- Soak: 3 devices, 10k random edits, deletes, word lookups and offline periods.

### 12.4 End-to-end scenarios (run in the harness first, then on real clients)
1. **Both sides changed:** note v3, A pushes "x" (v4), B pushes "y" with base 3, giving a conflict whose sides are [x, y] on both feeds. Resolving to "x y" gives v5 and closes the conflict.
2. **Identical edits:** A and B both change the color to 3 from the same base, so the second push is a noop and no conflict opens.
3. **Offline replay:** B makes 20 edits offline across 5 records, plus a delete and a create, while seq advances by 1,000. One collapsed batch gives the right mix of applied and conflict, and B catches up from its old cursor.
4. **Duplicate file upload:** HEAD returns 200, so the client skips the upload. A PUT anyway returns 200 without reading the body, and the file exists once on disk. Registering the same sha from a second Mac returns `existing`.
5. **Edited book (docId):** the PDF is re-saved, giving a new sha with the same pdfID. Register returns `attached` with the same bookId, the highlights are unchanged, and `/v1/books` shows the new `sha256` with two `files`.
6. **Crash mid-upload:** `kill -9` at 40% of 200 MB. After a launchd restart, HEAD returns `Upload-Offset`, the client resumes, and the server returns 201 once the hash is verified. A corrupted byte gives 422, and a clean re-upload works.
7. **Retried push:** the response is dropped, so the same batch is sent again and gets the same response, with no duplicates. A `kill -9` during the transaction followed by a retry applies the batch exactly once.
8. **Edit vs delete:** a conflict with a tombstone side. Keeping the edit gives a live record at head+1.
9. **Positions from two offline devices:** A and B each move offline in the same book, and B reconnects last. Both pushes apply (no conflict), B's record has the higher seq, and every client opens at B's position. A pushing to B's position id is rejected `not_owner`.
10. **Same word on two devices:** equal word payloads give one word record (noop), and both lookups survive. Different definitions give a word conflict, while the lookups are unaffected.
11. **Server reset:** a new `serverId`, so clients re-register and re-push with b=0. Nothing is lost.
12. **Pairing:** QR, redeem, push and pull. After a revoke the device gets 401. An expired token gets 403.

### 12.5 QA on real clients (Reader on Mac and Android `apps/mobile`)
Setup: the server runs under launchd on the mini, with Reader on the mini, Reader on a second Mac, and an Android phone on the tailnet.
1. Pair the phone through Reader's QR and the second Mac through the CLI QR. With Tailscale off, the phone cannot connect. The cleartext exception covers only the mini.
2. Add a PDF and an EPUB in Reader. The phone lists both with title, size and format, without downloading them. Opening one downloads it, and toggling airplane mode in the middle resumes the download.
3. A PDF highlight made in Reader appears in the right place on the phone (rects, also checked on a rotated page per D9). A phone highlight appears in the sidecar and, for vault books, as owned blocks only (vault diff has no other changes).
4. An EPUB highlight from the phone is placed by CFI in Reader, or by quote and context if the CFI is wrong.
5. Edit 5 notes on the phone in airplane mode, and 2 of the same notes in Reader. Three apply cleanly and two show "Two versions" on both. Resolving in Reader updates the phone. No `Also changed outside Reader` text appears in the vault.
6. Deleting a highlight on the phone while editing its note in Reader gives a conflict, and keeping the edit restores it.
7. Looking up the same word on both clients gives one word with both lookups.
7a. Reading position: page through a PDF and an EPUB on the phone, close them, and open them in Reader. Reader opens at the phone's page/point and CFI and shows "Opened where you left off on your phone · Back to p. N" using the phone's name and kind from `/v1/devices`. With the book already open in Reader, a newer phone position does not move the page; it applies at the next open.
7b. Put the phone and the second Mac offline, move to different places in the same book, reconnect the Mac first and then the phone. All clients open at the phone's position (last to reach the server), whatever the device clocks say.
7c. After 7a–7b, `GET /v1/conflicts` lists no `position` conflicts, and the feed never shows a `conflict` on a position record.
7d. **Callout migration (Q-M2):** on a copy of a vault book that has `Also changed outside Reader` callouts, run the migration as a dry run first, which writes a report and a backup outside the vault and changes nothing. Then run it for real. Each callout becomes one server conflict with two sides pushed at `baseVersion` 0. The vault files are byte-identical until a side is picked in Reader, after which only that block is rewritten, without a callout.
7e. **Deleted vault file (Q-M1):** delete (or rename) a chapter notes file in Obsidian while Reader is closed, then open the book. Reader pushes no deletes (the server's seq does not move for those records) and offers Restore or Delete. Delete 6 blocks from a file that still parses: Reader asks for confirmation before pushing, because that is more than 5 records.
8. Re-save a PDF in Preview on the Mac, so the book is attached rather than duplicated, and the phone still shows its highlights.
9. Reboot the mini. After auto-login, the LaunchAgent starts and waits for Tailscale, and the clients resync with no duplicates.
10. An `extras` field added on the phone round-trips unchanged through Reader's sidecar.
11. Revoke the phone. It gets 401 and asks to re-pair.

## 13. Open questions
**Spec gate cleared as of rev 3.1.** Nothing below blocks the server build.

No open questions remain.

Closed in rev 3.1:
- **Q8 (Reader PM):** Synced books cannot be renamed in v1. Title, filename and format stay fixed at first register.
- **Q9 (React Native Engineer, Reader PM):** The phone uses a hand-written JS normalizer that matches Swift (§12.1) with no NFC, tested against all 30 `word-id` fixture cases. Reader's rule is unchanged, so no words are re-keyed.
- **Q7 (pdf reader):** Reader pushes its own state. For a vault book, that is what it parsed from the vault plus its local sync store, pushed as a diff against the last accepted records. Hand edits made in Obsidian sync only once the book is opened in Reader.
- **Q-M1 (pdf reader, D6):**
  - A missing or renamed notes file pushes nothing, and Reader offers Restore or Delete.
  - A delete is pushed only when the user deletes in Reader, or when a block is gone from a file that still exists and parses.
  - Reader confirms first if a push would delete more than 5 records, or more than 50% of a book's records.
- **Q-M2 (pdf reader, D7):** Migrating the existing `Also changed outside Reader` callouts:
  - Reader stops writing them.
  - It splits each affected block into its two sides and pushes both at `baseVersion` 0, so the server opens a conflict (or a noop if the sides are identical).
  - The vault stays untouched until Arturo picks a side.
  - A dry run comes first, with a backup outside the vault. QA 12.5 #7d covers this.
- **Q-M3 (pdf reader, D5):** The note id equals the highlight id. This works because the key is `(bookId, kind, id)`.
- **EPUB position accuracy (Reader PM):** The start of the chapter is acceptable for v1. The phone pushes its full CFI and `progression`, and the Mac pushes the chapter only, with in-chapter position as a later item. For the toast, positions in the same chapter are the same spot (§3.4).

Resolved in rev 3: Q2 (v1 = highlights, notes, words, lookups, books, per-device reading position; marks, panes, reading settings, Ask AI history do not sync).
Resolved in rev 2: Q1 (bookId/docId/files), Q3 (lookups as records), Q4 (LaunchAgent), Q5 (plain HTTP), Q6 (full list, download on open).

## 14. Changelog
- **rev 3.1:** Q8 closed (no rename in v1) and Q9 closed (hand-written JS normalizer, no NFC). Port moved from 8787 to 8788, because TailDrive already listens on 127.0.0.1:8787 on the mini.
  - **Spec gate cleared.**
  - §12.1 now describes the real `word-id/normalize.json` (30 cases, compared by codepoint arrays). The rule is: trim Swift's whitespace set, then lowercase each character on its own, with no final-sigma context. Naive JS fails 4 of the cases, and the server tests load the file. pdf reader added 3 typed-ς cases (27 to 30).
  - §10: D1–D12 resolved against mapping v2. The adapter goes in `ReaderSync` or App, never ReaderCore. The note id reuses the highlight id. Links add `fp=`. Rects are already unrotated. EPUB `docId` prefers the OPF unique-identifier.
  - §13: Q7, Q-M1, Q-M2, Q-M3 and EPUB position accuracy closed.
  - §3.4: EPUB positions land at the chapter start in v1, and same-chapter positions don't trigger the toast.
  - QA 12.5 #7d (callout migration) and #7e (a deleted vault file pushes no deletes).
  - Copied `spec/markdown-mapping.md` (mapping v2) and `spec/fixtures/word-id/normalize.json` next to the spec.
- **rev 3:** added the `position` record (Q2 closed): §0, §3.2 table, §3.4 (id `p_<bookId>_<deviceId>`, owner check, base check skipped, newest = highest seq), §5.3, §6, §12 tests and QA (12.4 #9, 12.5 #7a–7c). Positions apply only when a book opens; `GET /v1/devices` is now readable by any paired device, so clients can turn a position's `deviceId` into a name and kind (`mac`/`phone`) for the "Opened where you left off on your phone" toast. New `spec/schemas/position.schema.json`; `position` added to the `kind` enum, the envelope payload mapping and `openapi.yaml` (`Change.kind`, `not_owner` error code).
- **rev 2:** monorepo layout, schemas and codegen, book register rule (bookId/docId/files), lookups as records, identical content never conflicts, alignment with `spec/markdown-mapping.md` (§10).
- **rev 1:** first design.

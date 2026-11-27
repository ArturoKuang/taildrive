import json, os
OUT = "/workspace/reader-sync/spec/schemas"
BASE = "https://spec.reader.arturo-kuang/schemas/"
D2020 = "https://json-schema.org/draft/2020-12/schema"

def ref(name, d=None):
    return {"$ref": f"common.schema.json#/$defs/{d}"} if name == "common" else {"$ref": f"{name}.schema.json"}
C = lambda d: {"$ref": f"common.schema.json#/$defs/{d}"}

def schema(name, title, desc, props, required, extra=None):
    s = {"$schema": D2020, "$id": BASE + f"{name}.schema.json", "title": title, "description": desc,
         "type": "object", "properties": props, "required": required, "additionalProperties": False}
    if extra: s.update(extra)
    return s

common = {
  "$schema": D2020, "$id": BASE + "common.schema.json", "title": "Common definitions",
  "description": "Shared leaf types for Reader sync records. See SYNC_SPEC.md §3.",
  "$defs": {
    "recordId": {"type": "string", "minLength": 1, "maxLength": 512, "pattern": "^[^\\u0000-\\u001f\\u007f]+$",
                 "description": "Client-made id, compared byte-exact. Word ids are 'w_' + WordStore.normalize(word) and may contain spaces/Unicode."},
    "sha256": {"type": "string", "pattern": "^[0-9a-f]{64}$", "description": "Bare lowercase hex SHA-256."},
    "fingerprint": {"type": "string", "pattern": "^sha256:[0-9a-f]{64}$", "description": "Reader fingerprint form: 'sha256:' + hex."},
    "kind": {"enum": ["book", "highlight", "note", "word", "lookup", "position"]},
    "extras": {"type": "object", "description": "Unknown fields. Clients and server pass through untouched.", "additionalProperties": True},
    "page": {"type": ["integer", "null"], "minimum": 1, "description": "1-based PDF page. Hint only."},
    "cfi": {"type": ["string", "null"], "description": "EPUB CFI, raw string. Hint only."},
    "section": {"type": ["string", "null"]},
    "quote": {"type": "string", "description": "Anchor of record."},
    "context": {"type": "string", "description": "prefix/suffix: anchor of record."},
    "rects": {"type": "array", "description": "Hint only. Per line [minX,minY,maxX,maxY], unrotated PDF page space, origin bottom-left. Empty for EPUB.",
              "items": {"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4}},
    "isoTime": {"type": "string", "description": "ISO-8601 timestamp (informational)."},
    "deviceId": {"type": "string", "minLength": 1, "maxLength": 128},
    "changeId": {"type": "string", "minLength": 1, "maxLength": 128}
  }
}

anchor_props = {"page": C("page"), "cfi": C("cfi"), "section": C("section"),
                "quote": C("quote"), "prefix": C("context"), "suffix": C("context")}

highlight = schema("highlight", "Highlight payload", "HighlightEntry (ReaderCore Models.swift) minus id; md comment key 'note' = noteId.",
  {**anchor_props, "color": {"type": "integer", "minimum": 1, "maximum": 6},
   "rects": C("rects"), "noteId": {"anyOf": [C("recordId"), {"type": "null"}], "description": "Link of record to a note."},
   "extras": C("extras")},
  ["color", "quote", "prefix", "suffix", "extras"])

note = schema("note", "Note payload", "NoteEntry minus id. highlightId is an optional back-reference; highlight.noteId wins on disagreement.",
  {**anchor_props, "created": {"type": ["string", "null"]}, "updated": {"type": ["string", "null"]},
   "ai": {"type": "boolean", "default": False}, "provider": {"type": ["string", "null"]},
   "scope": {"type": "string", "default": "passage"}, "body": {"type": "string"},
   "highlightId": {"anyOf": [C("recordId"), {"type": "null"}]}, "extras": C("extras")},
  ["quote", "prefix", "suffix", "ai", "scope", "body", "extras"])

opt_str = {"type": ["string", "null"]}
word = schema("word", "Word payload", "WordEntry minus id and lookups (lookups are separate 'lookup' records). Record id = 'w_' + WordStore.normalize(word).",
  {"word": {"type": "string", "minLength": 1}, "definition": {"type": "string"},
   "explanation": opt_str, "explanationProvider": opt_str, "pronunciation": opt_str, "partOfSpeech": opt_str,
   "learned": {"type": "boolean", "default": False}, "extras": C("extras")},
  ["word", "definition", "learned", "extras"])

lookup = schema("lookup", "Lookup payload", "LookupEntry minus id (md comment key 'lookup'), plus wordId. Reader regroups into WordEntry.lookups by wordId, ordered by date then id.",
  {"wordId": C("recordId"), **anchor_props, "sentence": {"type": "string"}, "date": {"type": "string"}, "extras": C("extras")},
  ["wordId", "quote", "prefix", "suffix", "sentence", "date", "extras"])

book = schema("book", "Book payload (server-written)", "Written only by POST /v1/books. bookId (envelope id) = first-registered fingerprint and never changes.",
  {"title": {"type": "string"}, "filename": {"type": "string"}, "format": {"enum": ["pdf", "epub"]},
   "docId": {"type": ["string", "null"], "description": "pdfID (PDF trailer /ID, first entry, lowercase hex) or epubID (dc:identifier)."},
   "files": {"type": "array", "minItems": 1, "items": {"type": "object", "additionalProperties": False,
             "properties": {"sha256": C("sha256"), "size": {"type": "integer", "minimum": 0}, "addedAt": C("isoTime")},
             "required": ["sha256", "size", "addedAt"]}},
   "currentSha256": C("sha256"), "extras": C("extras")},
  ["title", "filename", "format", "files", "currentSha256", "extras"])

pdf_loc = {"type": "object", "additionalProperties": False, "required": ["format", "page"],
  "properties": {"format": {"const": "pdf"}, "page": {"type": "integer", "minimum": 1, "description": "1-based page, as in highlights."},
                 "point": {"type": "object", "additionalProperties": False, "required": ["x", "y"],
                           "description": "Optional point in unrotated PDF page space, origin bottom-left.",
                           "properties": {"x": {"type": "number"}, "y": {"type": "number"}}}}}
epub_loc = {"type": "object", "additionalProperties": False, "required": ["format", "cfi"],
  "properties": {"format": {"const": "epub"}, "cfi": {"type": "string", "minLength": 1},
                 "progression": {"type": "number", "minimum": 0, "maximum": 1}}}
position = schema("position", "Reading position payload", "One per (book, device); id = 'p_' + bookId + '_' + deviceId. Only that device writes it. Newest = highest server seq, never device clock.",
  {"bookId": C("fingerprint"), "deviceId": C("deviceId"), "locator": {"oneOf": [pdf_loc, epub_loc]}, "extras": C("extras")},
  ["bookId", "deviceId", "locator", "extras"])

kinds = ["book", "highlight", "note", "word", "lookup", "position"]
envelope = schema("envelope", "Record envelope", "Feed entry / stored record. In a push, the server ignores version, deviceId, seq, conflict.",
  {"bookId": C("fingerprint"), "kind": C("kind"), "id": C("recordId"),
   "version": {"type": "integer", "minimum": 1}, "baseVersion": {"type": "integer", "minimum": 0},
   "deleted": {"type": "boolean"}, "deviceId": C("deviceId"), "changeId": C("changeId"),
   "clientTime": {"type": ["string", "null"]}, "seq": {"type": "integer", "minimum": 1},
   "payload": {"type": ["object", "null"]},
   "conflict": {"anyOf": [{"$ref": "conflict.schema.json"}, {"type": "null"}]}},
  ["bookId", "kind", "id", "version", "deleted", "payload"],
  {"allOf": [{"if": {"properties": {"kind": {"const": k}, "deleted": {"const": False}}, "required": ["kind", "deleted"]},
              "then": {"properties": {"payload": {"$ref": f"{k}.schema.json"}}}} for k in kinds]
          + [{"if": {"properties": {"kind": {"const": "book"}}, "required": ["kind"]},
              "then": {"properties": {"id": C("fingerprint")}}},
             {"if": {"properties": {"kind": {"const": "position"}}, "required": ["kind"]},
              "then": {"properties": {"id": {"type": "string", "pattern": "^p_sha256:[0-9a-f]{64}_[^\\u0000-\\u001f\\u007f]+$"},
                                      "deleted": {"const": False}}}}]})

side = {"type": "object", "additionalProperties": False,
        "properties": {"changeId": C("changeId"), "deviceId": C("deviceId"),
                       "version": {"type": ["integer", "null"], "description": "Head version this side became, or null if never head."},
                       "deleted": {"type": "boolean"}, "payload": {"type": ["object", "null"]}, "clientTime": {"type": ["string", "null"]}},
        "required": ["changeId", "deviceId", "version", "deleted", "payload"]}
conflict = schema("conflict", "Conflict", "Open conflict on one record key. Sides are never pairwise identical (identical content is not a conflict).",
  {"conflictId": {"type": "string", "minLength": 1}, "bookId": C("fingerprint"), "kind": C("kind"), "id": C("recordId"),
   "baseVersion": {"type": "integer", "minimum": 0},
   "base": {"anyOf": [{"type": "object", "additionalProperties": False,
                        "properties": {"deleted": {"type": "boolean"}, "payload": {"type": ["object", "null"]}},
                        "required": ["deleted", "payload"]}, {"type": "null"}]},
   "sides": {"type": "array", "minItems": 2, "items": side},
   "openedSeq": {"type": "integer", "minimum": 1}},
  ["conflictId", "bookId", "kind", "id", "baseVersion", "base", "sides", "openedSeq"])

for name, s in [("common", common), ("highlight", highlight), ("note", note), ("word", word), ("lookup", lookup),
                ("book", book), ("position", position), ("envelope", envelope), ("conflict", conflict)]:
    with open(os.path.join(OUT, f"{name}.schema.json"), "w") as f:
        json.dump(s, f, indent=2, ensure_ascii=False); f.write("\n")
print("written")

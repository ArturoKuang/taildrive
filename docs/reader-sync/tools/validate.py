import json, glob, os, sys, yaml
from jsonschema import Draft202012Validator
from referencing import Registry, Resource
from openapi_spec_validator import validate
from openapi_spec_validator.readers import read_from_filename

SPEC = sys.argv[1] if len(sys.argv) > 1 else "/workspace/reader-sync/spec"
schemas = {}
for p in sorted(glob.glob(f"{SPEC}/schemas/*.schema.json")):
    s = json.load(open(p)); Draft202012Validator.check_schema(s); schemas[os.path.basename(p)] = s
reg = Registry().with_resources([(s["$id"], Resource.from_contents(s)) for s in schemas.values()])
V = lambda name: Draft202012Validator(schemas[name], registry=reg)
print("schemas ok:", ", ".join(schemas))

FP = "sha256:" + "ab"*32; H = "cd"*32
hl = {"color": 3, "page": 12, "cfi": None, "quote": "q", "prefix": "a", "suffix": "b", "section": None,
      "rects": [[72, 640.1, 310.5, 652.3]], "noteId": "n_" + "0"*32, "extras": {"future": {"x": 1}}}
note = {"quote": "q", "prefix": "", "suffix": "", "ai": False, "scope": "passage", "body": "hi", "highlightId": "h_1",
        "created": "2026-11-22T08:00:00Z", "updated": None, "extras": {}}
word = {"word": "ice cream", "definition": "d", "learned": False, "extras": {}}
lookup = {"wordId": "w_ice cream", "quote": "ice cream", "prefix": "", "suffix": "", "sentence": "s", "date": "2026-11-22T08:00:00Z", "extras": {}}
book = {"title": "T", "filename": "T.pdf", "format": "pdf", "docId": "4b1e77c0",
        "files": [{"sha256": H, "size": 10, "addedAt": "2026-11-22T08:00:00Z"}], "currentSha256": H, "extras": {}}
pos_pdf = {"bookId": FP, "deviceId": "dev_a", "locator": {"format": "pdf", "page": 12, "point": {"x": 72.0, "y": 640.5}}, "extras": {}}
pos_epub = {"bookId": FP, "deviceId": "dev_a", "locator": {"format": "epub", "cfi": "epubcfi(/6/4!/4/2/1:0)", "progression": 0.42}, "extras": {}}
PID = "p_" + FP + "_dev_a"
def env(kind, id, payload, deleted=False, conflict=None):
    return {"bookId": FP, "kind": kind, "id": id, "version": 1, "baseVersion": 0, "deleted": deleted, "deviceId": "dev_a",
            "changeId": "c_1", "clientTime": None, "seq": 5, "payload": payload, "conflict": conflict}
conf = {"conflictId": "cf_1", "bookId": FP, "kind": "note", "id": "n_1", "baseVersion": 3, "base": {"deleted": False, "payload": note},
        "sides": [{"changeId": "c_a", "deviceId": "dev_a", "version": 4, "deleted": False, "payload": note},
                  {"changeId": "c_b", "deviceId": "dev_b", "version": None, "deleted": True, "payload": None}], "openedSeq": 9}
good = [("highlight", hl), ("note", note), ("word", word), ("lookup", lookup), ("book", book), ("conflict", conf),
        ("envelope", env("highlight", "h_1", hl)), ("envelope", env("word", "w_café", word)),
        ("envelope", env("note", "h_1", None, deleted=True)), ("envelope", env("book", FP, book)),
        ("envelope", env("note", "n_1", note, conflict=conf)),
        ("position", pos_pdf), ("position", pos_epub), ("position", {**pos_pdf, "locator": {"format": "pdf", "page": 1}}),
        ("envelope", env("position", PID, pos_pdf)), ("envelope", env("position", PID, pos_epub))]
bad = [("highlight", {**hl, "color": 9}), ("highlight", {**hl, "unknownTop": 1}), ("word", {**word, "lookups": []}),
       ("envelope", env("highlight", "h_1", {**hl, "color": 0})), ("envelope", env("book", "h_x", book)),
       ("envelope", env("word", "w_\u0007", word)), ("book", {**book, "currentSha256": "sha256:" + H}),
       ("conflict", {**conf, "sides": conf["sides"][:1]}), ("highlight", {**hl, "rects": [[1, 2, 3]]}),
       ("envelope", env("position", "pos_" + FP + "_dev_a", pos_pdf)),                      # wrong id prefix
       ("envelope", env("position", "p_h_1_dev_a", pos_pdf)),                               # id without a bookId
       ("position", {**pos_pdf, "locator": {"format": "pdf", "cfi": "epubcfi(/6/4!/4/2/1:0)"}}),  # EPUB locator on a PDF
       ("position", {**pos_pdf, "locator": {"format": "pdf", "page": 3, "cfi": "epubcfi(/6/4)"}}),  # mixed locator
       ("position", {**pos_epub, "locator": {**pos_epub["locator"], "progression": 1.2}}),  # progression > 1
       ("position", {**pos_pdf, "locator": {"format": "pdf", "page": 0}}),                  # page < 1
       ("position", {**pos_pdf, "scroll": 0.5}),                                            # unknown top-level field
       ("position", {k: v for k, v in pos_pdf.items() if k != "extras"}),                   # extras missing
       ("envelope", env("position", PID, pos_pdf, deleted=True))]                           # positions are never deleted
fail = 0
for n, inst in good:
    errs = list(V(f"{n}.schema.json").iter_errors(inst))
    if errs: fail += 1; print("UNEXPECTED FAIL", n, errs[0].message)
for n, inst in bad:
    if V(f"{n}.schema.json").is_valid(inst): fail += 1; print("UNEXPECTED PASS", n)
print(f"instances: {len(good)} valid, {len(bad)} invalid checked, failures={fail}")

wf = f"{SPEC}/fixtures/word-id/normalize.json"
if os.path.exists(wf):
    w = json.load(open(wf, encoding="utf-8"))
    cp = lambda a: "".join(chr(int(x[2:], 16)) for x in a)
    for c in w["cases"]:
        for k in ("name", "inputCodepoints", "expectedCodepoints", "wordId"):
            if k not in c: fail += 1; print("word-id case missing", k, c.get("name"))
        if c["wordId"] is not None and c["wordId"] != "w_" + cp(c["expectedCodepoints"]):
            fail += 1; print("word-id mismatch", c["name"])
    print(f"word-id fixture ok: {len(w['cases'])} cases")
path = f"{SPEC}/openapi.yaml"
yaml.safe_load(open(path))
d, base = read_from_filename(path)
validate(d, base_uri=base)
print("openapi ok:", len(d["paths"]), "paths")
sys.exit(1 if fail else 0)

//! Pure Reader sync rules (SYNC_SPEC rev 3.2 §3, §6, §12.1): canonical JSON, the merge rule,
//! the position rule and the word-id normalizer. No I/O, no clocks.

use serde_json::Value;

/// Compact JSON with object keys sorted by their UTF-8 bytes. Used for request hashes and for
/// every response body the server stores and replays, so replays are byte-for-byte identical.
pub(crate) fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical(item, out);
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).expect("string serializes"));
                out.push(b':');
                write_canonical(&map[key], out);
            }
            out.push(b'}');
        }
        scalar => out.extend(serde_json::to_vec(scalar).expect("scalar serializes")),
    }
}

/// JSON equality: parsed, key order ignored, numbers compared as numbers (`1 == 1.0`).
pub(crate) fn json_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if let (Some(x), Some(y)) = (x.as_i64(), y.as_i64()) {
                x == y
            } else if let (Some(x), Some(y)) = (x.as_u64(), y.as_u64()) {
                x == y
            } else {
                x.as_f64() == y.as_f64()
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| json_equal(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x
                    .iter()
                    .all(|(key, value)| y.get(key).is_some_and(|other| json_equal(value, other)))
        }
        _ => a == b,
    }
}

/// `canonicalEqual` over stored payload bytes (`None` is JSON null). Identical bytes short-circuit.
pub(crate) fn payload_equal(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) if a == b => true,
        (Some(a), Some(b)) => match (
            serde_json::from_str::<Value>(a),
            serde_json::from_str::<Value>(b),
        ) {
            (Ok(a), Ok(b)) => json_equal(&a, &b),
            _ => false,
        },
        (Some(x), None) | (None, Some(x)) => x.trim() == "null",
    }
}

/// `same(x, y)` from §6: equal `deleted`, and for live records equal payloads. Two tombstones are same.
pub(crate) fn same(deleted: bool, payload: Option<&str>, other_deleted: bool, other: Option<&str>) -> bool {
    if deleted != other_deleted {
        return false;
    }
    deleted || payload_equal(payload, other)
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Head<'a> {
    pub version: i64,
    pub deleted: bool,
    pub payload: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Side<'a> {
    pub deleted: bool,
    pub payload: Option<&'a str>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Incoming<'a> {
    pub base_version: i64,
    pub deleted: bool,
    pub payload: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Create,
    FastForward,
    NoopHead(i64),
    NoopConflict,
    OpenConflict,
    AppendConflict,
    Reject(&'static str),
}

/// The §6 table. Identical content is checked before the base, so an identical push is a noop even
/// with a future `baseVersion`. Clocks are never an input.
pub(crate) fn decide(head: Option<Head<'_>>, open_sides: &[Side<'_>], incoming: Incoming<'_>) -> Decision {
    let base = incoming.base_version;
    let Some(head) = head else {
        return if base == 0 {
            Decision::Create
        } else {
            Decision::Reject("bad_base")
        };
    };
    if same(head.deleted, head.payload, incoming.deleted, incoming.payload) {
        return Decision::NoopHead(head.version);
    }
    if base > head.version {
        return Decision::Reject("bad_base");
    }
    if base == head.version {
        return Decision::FastForward;
    }
    if open_sides
        .iter()
        .any(|side| same(side.deleted, side.payload, incoming.deleted, incoming.payload))
    {
        return Decision::NoopConflict;
    }
    if open_sides.is_empty() {
        Decision::OpenConflict
    } else {
        Decision::AppendConflict
    }
}

pub(crate) fn position_id(book_id: &str, device_id: &str) -> String {
    format!("p_{book_id}_{device_id}")
}

/// §3.4 owner check. `bad_request` for a delete, `not_owner` for a foreign id or payload.
pub(crate) fn position_rejection(
    book_id: &str,
    record_id: &str,
    payload: Option<&str>,
    device_id: &str,
    deleted: bool,
) -> Option<&'static str> {
    if deleted {
        return Some("bad_request");
    }
    if record_id != position_id(book_id, device_id) {
        return Some("not_owner");
    }
    let owned = payload
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .is_some_and(|value| {
            value.get("bookId").and_then(Value::as_str) == Some(book_id)
                && value.get("deviceId").and_then(Value::as_str) == Some(device_id)
        });
    (!owned).then_some("not_owner")
}

/// Positions never conflict: base is ignored, any differing push fast-forwards, identical is a noop.
pub(crate) fn decide_position(head: Option<Head<'_>>, incoming: Incoming<'_>) -> Decision {
    if incoming.deleted {
        return Decision::Reject("bad_request");
    }
    match head {
        None => Decision::Create,
        Some(head) if !head.deleted && payload_equal(head.payload, incoming.payload) => {
            Decision::NoopHead(head.version)
        }
        Some(_) => Decision::FastForward,
    }
}

/// Swift `CharacterSet.whitespacesAndNewlines`: Unicode Z* plus U+0009–U+000D and U+0085. Not U+FEFF.
fn is_swift_whitespace(c: char) -> bool {
    matches!(c,
        '\u{0009}'..='\u{000D}' | '\u{0020}' | '\u{0085}' | '\u{00A0}' | '\u{1680}'
        | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}')
}

/// Reader's `WordStore.normalize`: trim Swift's whitespace set, then lowercase each character on
/// its own. `str::to_lowercase` is deliberately not used: it applies the Greek final-sigma rule.
pub fn normalize_word(word: &str) -> String {
    word.trim_matches(is_swift_whitespace)
        .chars()
        .flat_map(char::to_lowercase)
        .collect()
}

/// `"w_" + normalize(word)`, or `None` when nothing is left after trimming (Reader makes no word).
pub fn word_id(word: &str) -> Option<String> {
    let normalized = normalize_word(word);
    (!normalized.is_empty()).then(|| format!("w_{normalized}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = r#"{"body":"a","extras":{}}"#;
    const B: &str = r#"{"body":"b","extras":{}}"#;
    const C: &str = r#"{"body":"c","extras":{}}"#;

    fn head(version: i64, payload: &str) -> Option<Head<'_>> {
        Some(Head { version, deleted: false, payload: Some(payload) })
    }
    fn live(base_version: i64, payload: &str) -> Incoming<'_> {
        Incoming { base_version, deleted: false, payload: Some(payload) }
    }
    fn side(payload: &str) -> Side<'_> {
        Side { deleted: false, payload: Some(payload) }
    }
    fn tomb(base_version: i64) -> Incoming<'static> {
        Incoming { base_version, deleted: true, payload: None }
    }

    #[test]
    fn word_id_fixture_all_30_cases() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/reader-sync/spec/fixtures/word-id/normalize.json");
        let fixture: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let cases = fixture["cases"].as_array().unwrap();
        assert_eq!(cases.len(), 30);
        let decode = |codepoints: &Value| -> String {
            codepoints
                .as_array()
                .unwrap()
                .iter()
                .map(|cp| char::from_u32(u32::from_str_radix(&cp.as_str().unwrap()[2..], 16).unwrap()).unwrap())
                .collect()
        };
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let input = decode(&case["inputCodepoints"]);
            let expected = decode(&case["expectedCodepoints"]);
            let got = normalize_word(&input);
            assert_eq!(
                got.chars().map(u32::from).collect::<Vec<_>>(),
                expected.chars().map(u32::from).collect::<Vec<_>>(),
                "case {name}"
            );
            assert_eq!(word_id(&input).as_deref(), case["wordId"].as_str(), "wordId for {name}");
        }
    }

    #[test]
    fn naive_string_lowercase_would_fail_final_sigma() {
        assert_eq!("ΟΔΟΣ".to_lowercase(), "οδος");
        assert_eq!(normalize_word("ΟΔΟΣ"), "οδοσ");
        assert_eq!(normalize_word("ΛΌΓΟς"), "λόγος");
        assert_eq!(normalize_word("ς"), "ς");
        assert_eq!(normalize_word("\u{FEFF}word"), "\u{FEFF}word");
    }

    #[test]
    fn canonical_equality_ignores_order_and_number_format() {
        assert!(payload_equal(Some(r#"{"a":1,"b":[1.0,2]}"#), Some(r#"{ "b": [1, 2.0], "a": 1.0 }"#)));
        assert!(!payload_equal(Some(r#"{"a":1,"extras":{"x":1}}"#), Some(r#"{"a":1,"extras":{}}"#)));
        assert!(payload_equal(None, None));
        assert!(!payload_equal(Some(A), None));
        assert_eq!(
            String::from_utf8(canonical_bytes(&json!({"b":1,"a":{"d":[true,null],"c":"é"}}))).unwrap(),
            r#"{"a":{"c":"é","d":[true,null]},"b":1}"#
        );
    }

    #[test]
    fn merge_matrix_every_row() {
        // record absent
        assert_eq!(decide(None, &[], live(0, A)), Decision::Create);
        assert_eq!(decide(None, &[], live(2, A)), Decision::Reject("bad_base"));
        // identical content is never a conflict, whatever the base (including a future base)
        assert_eq!(decide(head(3, A), &[], live(3, A)), Decision::NoopHead(3));
        assert_eq!(decide(head(3, A), &[], live(1, A)), Decision::NoopHead(3));
        assert_eq!(decide(head(3, A), &[], live(9, A)), Decision::NoopHead(3));
        assert_eq!(decide(head(1, A), &[], live(0, A)), Decision::NoopHead(1), "create/create equal");
        assert_eq!(
            decide(head(3, r#"{"extras":{},"body":"a"}"#), &[], live(1, A)),
            Decision::NoopHead(3),
            "key order ignored"
        );
        // different payload, future base
        assert_eq!(decide(head(3, A), &[], live(4, B)), Decision::Reject("bad_base"));
        // b == h: fast-forward (also with an open conflict)
        assert_eq!(decide(head(3, A), &[], live(3, B)), Decision::FastForward);
        assert_eq!(decide(head(3, A), &[side(A), side(B)], live(3, C)), Decision::FastForward);
        // b < h
        assert_eq!(decide(head(3, A), &[], live(2, B)), Decision::OpenConflict);
        assert_eq!(decide(head(1, A), &[], live(0, B)), Decision::OpenConflict, "create/create different");
        assert_eq!(decide(head(4, A), &[side(A), side(B)], live(2, B)), Decision::NoopConflict);
        assert_eq!(decide(head(4, A), &[side(A), side(B)], live(2, C)), Decision::AppendConflict);
        // edit vs delete, delete vs delete, undelete
        let tombstone = Some(Head { version: 4, deleted: true, payload: Some(A) });
        assert_eq!(decide(head(4, A), &[], tomb(3)), Decision::OpenConflict, "delete vs edit");
        assert_eq!(decide(tombstone, &[], live(3, B)), Decision::OpenConflict, "edit vs delete");
        assert_eq!(decide(tombstone, &[], tomb(2)), Decision::NoopHead(4), "delete vs delete");
        assert_eq!(decide(tombstone, &[], live(4, B)), Decision::FastForward, "undelete");
        assert_eq!(
            decide(head(4, A), &[Side { deleted: true, payload: None }, side(A)], tomb(1)),
            Decision::NoopConflict,
            "tombstone side equality"
        );
    }

    #[test]
    fn position_rule() {
        let fp = format!("sha256:{}", "ab".repeat(32));
        let id = position_id(&fp, "dev_a");
        let mine = format!(r#"{{"bookId":"{fp}","deviceId":"dev_a","locator":{{"format":"pdf","page":2}},"extras":{{}}}}"#);
        let theirs = format!(r#"{{"bookId":"{fp}","deviceId":"dev_b","locator":{{"format":"pdf","page":2}},"extras":{{}}}}"#);
        assert_eq!(position_rejection(&fp, &id, Some(&mine), "dev_a", false), None);
        assert_eq!(position_rejection(&fp, &id, Some(&mine), "dev_b", false), Some("not_owner"));
        assert_eq!(position_rejection(&fp, &id, Some(&theirs), "dev_a", false), Some("not_owner"));
        assert_eq!(position_rejection(&fp, &id, Some(&mine), "dev_a", true), Some("bad_request"));
        assert_eq!(decide_position(None, live(7, &mine)), Decision::Create);
        assert_eq!(decide_position(head(5, &mine), live(0, &mine)), Decision::NoopHead(5));
        assert_eq!(decide_position(head(5, &mine), live(0, A)), Decision::FastForward, "base ignored");
        assert_eq!(decide_position(head(5, &mine), live(99, A)), Decision::FastForward);
    }
}

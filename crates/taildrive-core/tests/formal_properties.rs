use std::{collections::BTreeMap, sync::LazyLock};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use proptest::prelude::*;
use taildrive_core::{
    path::{DrivePath, MAX_PATH_BYTES, PathError, conflict_path, parse_folder_path, parse_path},
    update::{
        BuildInfo, ReleaseManifest, UpdateError, generate_signing_keypair, is_update_available,
        resolve_artifact_url, sign_manifest_bytes, validate_update_url, verify_manifest_signature,
    },
};
use unicode_normalization::UnicodeNormalization;

const MAX_CONFLICT_NAME_BYTES: usize = 255;

fn formal_cases(default_cases: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .or_else(|| std::env::var("FORMAL_CASES").ok())
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(default_cases)
}

fn formal_config(default_cases: u32) -> ProptestConfig {
    let mut cfg = ProptestConfig {
        cases: formal_cases(default_cases),
        failure_persistence: None,
        ..ProptestConfig::default()
    };
    if let Ok(seed_str) = std::env::var("FORMAL_SEED")
        && let Ok(seed) = seed_str.parse::<u64>()
    {
        cfg.rng_seed = proptest::test_runner::RngSeed::Fixed(seed);
    }
    cfg
}

/// Generates a single valid path segment (no `/`, `\`, control chars, not empty, not `.` or `..`).
fn arb_valid_segment() -> impl Strategy<Value = String> {
    prop_oneof![
        // ASCII + dots + spaces + punctuation
        "[a-zA-Z0-9._ -]{1,24}".prop_filter("valid segment", |s| {
            !s.is_empty() && s != "." && s != ".."
        }),
        // Leading-dot filenames and multi-extension filenames
        prop_oneof![
            Just(".env".to_owned()),
            Just("..hidden".to_owned()),
            Just("a.tar.gz".to_owned()),
            Just("file.v1.2.pdf".to_owned()),
            Just("...".to_owned()),
        ],
        // Unicode NFC / NFD accents, CJK, Hangul, Emoji
        prop::collection::vec(
            prop_oneof![
                Just("cafe\u{0301}".to_owned()),
                Just("caf\u{00e9}".to_owned()),
                Just("a\u{0308}".to_owned()),
                Just("\u{00e4}".to_owned()),
                Just("n\u{0303}".to_owned()),
                Just("\u{1100}\u{1161}".to_owned()),
                Just("\u{ac00}".to_owned()),
                Just("🎉".to_owned()),
                Just("日本語".to_owned()),
                Just("a".to_owned()),
                Just(".txt".to_owned()),
            ],
            1..=5,
        )
        .prop_map(|parts| parts.concat())
        .prop_filter("valid unicode segment", |s| {
            !s.is_empty() && s != "." && s != ".."
        }),
    ]
}

/// Generates a valid `DrivePath` spanning shallow, nested, Unicode, and near-limit paths.
fn arb_valid_drive_path() -> impl Strategy<Value = DrivePath> {
    prop_oneof![
        // Normal 1..=6 segment paths
        4 => prop::collection::vec(arb_valid_segment(), 1..=6)
            .prop_map(|segs| segs.join("/"))
            .prop_filter_map("within MAX_PATH_BYTES", |raw| parse_path(&raw).ok()),
        // Near-MAX_PATH_BYTES parent with short name (forces tight/1-byte conflict budget & fallback rename)
        1 => (
            (MAX_PATH_BYTES - 20)..=(MAX_PATH_BYTES - 2),
            prop_oneof![
                Just("x".to_owned()),
                Just("c".to_owned()),
                Just("d".to_owned()),
                Just("0".to_owned()),
                Just("a.b".to_owned()),
                Just("é".to_owned()),
            ],
        )
            .prop_filter_map("near-limit parent", |(parent_len, name)| {
                if parent_len + 1 + name.len() > MAX_PATH_BYTES {
                    return None;
                }
                let parent = "p".repeat(parent_len);
                parse_path(&format!("{parent}/{name}")).ok()
            }),
        // Very long single-segment filename or long extension (forces truncation & SHA-256 fallback)
        1 => (
            120usize..=500usize,
            prop_oneof![
                Just(("a", "")),
                Just(("é", ".txt")),
                Just(("🎉", ".tar.gz")),
                Just(("c", ".md")),
            ],
        )
            .prop_filter_map("long filename", |(reps, (unit, ext))| {
                let raw = format!("{}{ext}", unit.repeat(reps));
                parse_path(&raw).ok()
            }),
        // Oversized extension (> 255 bytes) so suffix.len() > budget even with empty host/timestamp
        1 => (240usize..=300usize).prop_filter_map("long extension", |ext_len| {
            let raw = format!("stem.{}", "e".repeat(ext_len));
            parse_path(&raw).ok()
        }),
    ]
}

/// Generates hostile host / timestamp strings full of traversal, slashes, NUL, controls, NFD, and length extremes.
fn arb_hostile_tag_input() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        prop::collection::vec(
            prop_oneof![
                Just("/".to_owned()),
                Just("\\".to_owned()),
                Just("\0".to_owned()),
                Just("\n".to_owned()),
                Just("\r".to_owned()),
                Just("\t".to_owned()),
                Just("\u{007f}".to_owned()),
                Just("\u{0085}".to_owned()),
                Just("..".to_owned()),
                Just(".".to_owned()),
                Just("e\u{0301}".to_owned()),
                Just("\u{0301}".to_owned()),
                Just("\u{0308}".to_owned()),
                Just("host".to_owned()),
                Just("🎉".to_owned()),
            ],
            0..=40,
        )
        .prop_map(|parts| parts.concat()),
        (250usize..=400usize).prop_map(|n| "x".repeat(n)),
        Just(String::new()),
    ]
}

fn arb_control_char() -> impl Strategy<Value = char> {
    prop_oneof![
        (0x00u32..=0x1Fu32).prop_map(|c| char::from_u32(c).unwrap()),
        Just('\u{007F}'),
        (0x80u32..=0x9Fu32).prop_map(|c| char::from_u32(c).unwrap()),
    ]
}

fn make_manifest(build_number: u64, commit_sha: String) -> ReleaseManifest {
    ReleaseManifest {
        schema_version: 1,
        channel: "main".to_owned(),
        version: format!("0.1.0-main.{build_number}+0000000"),
        build_number,
        commit_sha,
        published_at: "2026-11-28T00:00:00Z".to_owned(),
        release_url: "https://example.com/releases/tag/v1".to_owned(),
        notes: String::new(),
        artifacts: BTreeMap::new(),
    }
}

fn make_build_info(channel: String, build_number: u64, commit_sha: String) -> BuildInfo {
    let short_sha = commit_sha.chars().take(7).collect();
    BuildInfo {
        version: format!("0.1.0-{channel}.{build_number}"),
        channel,
        build_number,
        commit_sha,
        short_sha,
        built_at: String::new(),
    }
}

static SIGNING_KEYS: LazyLock<(String, String, String)> = LazyLock::new(|| {
    let (priv_a, pub_a) = generate_signing_keypair().expect("generate primary keypair");
    let (_, pub_b) = generate_signing_keypair().expect("generate secondary keypair");
    (priv_a, pub_a, pub_b)
});

proptest! {
    #![proptest_config(formal_config(256))]

    // -------------------------------------------------------------------------
    // 1. parse_path properties
    // -------------------------------------------------------------------------

    #[test]
    fn parse_path_nfc_equivalence_and_idempotency(
        segs in prop::collection::vec(arb_valid_segment(), 1..=6)
    ) {
        let raw = segs.join("/");
        let nfc_raw: String = raw.nfc().collect();
        let nfd_raw: String = raw.nfd().collect();
        prop_assume!(nfc_raw.len() <= MAX_PATH_BYTES);

        let from_raw = parse_path(&raw).expect("valid raw path");
        let from_nfc = parse_path(&nfc_raw).expect("valid nfc path");
        let from_nfd = parse_path(&nfd_raw).expect("valid nfd path");

        // NFC equivalence
        prop_assert_eq!(&from_raw, &from_nfc);
        prop_assert_eq!(&from_nfc, &from_nfd);

        // Idempotency & canonical NFC form
        prop_assert_eq!(parse_path(from_raw.as_str()), Ok(from_raw.clone()));
        prop_assert_eq!(from_raw.as_str(), nfc_raw.as_str());
        prop_assert_eq!(parse_folder_path(Some(&raw)), Ok(from_raw));
    }

    #[test]
    fn parse_path_universal_postconditions_on_arbitrary_strings(raw in any::<String>()) {
        match parse_path(&raw) {
            Ok(parsed) => {
                let s = parsed.as_str();
                prop_assert!(!s.is_empty(), "accepted empty path");
                prop_assert!(!parsed.is_root(), "parse_path must never return ROOT");
                prop_assert!(!s.starts_with('/'), "accepted leading slash: {s:?}");
                prop_assert!(!s.ends_with('/'), "accepted trailing slash: {s:?}");
                prop_assert!(!s.contains("//"), "accepted double slash: {s:?}");
                prop_assert!(!s.contains('\\'), "accepted backslash: {s:?}");
                prop_assert!(!s.chars().any(char::is_control), "accepted control char: {s:?}");
                prop_assert!(s.len() <= MAX_PATH_BYTES, "accepted > MAX_PATH_BYTES: {}", s.len());
                for seg in s.split('/') {
                    prop_assert!(!seg.is_empty() && seg != "." && seg != "..", "bad segment {seg:?} in {s:?}");
                }
                // Canonical NFC and idempotent
                let nfc: String = s.nfc().collect();
                prop_assert_eq!(s, nfc.as_str());
                prop_assert_eq!(parse_path(s), Ok(parsed));
            }
            Err(_) => {
                // Rejections are fine; empty specifically yields PathError::Empty.
                if raw.is_empty() {
                    prop_assert_eq!(parse_path(&raw), Err(PathError::Empty));
                }
            }
        }
    }

    #[test]
    fn parse_path_rejects_all_forbidden_constructions(
        prefix in prop::collection::vec(arb_valid_segment(), 0..=3),
        suffix in prop::collection::vec(arb_valid_segment(), 0..=3),
        ctrl in arb_control_char(),
    ) {
        let p = prefix.join("/");
        let s = suffix.join("/");

        // Empty
        prop_assert_eq!(parse_path(""), Err(PathError::Empty));

        // Leading slash
        let leading = if s.is_empty() { "/a".to_owned() } else { format!("/{s}") };
        prop_assert_eq!(parse_path(&leading), Err(PathError::Absolute));

        // Trailing slash
        let trailing = if p.is_empty() { "a/".to_owned() } else { format!("{p}/") };
        prop_assert_eq!(parse_path(&trailing), Err(PathError::BadSegment));

        // Double slash
        let doubled = format!("{}//{}", if p.is_empty() { "a" } else { &p }, if s.is_empty() { "b" } else { &s });
        prop_assert_eq!(parse_path(&doubled), Err(PathError::BadSegment));

        // "." and ".." segments at start, middle, or end
        for bad in [".", ".."] {
            let mut segs = prefix.clone();
            segs.push(bad.to_owned());
            segs.extend(suffix.clone());
            let candidate = segs.join("/");
            prop_assert_eq!(parse_path(&candidate), Err(PathError::BadSegment));
        }

        // Backslash anywhere
        let with_backslash = format!("{p}\\{s}");
        prop_assert_eq!(parse_path(&with_backslash), Err(PathError::ForbiddenChar));

        // Control char anywhere (C0, DEL, C1)
        let with_ctrl = format!("{p}{ctrl}{s}");
        prop_assert_eq!(parse_path(&with_ctrl), Err(PathError::ForbiddenChar));

        // Exceeding MAX_PATH_BYTES in UTF-8 bytes
        let over_ascii = "a".repeat(MAX_PATH_BYTES + 1);
        prop_assert_eq!(parse_path(&over_ascii), Err(PathError::TooLong));
        let over_utf8 = "é".repeat(MAX_PATH_BYTES / 2 + 1);
        prop_assert_eq!(parse_path(&over_utf8), Err(PathError::TooLong));
    }

    #[test]
    fn parse_path_parent_and_ancestors_structural_consistency(path in arb_valid_drive_path()) {
        let ancestors = path.ancestors();
        let parent = path.parent();
        let name = path.name();

        prop_assert!(!name.is_empty());
        prop_assert!(!name.contains('/'));

        // Strictly increasing proper prefixes, each a valid DrivePath
        for pair in ancestors.windows(2) {
            prop_assert!(pair[0].as_str().len() < pair[1].as_str().len());
            let expected_prefix = format!("{}/", pair[0].as_str());
            prop_assert!(pair[1].as_str().starts_with(&expected_prefix));
        }
        for anc in &ancestors {
            prop_assert_eq!(parse_path(anc.as_str()), Ok(anc.clone()));
            let expected_prefix = format!("{}/", anc.as_str());
            prop_assert!(path.as_str().starts_with(&expected_prefix));
        }

        // Consistency between ancestors(), parent(), and name()
        if ancestors.is_empty() {
            prop_assert!(parent.is_root());
            prop_assert_eq!(path.as_str(), name);
        } else {
            prop_assert!(!parent.is_root());
            prop_assert_eq!(ancestors.last(), Some(&parent));
            prop_assert_eq!(path.as_str(), format!("{}/{name}", parent.as_str()));
        }

        // Iterating parent() back to ROOT yields exact reverse of ancestors()
        let mut walked = Vec::new();
        let mut cur = path.parent();
        while !cur.is_root() {
            walked.push(cur.clone());
            cur = cur.parent();
        }
        walked.reverse();
        prop_assert_eq!(walked, ancestors);
    }

    // -------------------------------------------------------------------------
    // 2. conflict_path properties
    // -------------------------------------------------------------------------

    #[test]
    fn conflict_path_always_valid_distinct_and_preserves_parent(
        path in arb_valid_drive_path(),
        host in arb_hostile_tag_input(),
        timestamp in arb_hostile_tag_input(),
    ) {
        let out = conflict_path(&path, &host, &timestamp);

        // Always parses cleanly and is already canonical NFC
        prop_assert_eq!(
            parse_path(out.as_str()),
            Ok(out.clone()),
            "conflict_path produced invalid/non-canonical path {:?} from path={:?} host={:?} ts={:?}",
            out.as_str(),
            path.as_str(),
            host,
            timestamp
        );
        // Never equals input path
        prop_assert_ne!(&out, &path);
        // Always preserves parent
        prop_assert_eq!(out.parent(), path.parent());
        // Respects total path limit and conflict filename byte limit
        prop_assert!(out.as_str().len() <= MAX_PATH_BYTES);
        prop_assert!(out.name().len() <= MAX_CONFLICT_NAME_BYTES);

        // Second-order conflict application also remains valid and distinct
        let out2 = conflict_path(&out, &host, &timestamp);
        prop_assert_eq!(parse_path(out2.as_str()), Ok(out2.clone()));
        prop_assert_ne!(&out2, &out);
        prop_assert_eq!(out2.parent(), path.parent());
        prop_assert!(out2.as_str().len() <= MAX_PATH_BYTES);
        prop_assert!(out2.name().len() <= MAX_CONFLICT_NAME_BYTES);
    }

    // -------------------------------------------------------------------------
    // 3. update anti-downgrade, URL validation, and Ed25519 signature soundness
    // -------------------------------------------------------------------------

    #[test]
    fn update_is_available_strict_anti_downgrade(
        current_build in 1u64..=10_000,
        manifest_build in 0u64..=10_000,
        current_sha in "[0-9a-f]{7,40}",
        manifest_sha in prop_oneof![
            Just(String::new()),
            "[0-9a-f]{7,40}",
        ],
        channel in prop_oneof![Just("main".to_owned()), Just("beta".to_owned()), Just("stable".to_owned())],
        allow_dev_upgrade in any::<bool>(),
    ) {
        let current = make_build_info(channel, current_build, current_sha.clone());
        let manifest = make_manifest(manifest_build, manifest_sha.clone());

        let available = is_update_available(&current, &manifest, allow_dev_upgrade);

        if manifest_build <= current_build || manifest_sha.is_empty() || manifest_sha == current_sha {
            prop_assert!(!available, "must never downgrade or re-offer same/empty commit");
        } else {
            prop_assert!(available, "strictly newer build with distinct non-empty commit must be available");
        }

        // Same-commit manifest is never offered even with higher build_number
        let same_commit_higher = make_manifest(current_build + 1, current_sha);
        prop_assert!(!is_update_available(&current, &same_commit_higher, allow_dev_upgrade));
    }

    #[test]
    fn update_is_available_dev_build_gate(
        current_build in 0u64..=100,
        manifest_build in 0u64..=100,
        manifest_sha in prop_oneof![Just(String::new()), Just("dev".to_owned()), "[0-9a-f]{8}"],
        is_dev_channel in any::<bool>(),
        allow_dev_upgrade in any::<bool>(),
    ) {
        let channel = if is_dev_channel { "dev".to_owned() } else { "main".to_owned() };
        let build_num = if is_dev_channel { current_build } else { 0 };
        let current = make_build_info(channel, build_num, "dev".to_owned());
        let manifest = make_manifest(manifest_build, manifest_sha.clone());

        let expected = allow_dev_upgrade && !manifest_sha.is_empty() && manifest_sha != "dev";
        prop_assert_eq!(is_update_available(&current, &manifest, allow_dev_upgrade), expected);
    }

    #[test]
    fn validate_update_url_rejects_non_https_and_non_loopback_http(
        bad_scheme in prop_oneof![
            Just("ftp"),
            Just("file"),
            Just("javascript"),
            Just("data"),
            Just("ws"),
            Just("wss"),
            Just("ssh"),
            Just("gopher"),
            Just("mailto"),
            Just("blob"),
        ],
        host in prop_oneof![
            Just("evil.com".to_owned()),
            Just("github.com".to_owned()),
            Just("127.0.0.2".to_owned()),
            Just("0.0.0.0".to_owned()),
            Just("10.0.0.1".to_owned()),
            Just("100.64.0.1".to_owned()),
            Just("localhost.evil.com".to_owned()),
            Just("evil-localhost".to_owned()),
            Just("[::2]".to_owned()),
            "[a-z]{2,10}\\.(com|net|org|io)".prop_filter("not localhost", |h| h != "localhost"),
        ],
        path in "/[a-zA-Z0-9._/-]{0,32}",
    ) {
        // Non-HTTP(S) schemes are always rejected
        let non_http = format!("{bad_scheme}://{host}{path}");
        prop_assert!(
            matches!(validate_update_url(&non_http), Err(UpdateError::InvalidUrl(_))),
            "accepted forbidden scheme URL: {non_http}"
        );
        if bad_scheme == "javascript" || bad_scheme == "data" {
            let opaque = format!("{bad_scheme}:alert(1)");
            prop_assert!(matches!(validate_update_url(&opaque), Err(UpdateError::InvalidUrl(_))));
        }

        // Plain HTTP to any non-loopback host is always rejected
        let plain_http = format!("http://{host}{path}");
        prop_assert!(
            matches!(validate_update_url(&plain_http), Err(UpdateError::InvalidUrl(_))),
            "accepted non-loopback HTTP URL: {plain_http}"
        );

        // resolve_artifact_url must not downgrade from HTTPS manifest to non-loopback HTTP or forbidden scheme
        let manifest_url = "https://releases.example.com/v1/manifest.json";
        prop_assert!(resolve_artifact_url(manifest_url, &plain_http).is_err());
        prop_assert!(resolve_artifact_url(manifest_url, &non_http).is_err());

        // HTTPS to valid host is accepted
        let https_url = format!("https://{host}{path}");
        prop_assert!(validate_update_url(&https_url).is_ok(), "rejected valid HTTPS URL: {https_url}");
    }

    #[test]
    fn validate_update_url_accepts_loopback_http(
        loopback in prop_oneof![Just("127.0.0.1"), Just("localhost"), Just("[::1]")],
        port in prop::option::of(1024u16..=65535),
        path in "/[a-zA-Z0-9._/-]{0,24}",
    ) {
        let url = match port {
            Some(p) => format!("http://{loopback}:{p}{path}"),
            None => format!("http://{loopback}{path}"),
        };
        prop_assert!(validate_update_url(&url).is_ok(), "rejected loopback HTTP URL: {url}");
    }

    #[test]
    fn ed25519_manifest_signature_rejects_any_single_bit_flip(
        payload in prop::collection::vec(any::<u8>(), 1..=2048),
        bit_index in any::<usize>(),
        sig_bit_index in any::<usize>(),
    ) {
        let (priv_b64, pub_b64, other_pub_b64) = &*SIGNING_KEYS;
        let sig_b64 = sign_manifest_bytes(&payload, priv_b64).expect("sign manifest");

        // Authentic payload verifies
        prop_assert!(verify_manifest_signature(&payload, &sig_b64, pub_b64).is_ok());

        // Wrong public key fails
        prop_assert!(matches!(
            verify_manifest_signature(&payload, &sig_b64, other_pub_b64),
            Err(UpdateError::InvalidSignature)
        ));

        // Any 1-bit flip in manifest bytes fails
        let total_bits = payload.len() * 8;
        let flip = bit_index % total_bits;
        let mut tampered = payload.clone();
        tampered[flip / 8] ^= 1u8 << (flip % 8);
        prop_assert!(matches!(
            verify_manifest_signature(&tampered, &sig_b64, pub_b64),
            Err(UpdateError::InvalidSignature)
        ));

        // Any 1-bit flip in signature bytes fails
        let mut sig_bytes = BASE64.decode(&sig_b64).expect("valid b64 sig");
        let sig_flip = sig_bit_index % (sig_bytes.len() * 8);
        sig_bytes[sig_flip / 8] ^= 1u8 << (sig_flip % 8);
        let tampered_sig = BASE64.encode(&sig_bytes);
        prop_assert!(matches!(
            verify_manifest_signature(&payload, &tampered_sig, pub_b64),
            Err(UpdateError::InvalidSignature)
        ));
    }
}

#[test]
fn parse_folder_path_root_semantics() {
    assert_eq!(parse_folder_path(None), Ok(DrivePath::ROOT));
    assert_eq!(parse_folder_path(Some("")), Ok(DrivePath::ROOT));
    assert!(DrivePath::ROOT.is_root());
    assert!(DrivePath::ROOT.parent().is_root());
    assert!(DrivePath::ROOT.ancestors().is_empty());
}

//! Reader sync conformance tests over real TCP (SYNC_SPEC rev 3.2 §12): pairing, the merge matrix,
//! idempotency, feed paging, positions, books, blobs, disk_full and schema conformance.

use std::{net::SocketAddr, time::Duration};

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use taildrive_core::hash::sha256_hex;
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::TcpListener, sync::oneshot, task::JoinHandle};

use crate::{App, config::Config};

const ADMIN: (&str, &str) = ("tailscale-user-login", "arturo@test");

struct Srv {
    addr: SocketAddr,
    http: Client,
    app: App,
    dir: TempDir,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

async fn start(dir: TempDir, tweak: impl FnOnce(&mut Config)) -> Srv {
    let mut config = Config::new(dir.path());
    config.bind = SocketAddr::from(([127, 0, 0, 1], 0));
    config.free_floor_bytes = 0;
    tweak(&mut config);
    let listener = TcpListener::bind(config.bind).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = App::new(config).await.unwrap();
    let (shutdown, signal) = oneshot::channel::<()>();
    let serving = app.clone();
    let task = tokio::spawn(async move {
        serving
            .serve(listener, async {
                let _ = signal.await;
            })
            .await
    });
    let http = Client::builder().no_proxy().timeout(Duration::from_secs(20)).build().unwrap();
    Srv { addr, http, app, dir, shutdown, task }
}

async fn fresh() -> Srv {
    start(tempfile::tempdir().unwrap(), |_| {}).await
}

fn sha(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

fn fp(bytes: &[u8]) -> String {
    format!("sha256:{}", sha(bytes))
}

fn note(body: &str) -> Value {
    json!({"quote": "q", "prefix": "", "suffix": "", "ai": false, "scope": "passage", "body": body, "extras": {}})
}

fn change(cid: &str, book: &str, kind: &str, id: &str, base: i64, payload: Value) -> Value {
    json!({"changeId": cid, "bookId": book, "kind": kind, "id": id, "baseVersion": base, "deleted": false,
           "clientTime": "2026-11-26T00:00:00Z", "payload": payload})
}

fn tombstone(cid: &str, book: &str, kind: &str, id: &str, base: i64) -> Value {
    json!({"changeId": cid, "bookId": book, "kind": kind, "id": id, "baseVersion": base, "deleted": true, "payload": null})
}

impl Srv {
    fn url(&self, path: &str) -> String {
        format!("http://{}/v1/reader{path}", self.addr)
    }

    async fn stop(self) -> TempDir {
        let _ = self.shutdown.send(());
        let _ = self.task.await;
        drop(self.app);
        self.dir
    }

    async fn issue(&self, kind: &str) -> Value {
        let response = self
            .http
            .post(self.url("/v1/pairing/tokens"))
            .header(ADMIN.0, ADMIN.1)
            .json(&json!({"kind": kind}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn redeem(&self, token: &str, device: &str, platform: &str) -> (StatusCode, Value) {
        let response = self
            .http
            .post(self.url("/v1/pairing/redeem"))
            .json(&json!({"token": token, "deviceId": device, "name": format!("{device} name"), "platform": platform, "appVersion": "t"}))
            .send()
            .await
            .unwrap();
        (response.status(), response.json().await.unwrap_or(Value::Null))
    }

    async fn pair(&self, device: &str, platform: &str) -> String {
        let kind = if platform == "macos" { "mac" } else { "phone" };
        let token = self.issue(kind).await["token"].as_str().unwrap().to_owned();
        let (status, body) = self.redeem(&token, device, platform).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["credential"].as_str().unwrap().to_owned()
    }

    async fn send(&self, method: reqwest::Method, cred: &str, path: &str, body: Option<&Value>) -> (StatusCode, Value) {
        let mut request = self.http.request(method, self.url(path)).bearer_auth(cred);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    async fn get(&self, cred: &str, path: &str) -> (StatusCode, Value) {
        self.send(reqwest::Method::GET, cred, path, None).await
    }

    async fn push_raw(&self, cred: &str, body: String) -> (StatusCode, String) {
        let response = self
            .http
            .post(self.url("/v1/changes"))
            .bearer_auth(cred)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        (response.status(), response.text().await.unwrap())
    }

    async fn push(&self, cred: &str, batch: &str, changes: Vec<Value>) -> (StatusCode, Value) {
        let body = json!({"batchId": batch, "changes": changes});
        self.send(reqwest::Method::POST, cred, "/v1/changes", Some(&body)).await
    }

    /// Pushes one change and returns its result.
    async fn one(&self, cred: &str, batch: &str, change: Value) -> Value {
        let (status, body) = self.push(cred, batch, vec![change]).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["results"][0].clone()
    }

    async fn register(&self, cred: &str, file: &[u8], doc_id: Option<&str>, format: &str) -> Value {
        let body = json!({"sha256": sha(file), "size": file.len(), "format": format, "docId": doc_id,
                          "title": "Title", "filename": format!("Book.{format}")});
        let (status, response) = self.send(reqwest::Method::POST, cred, "/v1/books", Some(&body)).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        response
    }

    async fn feed(&self, cred: &str, since: i64) -> Vec<Value> {
        let (status, page) = self.get(cred, &format!("/v1/changes?since={since}&limit=1000")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["hasMore"], false);
        page["changes"].as_array().unwrap().clone()
    }

    async fn record(&self, cred: &str, kind: &str, id: &str) -> Value {
        self.feed(cred, 0)
            .await
            .into_iter()
            .find(|e| e["kind"] == kind && e["id"] == id)
            .unwrap_or(Value::Null)
    }

    async fn seq(&self) -> i64 {
        let response = self.http.get(self.url("/v1/health")).send().await.unwrap();
        response.json::<Value>().await.unwrap()["seq"].as_i64().unwrap()
    }
}

#[tokio::test]
async fn pairing_qr_redeem_rotation_expiry_revoke_and_rate_limit() {
    let srv = start(tempfile::tempdir().unwrap(), |c| c.api_tokens = vec!["td-token".to_owned()]).await;
    let health: Value = srv.http.get(srv.url("/v1/health")).send().await.unwrap().json().await.unwrap();
    assert_eq!(health["ok"], true);
    let server_id = health["serverId"].as_str().unwrap().to_owned();
    assert!(server_id.starts_with("srv_"));

    // Issuing needs TailDrive auth or a mac credential.
    let anonymous = srv.http.post(srv.url("/v1/pairing/tokens")).json(&json!({"kind": "phone"})).send().await.unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let issued = srv.issue("phone").await;
    let token = issued["token"].as_str().unwrap();
    assert_eq!(
        issued["qr"].as_str().unwrap(),
        format!("readersync://pair?v=2&url=https://arturos-mac-mini.tail8dc761.ts.net/v1/reader&token={token}&server={server_id}")
    );
    assert_eq!(token.len(), 39, "24 random bytes in base32");

    let (status, first) = srv.redeem(token, "dev_phone", "android").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["kind"], "phone");
    assert_eq!(first["serverId"], server_id.as_str());
    let old = first["credential"].as_str().unwrap().to_owned();
    assert_eq!(srv.get(&old, "/v1/devices").await.0, StatusCode::OK);

    // A lost response is safe to retry: same token + same device rotates the credential.
    let (status, second) = srv.redeem(token, "dev_phone", "android").await;
    assert_eq!(status, StatusCode::OK);
    let phone = second["credential"].as_str().unwrap().to_owned();
    assert_ne!(old, phone);
    assert_eq!(srv.get(&old, "/v1/devices").await.0, StatusCode::UNAUTHORIZED, "old credential revoked");
    assert_eq!(srv.get(&phone, "/v1/devices").await.0, StatusCode::OK);
    assert_eq!(srv.redeem(token, "dev_other", "android").await.0, StatusCode::FORBIDDEN, "token bound to one device");

    // Expiry after 10 minutes.
    let late = srv.issue("phone").await["token"].as_str().unwrap().to_owned();
    srv.app.state.reader.store.clock.advance(601);
    assert_eq!(srv.redeem(&late, "dev_late", "android").await.0, StatusCode::FORBIDDEN);

    // Only mac devices (or TailDrive auth) issue tokens and revoke.
    let mac = srv.pair("dev_mac", "macos").await;
    let by_phone = srv.send(reqwest::Method::POST, &phone, "/v1/pairing/tokens", Some(&json!({"kind": "phone"}))).await;
    assert_eq!(by_phone.0, StatusCode::FORBIDDEN);
    let by_mac = srv.send(reqwest::Method::POST, &mac, "/v1/pairing/tokens", Some(&json!({"kind": "phone"}))).await;
    assert_eq!(by_mac.0, StatusCode::OK);
    let (_, devices) = srv.get(&phone, "/v1/devices").await;
    let listed = devices["devices"].as_array().unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|d| d.get("credential").is_none() && d["name"].is_string() && d["kind"].is_string()));
    assert_eq!(srv.send(reqwest::Method::DELETE, &phone, "/v1/devices/dev_mac", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(srv.send(reqwest::Method::DELETE, &mac, "/v1/devices/nobody", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(srv.send(reqwest::Method::DELETE, &mac, "/v1/devices/dev_phone", None).await.0, StatusCode::NO_CONTENT);
    let (status, error) = srv.get(&phone, "/v1/changes?since=0").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked device gets 401");
    assert_eq!(error["error"]["code"], "unauthorized");

    // Reader credentials and TailDrive API tokens are separate namespaces.
    assert_eq!(srv.get("td-token", "/v1/changes?since=0").await.0, StatusCode::UNAUTHORIZED);
    let taildrive = srv.http.get(format!("http://{}/v1/health", srv.addr)).bearer_auth("td-token").send().await.unwrap();
    assert_eq!(taildrive.status(), StatusCode::OK, "TailDrive routes still work beside /v1/reader");
    let td_with_device = srv.http.get(format!("http://{}/v1/health", srv.addr)).bearer_auth(&mac).send().await.unwrap();
    assert_eq!(td_with_device.status(), StatusCode::UNAUTHORIZED);
    let unknown = srv.http.get(srv.url("/v1/nope")).send().await.unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    assert_eq!(unknown.json::<Value>().await.unwrap()["error"]["code"], "not_found");

    // 10 failed redeems per minute (two happened above), then 429.
    for _ in 0..8 {
        assert_eq!(srv.redeem("WRONG", "dev_x", "ios").await.0, StatusCode::FORBIDDEN);
    }
    assert_eq!(srv.redeem("WRONG", "dev_x", "ios").await.1["error"]["code"], "rate_limited");
    srv.stop().await;
}

#[tokio::test]
async fn merge_matrix_conflicts_and_resolution_over_http() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let b = srv.pair("dev_b", "android").await;
    let file = b"book bytes";
    let book = fp(file);
    let registered = srv.register(&a, file, None, "pdf").await;
    assert_eq!(registered["status"], "created");
    assert_eq!(registered["bookId"], book.as_str());

    // create, fast-forward, identical (even with a future base) is a noop
    let r = srv.one(&a, "b1", change("c1", &book, "note", "n_1", 0, note("v1"))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("applied"), Some(1)));
    let r = srv.one(&a, "b2", change("c2", &book, "note", "n_1", 1, note("v2"))).await;
    assert_eq!(r["version"], 2);
    let before = srv.seq().await;
    let r = srv.one(&b, "b3", change("c3", &book, "note", "n_1", 9, note("v2"))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("noop"), Some(2)));
    assert!(r.get("seq").is_none());
    assert_eq!(srv.seq().await, before, "noop assigns no seq");
    // different payload with a future base: per-change bad_base; absent record with b>0 too
    let r = srv.one(&b, "b4", change("c4", &book, "note", "n_1", 3, note("x"))).await;
    assert_eq!((r["status"].as_str(), r["error"]["code"].as_str()), (Some("rejected"), Some("bad_base")));
    let r = srv.one(&b, "b5", change("c5", &book, "note", "n_absent", 2, note("x"))).await;
    assert_eq!(r["error"]["code"], "bad_base");

    // both changed: conflict keeps both, head unchanged
    let r = srv.one(&b, "b6", change("c6", &book, "note", "n_1", 1, note("from b"))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("conflict"), Some(2)));
    let conflict_id = r["conflictId"].as_str().unwrap().to_owned();
    let r = srv.one(&a, "b7", change("c7", &book, "note", "n_1", 1, note("third"))).await;
    assert_eq!((r["status"].as_str(), r["conflictId"].as_str()), (Some("conflict"), Some(conflict_id.as_str())), "append side");
    let r = srv.one(&a, "b8", change("c8", &book, "note", "n_1", 1, note("from b"))).await;
    assert_eq!((r["status"].as_str(), r["conflictId"].as_str()), (Some("noop"), Some(conflict_id.as_str())), "matches a side");
    let head = srv.record(&b, "note", "n_1").await;
    assert_eq!(head["version"], 2);
    assert_eq!(head["payload"]["body"], "v2", "head unchanged by a conflict");
    let conflict = &head["conflict"];
    assert_eq!(conflict["conflictId"], conflict_id.as_str());
    assert_eq!(conflict["baseVersion"], 1);
    assert_eq!(conflict["base"]["payload"]["body"], "v1");
    let bodies: Vec<&str> = conflict["sides"].as_array().unwrap().iter().map(|s| s["payload"]["body"].as_str().unwrap()).collect();
    assert_eq!(bodies, ["v2", "from b", "third"]);
    assert_eq!(conflict["sides"][0]["version"], 2);
    assert_eq!(conflict["sides"][1]["version"], Value::Null);
    let (_, listed) = srv.get(&b, &format!("/v1/conflicts?bookId={book}")).await;
    assert_eq!(listed["conflicts"].as_array().unwrap().len(), 1);

    // resolve: stale base, success, idempotent retry, already resolved
    let path = format!("/v1/conflicts/{conflict_id}/resolve");
    let stale = srv.send(reqwest::Method::POST, &a, &path, Some(&json!({"changeId": "r0", "baseVersion": 1, "deleted": false, "payload": note("x y")}))).await;
    assert_eq!((stale.0, stale.1["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("stale_base")));
    let body = json!({"changeId": "r1", "baseVersion": 2, "deleted": false, "payload": note("x y")});
    let resolved = srv.http.post(srv.url(&path)).bearer_auth(&a).json(&body).send().await.unwrap().text().await.unwrap();
    let parsed: Value = serde_json::from_str(&resolved).unwrap();
    assert_eq!((parsed["status"].as_str(), parsed["version"].as_i64()), (Some("applied"), Some(3)));
    let again = srv.http.post(srv.url(&path)).bearer_auth(&a).json(&body).send().await.unwrap().text().await.unwrap();
    assert_eq!(again, resolved, "resolve is idempotent by changeId");
    let closed = srv.send(reqwest::Method::POST, &b, &path, Some(&json!({"changeId": "r2", "baseVersion": 3, "deleted": false, "payload": note("z")}))).await;
    assert_eq!((closed.0, closed.1["error"]["code"].as_str()), (StatusCode::CONFLICT, Some("already_resolved")));
    let head = srv.record(&b, "note", "n_1").await;
    assert_eq!((head["version"].as_i64(), head["conflict"].is_null()), (Some(3), true));
    assert_eq!(head["payload"]["body"], "x y");
    assert!(srv.get(&b, "/v1/conflicts").await.1["conflicts"].as_array().unwrap().is_empty());

    // edit vs delete is a conflict with a tombstone side; keeping the edit gives head+1 live
    srv.one(&a, "e1", change("e1", &book, "highlight", "h_1", 0, json!({"color": 1, "quote": "q", "prefix": "", "suffix": "", "extras": {}}))).await;
    srv.one(&a, "e2", change("e2", &book, "highlight", "h_1", 1, json!({"color": 2, "quote": "q", "prefix": "", "suffix": "", "extras": {}}))).await;
    let r = srv.one(&b, "e3", tombstone("e3", &book, "highlight", "h_1", 1)).await;
    assert_eq!(r["status"], "conflict");
    let conflict = srv.record(&a, "highlight", "h_1").await["conflict"].clone();
    assert_eq!(conflict["sides"][1]["deleted"], true);
    let keep = json!({"changeId": "e4", "baseVersion": 2, "deleted": false, "payload": conflict["sides"][0]["payload"]});
    let kept = srv.send(reqwest::Method::POST, &a, &format!("/v1/conflicts/{}/resolve", conflict["conflictId"].as_str().unwrap()), Some(&keep)).await;
    assert_eq!(kept.1["version"], 3);
    let head = srv.record(&a, "highlight", "h_1").await;
    assert_eq!((head["deleted"].as_bool(), head["payload"]["color"].as_i64()), (Some(false), Some(2)));
    // delete, delete vs delete (noop), undelete (fast-forward from the tombstone)
    let r = srv.one(&a, "d1", tombstone("d1", &book, "highlight", "h_1", 3)).await;
    assert_eq!(r["version"], 4);
    let tomb = srv.record(&a, "highlight", "h_1").await;
    assert_eq!((tomb["deleted"].as_bool(), tomb["payload"]["color"].as_i64()), (Some(true), Some(2)), "tombstone keeps last payload");
    let r = srv.one(&b, "d2", tombstone("d2", &book, "highlight", "h_1", 2)).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("noop"), Some(4)));
    let r = srv.one(&b, "d3", change("d3", &book, "highlight", "h_1", 4, json!({"color": 5, "quote": "q", "prefix": "", "suffix": "", "extras": {}}))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("applied"), Some(5)));

    // word create/create: equal is a noop, different is a conflict with base null; lookups never collide
    let word = |d: &str| json!({"word": "ice cream", "definition": d, "learned": false, "extras": {}});
    let wid = crate::reader::wire::word_id(" Ice Cream ").unwrap();
    assert_eq!(wid, "w_ice cream");
    srv.one(&a, "w1", change("w1", &book, "word", &wid, 0, word("cold"))).await;
    assert_eq!(srv.one(&b, "w2", change("w2", &book, "word", &wid, 0, word("cold"))).await["status"], "noop");
    let r = srv.one(&b, "w3", change("w3", &book, "word", &wid, 0, word("frozen"))).await;
    assert_eq!(r["status"], "conflict");
    assert_eq!(srv.record(&a, "word", &wid).await["conflict"]["base"], Value::Null);
    let lookup = |id: &str| change(id, &book, "lookup", id, 0, json!({"wordId": wid, "quote": "q", "prefix": "", "suffix": "", "sentence": "s", "date": "2026-11-26T00:00:00Z", "extras": {}}));
    assert_eq!(srv.one(&a, "l1", lookup("l_a")).await["status"], "applied");
    assert_eq!(srv.one(&b, "l2", lookup("l_b")).await["status"], "applied");

    // ids are byte-exact: NFC and NFD stay two records; spaces and emoji survive
    srv.one(&a, "u1", change("u1", &book, "word", "w_caf\u{e9}", 0, word("nfc"))).await;
    srv.one(&a, "u2", change("u2", &book, "word", "w_cafe\u{301}", 0, word("nfd"))).await;
    srv.one(&a, "u3", change("u3", &book, "word", "w_👨‍👩‍👧 family  x", 0, word("emoji"))).await;
    let feed = srv.feed(&a, 0).await;
    assert!(feed.iter().any(|e| e["id"] == "w_caf\u{e9}" && e["payload"]["definition"] == "nfc"));
    assert!(feed.iter().any(|e| e["id"] == "w_cafe\u{301}" && e["payload"]["definition"] == "nfd"));
    assert!(feed.iter().any(|e| e["id"] == "w_👨‍👩‍👧 family  x"));
    let (status, _) = srv.push(&a, "u4", vec![change("u4", &book, "word", "w_tab\there", 0, word("x"))]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "control characters are not allowed in ids");
    srv.stop().await;
}

#[tokio::test]
async fn batch_rules_idempotency_and_exact_payload_bytes() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let book = fp(b"file");
    srv.register(&a, b"file", None, "epub").await;

    // kind book -> 400 use_register; unknown book -> per-change rejected; duplicate key -> 400
    let (status, body) = srv.push(&a, "k1", vec![change("k1", &book, "book", &book, 0, json!({}))]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::BAD_REQUEST, Some("use_register")));
    let other = fp(b"unregistered");
    let r = srv.one(&a, "k2", change("k2", &other, "note", "n_1", 0, note("x"))).await;
    assert_eq!((r["status"].as_str(), r["error"]["code"].as_str()), (Some("rejected"), Some("unknown_book")));
    let (status, _) = srv.push(&a, "k3", vec![change("k3", &book, "note", "n", 0, note("a")), change("k4", &book, "note", "n", 0, note("b"))]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let too_many: Vec<Value> = (0..501).map(|i| change(&format!("m{i}"), &book, "note", &format!("n{i}"), 0, note("x"))).collect();
    assert_eq!(srv.push(&a, "k5", too_many).await.0, StatusCode::BAD_REQUEST);

    // the payload is stored and returned as the exact bytes received
    let odd = r#"{ "extras" : {"z":1, "a":[1.50,2]}, "body":"é\u00e9",  "quote":"q","prefix":"","suffix":"","ai":false,"scope":"passage" }"#;
    let raw_batch = format!(r#"{{"batchId":"x1","changes":[{{"changeId":"x1","bookId":"{book}","kind":"note","id":"n_raw","baseVersion":0,"deleted":false,"payload":{odd}}}]}}"#);
    let (status, first) = srv.push_raw(&a, raw_batch.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let feed = srv.http.get(srv.url("/v1/changes?since=0")).bearer_auth(&a).send().await.unwrap().text().await.unwrap();
    assert!(feed.contains(&format!(r#""payload":{odd}"#)), "exact payload bytes in the feed");

    // replaying a batch returns the stored response byte for byte and applies nothing twice
    let seq = srv.seq().await;
    let (_, replay) = srv.push_raw(&a, raw_batch.replace(r#"{"batchId":"x1","#, r#"{ "batchId" : "x1", "#)).await;
    assert_eq!(replay, first);
    assert_eq!(srv.seq().await, seq);
    // same batchId, different body -> 422 change_id_reused
    let (status, body) = srv.push(&a, "x1", vec![change("x9", &book, "note", "n_other", 0, note("x"))]).await;
    assert_eq!((status, body["error"]["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("change_id_reused")));
    // an applied changeId inside a new batch returns its stored result; a different body is rejected
    let r = srv.one(&a, "y1", change("c_same", &book, "note", "n_y", 0, note("y"))).await;
    let again = srv.one(&a, "y2", change("c_same", &book, "note", "n_y", 0, note("y"))).await;
    assert_eq!(again, r);
    let reused = srv.one(&a, "y3", change("c_same", &book, "note", "n_y", 1, note("changed"))).await;
    assert_eq!((reused["status"].as_str(), reused["error"]["code"].as_str()), (Some("rejected"), Some("change_id_reused")));

    // a failure inside the transaction commits nothing; the retry applies exactly once
    let seq = srv.seq().await;
    srv.app.state.reader.store.fail_next_push.store(true, std::sync::atomic::Ordering::SeqCst);
    let batch: Vec<Value> = (0..20).map(|i| change(&format!("t{i}"), &book, "note", &format!("n_t{i}"), 0, note("t"))).collect();
    assert_eq!(srv.push(&a, "t", batch.clone()).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(srv.seq().await, seq, "nothing committed");
    let (status, body) = srv.push(&a, "t", batch.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["results"].as_array().unwrap().iter().all(|r| r["status"] == "applied"));
    assert_eq!(srv.push(&a, "t", batch).await.1, body, "retry after success replays");
    let feed = srv.feed(&a, 0).await;
    assert_eq!(feed.iter().filter(|e| e["id"].as_str().is_some_and(|id| id.starts_with("n_t"))).count(), 20, "no duplicates");
    srv.stop().await;
}

#[tokio::test]
async fn feed_pages_each_record_once_at_its_latest_state() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let book = fp(b"paged");
    srv.register(&a, b"paged", None, "pdf").await;
    let changes: Vec<Value> = (0..25).map(|i| change(&format!("p{i}"), &book, "note", &format!("n{i:02}"), 0, note("v1"))).collect();
    srv.push(&a, "p", changes).await;
    srv.one(&a, "u", change("u", &book, "note", "n03", 1, note("v2"))).await;
    let (mut since, mut seen, mut pages) = (0, Vec::new(), 0);
    loop {
        let (_, page) = srv.get(&a, &format!("/v1/changes?since={since}&limit=7")).await;
        pages += 1;
        let changes = page["changes"].as_array().unwrap();
        for e in changes {
            assert!(e["seq"].as_i64().unwrap() > since);
            seen.push((e["id"].as_str().unwrap().to_owned(), e["seq"].as_i64().unwrap(), e["payload"]["body"].clone()));
        }
        since = page["cursor"].as_i64().unwrap();
        if page["hasMore"] == false {
            break;
        }
    }
    assert_eq!(pages, 4);
    assert_eq!(seen.len(), 26, "book + 25 notes, each once");
    assert!(seen.windows(2).all(|w| w[0].1 < w[1].1), "strictly increasing seq");
    let n03: Vec<_> = seen.iter().filter(|(id, _, _)| id == "n03").collect();
    assert_eq!(n03.len(), 1);
    assert_eq!(n03[0].2, "v2");
    assert_eq!(seen.last().unwrap().0, "n03", "an edited record moves to the end of the feed");
    let (_, empty) = srv.get(&a, &format!("/v1/changes?since={since}")).await;
    assert_eq!((empty["changes"].as_array().unwrap().len(), empty["cursor"].as_i64()), (0, Some(since)));
    assert_eq!(srv.get(&a, "/v1/changes?since=0&limit=1001").await.0, StatusCode::BAD_REQUEST);
    srv.stop().await;
}

#[tokio::test]
async fn positions_are_owner_only_never_conflict_and_newest_is_highest_seq() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let b = srv.pair("dev_b", "android").await;
    let book = fp(b"pos");
    srv.register(&a, b"pos", None, "pdf").await;
    let pos = |device: &str, page: i64| json!({"bookId": book, "deviceId": device, "locator": {"format": "pdf", "page": page}, "extras": {}});
    let pa = format!("p_{book}_dev_a");
    let pb = format!("p_{book}_dev_b");
    let r = srv.one(&a, "1", change("1", &book, "position", &pa, 0, pos("dev_a", 3))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("applied"), Some(1)));
    let r = srv.one(&a, "2", change("2", &book, "position", &pa, 0, pos("dev_a", 9))).await;
    assert_eq!((r["status"].as_str(), r["version"].as_i64()), (Some("applied"), Some(2)), "stale base still fast-forwards");
    let r = srv.one(&a, "3", change("3", &book, "position", &pa, 77, pos("dev_a", 9))).await;
    assert_eq!(r["status"], "noop");
    let r = srv.one(&b, "4", change("4", &book, "position", &pa, 2, pos("dev_a", 1))).await;
    assert_eq!(r["error"]["code"], "not_owner", "B cannot write A's id");
    let r = srv.one(&b, "5", change("5", &book, "position", &pb, 0, pos("dev_a", 1))).await;
    assert_eq!(r["error"]["code"], "not_owner", "payload deviceId must be the caller");
    let r = srv.one(&a, "6", tombstone("6", &book, "position", &pa, 2)).await;
    assert_eq!(r["error"]["code"], "bad_request", "positions are never deleted");
    let rb = srv.one(&b, "7", change("7", &book, "position", &pb, 0, pos("dev_b", 40))).await;
    assert!(rb["seq"].as_i64() > r["seq"].as_i64().or(Some(0)));
    let feed = srv.feed(&a, 0).await;
    let newest = feed.iter().filter(|e| e["kind"] == "position").max_by_key(|e| e["seq"].as_i64()).unwrap();
    assert_eq!(newest["deviceId"], "dev_b");
    assert_eq!(newest["payload"]["locator"]["page"], 40);
    assert!(feed.iter().all(|e| e["conflict"].is_null()));
    assert!(srv.get(&a, "/v1/conflicts").await.1["conflicts"].as_array().unwrap().is_empty());
    srv.stop().await;
}

#[tokio::test]
async fn book_register_rules_existing_attached_created() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let b = srv.pair("dev_b", "macos").await;
    let v1 = srv.register(&a, b"pdf v1", Some("4b1e77c0"), "pdf").await;
    assert_eq!(v1["status"], "created");
    assert_eq!(v1["book"]["version"], 1);
    assert_eq!(v1["book"]["kind"], "book");
    let book = v1["bookId"].as_str().unwrap().to_owned();
    assert_eq!(srv.register(&b, b"pdf v1", None, "pdf").await["status"], "existing");
    let v2 = srv.register(&b, b"pdf v2 (re-saved)", Some("4b1e77c0"), "pdf").await;
    assert_eq!((v2["status"].as_str(), v2["bookId"].as_str()), (Some("attached"), Some(book.as_str())));
    assert_eq!(v2["book"]["version"], 2);
    assert_eq!(v2["book"]["payload"]["currentSha256"], sha(b"pdf v2 (re-saved)").as_str());
    assert_eq!(srv.register(&b, b"pdf v2 (re-saved)", Some("4b1e77c0"), "pdf").await["status"], "existing", "same answer twice");
    assert_eq!(srv.register(&a, b"an epub", Some("4b1e77c0"), "epub").await["status"], "created", "formats never match");
    assert_eq!(srv.register(&a, b"no doc id", Some(""), "pdf").await["status"], "created", "empty docId ignored");
    let (_, books) = srv.get(&a, "/v1/books").await;
    let listed = books["books"].as_array().unwrap();
    assert_eq!(listed.len(), 3);
    let first = listed.iter().find(|x| x["bookId"] == book.as_str()).unwrap();
    assert_eq!(first["files"].as_array().unwrap().len(), 2);
    assert_eq!((first["sha256"].as_str(), first["hasBlob"].as_bool()), (Some(sha(b"pdf v2 (re-saved)").as_str()), Some(false)));
    assert_eq!(first["size"], b"pdf v2 (re-saved)".len());
    // attach shows up in the feed with a new seq
    let feed = srv.feed(&a, 0).await;
    assert_eq!(feed.iter().find(|e| e["id"] == book.as_str()).unwrap()["version"], 2);
    srv.stop().await;
}

async fn put(srv: &Srv, cred: &str, hex: &str, body: Vec<u8>, range: Option<String>) -> reqwest::Response {
    let mut request = srv.http.put(srv.url(&format!("/v1/blobs/{hex}"))).bearer_auth(cred);
    if let Some(range) = range {
        request = request.header("content-range", range);
    }
    request.body(body).send().await.unwrap()
}

async fn head(srv: &Srv, cred: &str, hex: &str) -> (StatusCode, Option<u64>, Option<u64>) {
    let r = srv.http.head(srv.url(&format!("/v1/blobs/{hex}"))).bearer_auth(cred).send().await.unwrap();
    let num = |name: &str| r.headers().get(name).and_then(|v| v.to_str().ok()?.parse().ok());
    (r.status(), num("content-length"), num("upload-offset"))
}

/// Sends `Content-Length: total` but only `part` bytes, then drops the socket.
async fn interrupted_put(srv: &Srv, cred: &str, hex: &str, total: usize, part: &[u8]) {
    let mut stream = tokio::net::TcpStream::connect(srv.addr).await.unwrap();
    let head = format!(
        "PUT /v1/reader/v1/blobs/{hex} HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {cred}\r\nContent-Length: {total}\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).await.unwrap();
    stream.write_all(part).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(stream);
}

async fn wait_offset(srv: &Srv, cred: &str, hex: &str, want: u64) {
    for _ in 0..100 {
        if head(srv, cred, hex).await.2 == Some(want) {
            // the handler may still hold the per-sha lock for a moment
            tokio::time::sleep(Duration::from_millis(100)).await;
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("Upload-Offset never reached {want}");
}

#[tokio::test]
async fn blobs_dedupe_head_range_resume_restart_and_hash_check() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let content: Vec<u8> = (0..300_000_u32).map(|i| (i % 251) as u8).collect();
    let hex = sha(&content);
    let h = head(&srv, &a, &hex).await;
    assert_eq!((h.0, h.2), (StatusCode::NOT_FOUND, Some(0)));

    // interrupted upload keeps its bytes; HEAD reports the offset
    interrupted_put(&srv, &a, &hex, content.len(), &content[..100_000]).await;
    wait_offset(&srv, &a, &hex, 100_000).await;
    // wrong offset -> 409 with Upload-Offset
    let wrong = put(&srv, &a, &hex, content[50_000..].to_vec(), Some(format!("bytes 50000-299999/{}", content.len()))).await;
    assert_eq!(wrong.status(), StatusCode::CONFLICT);
    assert_eq!(wrong.headers()["upload-offset"], "100000");

    // the offset survives a server restart (kill/relaunch) and the upload resumes
    let dir = srv.stop().await;
    let srv = start(dir, |_| {}).await;
    assert_eq!(head(&srv, &a, &hex).await.2, Some(100_000));
    let partial = put(&srv, &a, &hex, content[100_000..200_000].to_vec(), Some(format!("bytes 100000-199999/{}", content.len()))).await;
    assert_eq!(partial.status(), StatusCode::ACCEPTED);
    assert_eq!(partial.headers()["upload-offset"], "200000");
    let done = put(&srv, &a, &hex, content[200_000..].to_vec(), Some(format!("bytes 200000-299999/{}", content.len()))).await;
    assert_eq!(done.status(), StatusCode::CREATED);
    let h = head(&srv, &a, &hex).await;
    assert_eq!((h.0, h.1), (StatusCode::OK, Some(content.len() as u64)));

    // content-addressed: duplicate upload is a no-op, stored once under reader/sha256/<hex>
    let dup = put(&srv, &a, &hex, content.clone(), None).await;
    assert_eq!(dup.status(), StatusCode::OK);
    let stored = srv.dir.path().join("blobs/reader/sha256").join(&hex);
    assert_eq!(std::fs::read(&stored).unwrap(), content);
    assert_eq!(std::fs::read_dir(srv.dir.path().join("blobs/reader/sha256")).unwrap().count(), 1);
    assert!(!srv.app.state.reader.blobs.part_exists(&hex).await);

    // GET with Range a-b, a-, -n, If-Range, 416
    let get = |range: Option<&str>, if_range: Option<&str>| {
        let mut r = srv.http.get(srv.url(&format!("/v1/blobs/{hex}"))).bearer_auth(&a);
        if let Some(range) = range { r = r.header("range", range.to_owned()); }
        if let Some(tag) = if_range { r = r.header("if-range", tag.to_owned()); }
        r.send()
    };
    let full = get(None, None).await.unwrap();
    assert_eq!(full.status(), StatusCode::OK);
    assert_eq!(full.headers()["etag"], format!("\"{hex}\"").as_str());
    assert_eq!(full.headers()["accept-ranges"], "bytes");
    assert_eq!(full.bytes().await.unwrap().as_ref(), content.as_slice());
    let r = get(Some("bytes=10-19"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(r.headers()["content-range"], "bytes 10-19/300000");
    assert_eq!(r.bytes().await.unwrap().as_ref(), &content[10..20]);
    let r = get(Some("bytes=299990-"), None).await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), &content[299_990..]);
    let r = get(Some("bytes=-5"), None).await.unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), &content[299_995..]);
    let r = get(Some("bytes=10-19"), Some("\"stale\"")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "If-Range mismatch sends the whole blob");
    let r = get(Some("bytes=10-19"), Some(&format!("\"{hex}\""))).await.unwrap();
    assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
    let r = get(Some("bytes=400000-"), None).await.unwrap();
    assert_eq!(r.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(r.headers()["content-range"], "bytes */300000");
    assert_eq!(r.json::<Value>().await.unwrap()["error"]["code"], "range_not_satisfiable");

    // a corrupted upload is discarded with 422 hash_mismatch; a clean re-upload works
    let other = b"another book".to_vec();
    let other_hex = sha(&other);
    let bad = put(&srv, &a, &other_hex, b"another b00k".to_vec(), None).await;
    assert_eq!(bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(bad.json::<Value>().await.unwrap()["error"]["code"], "hash_mismatch");
    assert_eq!(head(&srv, &a, &other_hex).await.2, Some(0));
    assert_eq!(put(&srv, &a, &other_hex, other.clone(), None).await.status(), StatusCode::CREATED);
    // registering then listing shows hasBlob
    srv.register(&a, &other, None, "pdf").await;
    let (_, books) = srv.get(&a, "/v1/books").await;
    assert_eq!(books["books"][0]["hasBlob"], true);
    assert_eq!(srv.get(&a, "/v1/blobs/NOTHEX").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(srv.http.get(srv.url(&format!("/v1/blobs/{hex}"))).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
    srv.stop().await;
}

#[tokio::test]
async fn startup_finishes_a_complete_part_and_drops_a_corrupt_one() {
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let good = b"complete but never published".to_vec();
    let corrupt = b"this one will not match".to_vec();
    let (good_hex, corrupt_hex) = (sha(&good), sha(&corrupt));
    let dir = srv.stop().await;
    // Simulate a crash after the last byte was written but before the hash/rename.
    let uploads = dir.path().join("reader/uploads");
    std::fs::write(uploads.join(format!("{good_hex}.part")), &good).unwrap();
    std::fs::write(uploads.join(format!("{corrupt_hex}.part")), b"this one will NOT match").unwrap();
    std::fs::write(uploads.join("orphan.part"), b"x").unwrap();
    let conn = rusqlite::Connection::open(dir.path().join("reader.db")).unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    conn.execute("INSERT INTO uploads VALUES (?1, ?2, ?3)", rusqlite::params![good_hex, good.len() as i64, now]).unwrap();
    conn.execute("INSERT INTO uploads VALUES (?1, ?2, ?3)", rusqlite::params![corrupt_hex, corrupt.len() as i64, now]).unwrap();
    drop(conn);
    let srv = start(dir, |_| {}).await;
    assert_eq!(head(&srv, &a, &good_hex).await.0, StatusCode::OK);
    let h = head(&srv, &a, &corrupt_hex).await;
    assert_eq!((h.0, h.2), (StatusCode::NOT_FOUND, Some(0)));
    assert!(!srv.dir.path().join("reader/uploads/orphan.part").exists());
    srv.stop().await;
}

#[tokio::test]
async fn disk_floor_blocks_book_uploads_with_507_but_records_still_push() {
    let srv = start(tempfile::tempdir().unwrap(), |c| c.free_floor_bytes = u64::MAX / 4).await;
    let a = srv.pair("dev_a", "macos").await;
    let file = b"a big book".to_vec();
    let registered = srv.register(&a, &file, None, "pdf").await;
    assert_eq!(registered["status"], "created", "register is metadata, not an upload");
    let blocked = put(&srv, &a, &sha(&file), file.clone(), None).await;
    assert_eq!(blocked.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(blocked.json::<Value>().await.unwrap()["error"]["code"], "disk_full");
    let r = srv.one(&a, "1", change("1", &fp(&file), "highlight", "h_1", 0, json!({"color": 1, "quote": "q", "prefix": "", "suffix": "", "extras": {}}))).await;
    assert_eq!(r["status"], "applied", "record pushes never check the free-disk floor");
    assert_eq!(head(&srv, &a, &sha(&file)).await.2, Some(0));
    srv.stop().await;

    // A book quota (off by default) is separate from TailDrive's files quota.
    let srv = start(tempfile::tempdir().unwrap(), |c| c.reader_book_quota_bytes = Some(5)).await;
    let a = srv.pair("dev_a", "macos").await;
    let over = put(&srv, &a, &sha(&file), file.clone(), None).await;
    assert_eq!(over.status(), StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(over.json::<Value>().await.unwrap()["error"]["code"], "quota_exceeded");
    srv.stop().await;
    assert_eq!(Config::new("/tmp").reader_book_quota_bytes, None, "no cap by default");
}

fn spec_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/reader-sync/spec")
}

fn validators() -> (jsonschema::Registry<'static>, std::collections::HashMap<String, Value>) {
    let mut schemas = std::collections::HashMap::new();
    let mut registry = jsonschema::Registry::new();
    for entry in std::fs::read_dir(spec_dir().join("schemas")).unwrap() {
        let path = entry.unwrap().path();
        let schema: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().trim_end_matches(".schema.json").to_owned();
        registry = registry.add(schema["$id"].as_str().unwrap(), schema.clone()).unwrap();
        schemas.insert(name, schema);
    }
    (registry.prepare().unwrap(), schemas)
}

#[tokio::test]
async fn schema_samples_and_every_server_output_validate() {
    let (registry, schemas) = validators();
    let validator = |name: &str| jsonschema::options().with_registry(&registry).build(&schemas[name]).unwrap();
    // The validator's sample instances (spec/fixtures/sync/schema-samples.json).
    let samples: Value = serde_json::from_str(&std::fs::read_to_string(spec_dir().join("fixtures/sync/schema-samples.json")).unwrap()).unwrap();
    let (good, bad) = (samples["valid"].as_array().unwrap(), samples["invalid"].as_array().unwrap());
    assert!(good.len() >= 16 && bad.len() >= 18);
    for case in good {
        let v = validator(case["schema"].as_str().unwrap());
        assert!(v.is_valid(&case["instance"]), "valid sample rejected: {case}");
    }
    for case in bad {
        let v = validator(case["schema"].as_str().unwrap());
        assert!(!v.is_valid(&case["instance"]), "invalid sample accepted: {case}");
    }

    // Push every valid record payload through the server; every feed envelope and conflict validates.
    let srv = fresh().await;
    let a = srv.pair("dev_a", "macos").await;
    let file = b"schema book";
    let book = fp(file);
    srv.register(&a, file, Some("doc"), "pdf").await;
    srv.register(&a, b"schema book v2", Some("doc"), "pdf").await;
    let mut n = 0;
    for case in good {
        let kind = case["schema"].as_str().unwrap();
        if !["highlight", "note", "word", "lookup", "position"].contains(&kind) {
            continue;
        }
        n += 1;
        let mut payload = case["instance"].clone();
        let id = if kind == "position" {
            payload["bookId"] = json!(book);
            payload["deviceId"] = json!("dev_a");
            format!("p_{book}_dev_a")
        } else {
            format!("{}_{n}", &kind[..1])
        };
        let r = srv.one(&a, &format!("s{n}"), change(&format!("s{n}"), &book, kind, &id, 0, payload)).await;
        assert_ne!(r["status"], "rejected", "{r}");
    }
    srv.one(&a, "c1", change("c1", &book, "note", "n_c", 0, note("a"))).await;
    srv.one(&a, "c2", change("c2", &book, "note", "n_c", 1, note("b"))).await;
    srv.one(&a, "c3", change("c3", &book, "note", "n_c", 1, note("c"))).await;
    srv.one(&a, "c4", tombstone("c4", &book, "note", "h_1", 0)).await;
    srv.one(&a, "c5", change("c5", &book, "note", "n_d", 0, note("a"))).await;
    srv.one(&a, "c6", tombstone("c6", &book, "note", "n_d", 1)).await;
    let envelope = validator("envelope");
    let conflict = validator("conflict");
    let feed = srv.feed(&a, 0).await;
    assert!(feed.len() >= 9, "{}", feed.len());
    for e in &feed {
        let errors: Vec<String> = envelope.iter_errors(e).map(|err| err.to_string()).collect();
        assert!(errors.is_empty(), "feed envelope fails the schema: {e}\n{errors:?}");
    }
    assert!(feed.iter().any(|e| !e["conflict"].is_null()));
    for c in srv.get(&a, "/v1/conflicts").await.1["conflicts"].as_array().unwrap() {
        assert!(conflict.is_valid(c), "{c}");
    }
    srv.stop().await;
}

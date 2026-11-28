use std::{
    net::{Ipv4Addr, SocketAddr},
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{future::join_all, stream};
use reqwest::{Client, Response, StatusCode};
use taildrive_core::{
    client::{ClientError, TailDriveClient, UploadBody},
    hash::{Sha256Hasher, sha256_hex},
    models::{
        CreateShareRequest, ErrorEnvelope, FileType, InitUploadRequest, QuotaStatus, UploadResponse,
    },
};
use taildrive_server::{
    App,
    config::Config,
    storage::{ByteStream, LocalFsStorage, S3Storage, StorageBackend, StorageError},
};
use tempfile::TempDir;
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    sync::{Barrier, Notify, mpsc, oneshot},
    task::JoinHandle,
    time::{sleep, timeout},
};

const TOKEN: &str = "integration-test-token";

struct Server {
    addr: SocketAddr,
    http: Client,
    shutdown: oneshot::Sender<()>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn start(config: Config, storage: Option<Arc<dyn StorageBackend>>) -> Self {
        let listener = TcpListener::bind(config.bind).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = match storage {
            Some(storage) => App::with_storage(config, storage).await.unwrap(),
            None => App::new(config).await.unwrap(),
        };
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            app.serve(listener, async {
                let _ = signal.await;
            })
            .await
        });
        let http = Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        Self {
            addr,
            http,
            shutdown,
            task,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.addr.port())
    }

    fn typed(&self) -> TailDriveClient {
        TailDriveClient::new(&self.url(""), Some(TOKEN.to_owned())).unwrap()
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn config(dir: &TempDir, quota: u64) -> Config {
    let mut config = Config::new(dir.path());
    config.api_tokens = vec!["separate-test-token".to_owned(), TOKEN.to_owned()];
    config.bind = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    config.quota_bytes = quota;
    config.free_floor_bytes = 0;
    config
}

async fn assert_error(response: Response, status: StatusCode, code: &str) -> ErrorEnvelope {
    assert_eq!(response.status(), status);
    let body = response.json::<ErrorEnvelope>().await.unwrap();
    assert_eq!(body.error.code, code);
    assert!(!body.error.message.is_empty());
    assert!(body.error.details.is_object());
    body
}

fn client_error(error: ClientError, status: StatusCode, code: &str) {
    match error {
        ClientError::Api {
            status: actual,
            body,
        } => {
            assert_eq!(actual, status, "{body:?}");
            assert_eq!(body.error.code, code);
        }
        error => panic!("expected {status} {code}, got {error:?}"),
    }
}

fn files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            files.extend(self::files(&entry.path()));
        } else {
            files.push(entry.path());
        }
    }
    files
}

async fn until(mut condition: impl FnMut() -> bool) {
    timeout(Duration::from_secs(5), async {
        while !condition() {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition was not met");
}

async fn put(client: &TailDriveClient, path: &str, bytes: &'static [u8]) -> UploadResponse {
    client
        .put_file(
            path,
            UploadBody::from_bytes(Bytes::from_static(bytes)),
            None,
            None,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn authentication_uses_the_real_tcp_peer() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(&dir, 1000);
    cfg.bind = SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0));
    let server = Server::start(cfg, None).await;
    let url = server.url("/v1/health");

    let proxy = server
        .http
        .get(&url)
        .header("tailscale-user-login", "alice@ts.net")
        .header("x-forwarded-for", "100.64.0.7")
        .send()
        .await
        .unwrap();
    assert_eq!(proxy.status(), StatusCode::OK);
    let health: taildrive_core::models::HealthResponse = proxy.json().await.unwrap();
    assert_eq!(health.tailnet_user.as_deref(), Some("alice@ts.net"));
    assert_eq!(health.storage_driver, "local-fs");
    assert_eq!(health.node, "macmini-ci");
    assert!(health.quota.disk_free_bytes.is_some());

    assert_error(
        server
            .http
            .get(&url)
            .header("x-forwarded-for", "127.0.0.1")
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "ERR_UNAUTHORIZED",
    )
    .await;
    assert_error(
        server
            .http
            .get(&url)
            .bearer_auth("integration-test-tokeN")
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "ERR_UNAUTHORIZED",
    )
    .await;
    assert!(
        server
            .typed()
            .health()
            .await
            .unwrap()
            .tailnet_user
            .is_none()
    );

    let interface = nix::ifaddrs::getifaddrs()
        .unwrap()
        .find_map(|interface| {
            let address = interface.address?.as_sockaddr_in()?.ip();
            (!address.is_loopback() && !address.is_unspecified()).then_some(address)
        })
        .expect("live peer test requires a non-loopback IPv4 interface");
    let remote = format!("http://{interface}:{}/v1/health", server.addr.port());
    let forged = server
        .http
        .get(&remote)
        .header("x-forwarded-for", "127.0.0.1")
        .header("tailscale-user-login", "forged@ts.net")
        .send()
        .await
        .unwrap();
    assert_error(forged, StatusCode::UNAUTHORIZED, "ERR_UNAUTHORIZED").await;
    let authorized = server
        .http
        .get(&remote)
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
    server.stop().await;
}

#[tokio::test]
async fn file_lifecycle_pagination_unicode_preconditions_and_range() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(config(&dir, 1000), None).await;
    let client = server.typed();
    assert!(
        client
            .list_files("", None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let folder = client.create_folder("cafe\u{301}/🎉").await.unwrap();
    assert_eq!(folder.path, "café/🎉");
    assert_eq!(folder.kind, FileType::Folder);
    client_error(
        client.create_folder("café/🎉").await.unwrap_err(),
        StatusCode::CONFLICT,
        "ERR_ALREADY_EXISTS",
    );

    let target = "café/🎉/data.txt";
    let put = server
        .http
        .put(server.url("/v1/files"))
        .bearer_auth(TOKEN)
        .query(&[("path", "cafe\u{301}/🎉/data.txt")])
        .header("x-content-sha256", sha256_hex(b"abcdef").to_uppercase())
        .body("abcdef")
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);
    let upload = put.json::<UploadResponse>().await.unwrap();
    assert_eq!(upload.item.path, target);
    assert_eq!(upload.item.mime.as_deref(), Some("text/plain"));
    assert_eq!(upload.item.version, 1);
    assert_eq!(upload.quota.used_bytes, 6);
    put_empty(&client, "café/🎉/empty").await;
    put_empty(&client, "café/🎉_keep").await;

    let page = client.list_files("café/🎉", None, Some(1)).await.unwrap();
    assert_eq!(page.items[0].path, target);
    let last = client
        .list_files("cafe\u{301}/🎉", page.next_cursor.as_deref(), Some(1))
        .await
        .unwrap();
    assert_eq!(last.items[0].name, "empty");
    assert!(last.next_cursor.is_none());

    let whole = client.download(target, None).await.unwrap();
    assert_eq!(whole.status(), StatusCode::OK);
    assert_eq!(
        whole.headers()["etag"],
        upload.item.etag.as_deref().unwrap()
    );
    assert_eq!(whole.headers()["x-content-sha256"], sha256_hex(b"abcdef"));
    assert_eq!(
        whole.headers()["content-disposition"],
        "attachment; filename*=UTF-8''data.txt"
    );
    assert_eq!(whole.bytes().await.unwrap(), b"abcdef"[..]);
    let part = client.download(target, Some("bytes=2-4")).await.unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(part.headers()["content-range"], "bytes 2-4/6");
    assert_eq!(part.headers()["content-length"], "3");
    assert_eq!(part.bytes().await.unwrap(), b"cde"[..]);

    let invalid = server
        .http
        .get(server.url("/v1/files/content"))
        .bearer_auth(TOKEN)
        .query(&[("path", target)])
        .header("range", "bytes=6-")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.headers()["content-range"], "bytes */6");
    assert_error(
        invalid,
        StatusCode::RANGE_NOT_SATISFIABLE,
        "ERR_RANGE_NOT_SATISFIABLE",
    )
    .await;
    let empty = client.download("café/🎉/empty", None).await.unwrap();
    assert_eq!(empty.headers()["content-length"], "0");
    assert!(empty.bytes().await.unwrap().is_empty());
    client_error(
        client
            .download("café/🎉/empty", Some("bytes=0-0"))
            .await
            .unwrap_err(),
        StatusCode::RANGE_NOT_SATISFIABLE,
        "ERR_RANGE_NOT_SATISFIABLE",
    );

    let replacement = Bytes::from_static(b"new");
    let stale = client
        .put_file(
            target,
            UploadBody::from_bytes(replacement.clone()),
            None,
            Some("\"obsolete\""),
        )
        .await
        .unwrap_err();
    client_error(stale, StatusCode::CONFLICT, "ERR_CONFLICT");
    let response = server
        .http
        .put(server.url("/v1/files"))
        .bearer_auth(TOKEN)
        .query(&[("path", target)])
        .header("if-match", upload.item.etag.unwrap())
        .header("x-content-sha256", sha256_hex(&replacement))
        .body(replacement)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated: UploadResponse = response.json().await.unwrap();
    assert_eq!(updated.item.version, 2);
    assert_eq!(updated.item.id, upload.item.id);
    assert_eq!(updated.quota.used_bytes, 3);
    assert_eq!(files(&dir.path().join("blobs")).len(), 3);
    client_error(
        client.delete_file("café/🎉", false).await.unwrap_err(),
        StatusCode::CONFLICT,
        "ERR_NOT_EMPTY",
    );
    assert_eq!(
        client
            .delete_file("café/🎉", true)
            .await
            .unwrap()
            .freed_bytes,
        3
    );
    assert_eq!(
        client.list_files("café", None, None).await.unwrap().items[0].name,
        "🎉_keep"
    );
    assert_eq!(files(&dir.path().join("blobs")).len(), 1);
    server.stop().await;
}

async fn put_empty(client: &TailDriveClient, name: &str) {
    put(client, name, b"").await;
}

#[tokio::test]
async fn validation_errors_have_envelopes_and_missing_blobs_fail_before_headers() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(config(&dir, 1000), None).await;
    let client = server.typed();
    for name in ["../escape", "/absolute", "a/../b", "a//b", "a\\b", "a\0b"] {
        client_error(
            client
                .put_file(name, UploadBody::from_bytes(Bytes::new()), None, None)
                .await
                .unwrap_err(),
            StatusCode::BAD_REQUEST,
            "ERR_INVALID_PATH",
        );
    }
    let chunked = reqwest::Body::wrap_stream(stream::iter([Ok::<_, std::io::Error>(
        Bytes::from_static(b"abc"),
    )]));
    let missing = server
        .http
        .put(server.url("/v1/files?path=length"))
        .bearer_auth(TOKEN)
        .header("x-content-sha256", sha256_hex(b"abc"))
        .body(chunked)
        .send()
        .await
        .unwrap();
    assert_error(missing, StatusCode::LENGTH_REQUIRED, "ERR_LENGTH_REQUIRED").await;

    let mismatch = server
        .http
        .put(server.url("/v1/files?path=mismatch"))
        .bearer_auth(TOKEN)
        .header("x-content-sha256", sha256_hex(b"xyz"))
        .body("abc")
        .send()
        .await
        .unwrap();
    let mismatch = assert_error(
        mismatch,
        StatusCode::UNPROCESSABLE_ENTITY,
        "ERR_CHECKSUM_MISMATCH",
    )
    .await;
    assert_eq!(mismatch.error.details["actual_sha256"], sha256_hex(b"abc"));
    assert!(files(&dir.path().join("tmp")).is_empty());
    assert!(files(&dir.path().join("blobs")).is_empty());
    assert!(
        client
            .list_files("", None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );

    let invalid = server
        .http
        .post(server.url("/v1/folders"))
        .bearer_auth(TOKEN)
        .header("content-type", "application/json")
        .body("{")
        .send()
        .await
        .unwrap();
    assert_error(invalid, StatusCode::BAD_REQUEST, "ERR_BAD_REQUEST").await;
    let missing = server
        .http
        .get(server.url("/v1/missing"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_error(missing, StatusCode::NOT_FOUND, "ERR_NOT_FOUND").await;
    let wrong_method = server
        .http
        .post(server.url("/v1/health"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_error(
        wrong_method,
        StatusCode::METHOD_NOT_ALLOWED,
        "ERR_METHOD_NOT_ALLOWED",
    )
    .await;

    put(&client, "orphan.txt", b"missing blob").await;
    std::fs::remove_file(&files(&dir.path().join("blobs"))[0]).unwrap();
    for range in [None, Some("bytes=1-3")] {
        client_error(
            client.download("orphan.txt", range).await.unwrap_err(),
            StatusCode::NOT_FOUND,
            "ERR_NOT_FOUND",
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn raw_tcp_disconnect_fin_and_rst_release_staging_and_quota() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(config(&dir, 1024), None).await;
    let tmp = dir.path().join("tmp");
    for reset in [false, true] {
        let mut socket = TcpStream::connect(server.addr).await.unwrap();
        let header = format!(
            "PUT /v1/files?path=interrupted HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 1024\r\nX-Content-SHA256: {}\r\n\r\n",
            server.addr,
            sha256_hex([b'x'; 1024]),
        );
        socket.write_all(header.as_bytes()).await.unwrap();
        socket.write_all(&[b'x'; 128]).await.unwrap();
        until(|| {
            files(&tmp)
                .iter()
                .any(|path| std::fs::metadata(path).is_ok_and(|entry| entry.len() == 128))
        })
        .await;
        if reset {
            socket.set_zero_linger().unwrap();
        } else {
            socket.shutdown().await.unwrap();
        }
        drop(socket);
        until(|| files(&tmp).is_empty()).await;
        assert!(files(&dir.path().join("blobs")).is_empty());
        let client = server.typed();
        let response = client
            .put_file(
                "full",
                UploadBody::from_bytes(Bytes::from_static(&[b'x'; 1024])),
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response.quota.used_bytes, 1024);
        client.delete_file("full", false).await.unwrap();
    }
    assert!(
        server
            .typed()
            .list_files("", None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    server.stop().await;
}

#[tokio::test]
async fn streaming_overwrites_recheck_preconditions_and_quota_at_commit() {
    for check_etag in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(config(&dir, 10), None).await;
        let client = server.typed();
        let original = put(&client, "target", b"original").await;
        let condition = original.item.etag.filter(|_| check_etag);
        let (send, receive) = mpsc::channel(2);
        let chunks = stream::unfold(receive, |mut receive| async {
            receive
                .recv()
                .await
                .map(|chunk| (Ok::<_, std::io::Error>(chunk), receive))
        });
        let pending_client = client.clone();
        let pending = tokio::spawn(async move {
            pending_client
                .put_file(
                    "target",
                    UploadBody::streaming(8, sha256_hex(b"modified"), chunks),
                    None,
                    condition.as_deref(),
                )
                .await
        });
        send.send(Bytes::from_static(b"modi")).await.unwrap();
        until(|| {
            files(&dir.path().join("tmp"))
                .iter()
                .any(|path| std::fs::metadata(path).is_ok_and(|entry| entry.len() == 4))
        })
        .await;

        put(&client, "target", b"x").await;
        put(&client, "fills-quota", b"123456789").await;
        send.send(Bytes::from_static(b"fied")).await.unwrap();
        drop(send);
        let error = pending.await.unwrap().unwrap_err();
        if check_etag {
            client_error(error, StatusCode::CONFLICT, "ERR_CONFLICT");
        } else {
            client_error(
                error,
                StatusCode::INSUFFICIENT_STORAGE,
                "ERR_QUOTA_EXCEEDED",
            );
        }
        assert_eq!(client.health().await.unwrap().quota.used_bytes, 10);
        let file = client
            .download("target", None)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(file, b"x"[..]);
        assert_eq!(files(&dir.path().join("blobs")).len(), 2);
        assert!(files(&dir.path().join("tmp")).is_empty());
        server.stop().await;
    }
}

struct DelayedStorage {
    local: LocalFsStorage,
    measured_free: AtomicU64,
    publishes: AtomicUsize,
    disk_calls: AtomicUsize,
    publish_entered: Notify,
    block_publish: bool,
    release_publish: Notify,
    deleted: AtomicUsize,
}

impl DelayedStorage {
    async fn new(dir: &TempDir, free: u64, block_publish: bool) -> Arc<Self> {
        Arc::new(Self {
            local: LocalFsStorage::new(dir.path()).await.unwrap(),
            measured_free: AtomicU64::new(free),
            publishes: AtomicUsize::new(0),
            disk_calls: AtomicUsize::new(0),
            publish_entered: Notify::new(),
            block_publish,
            release_publish: Notify::new(),
            deleted: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl StorageBackend for DelayedStorage {
    fn driver_name(&self) -> &'static str {
        self.local.driver_name()
    }

    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError> {
        self.publishes.fetch_add(1, Ordering::SeqCst);
        self.local.publish(key, staged).await?;
        self.publish_entered.notify_one();
        if self.block_publish {
            self.release_publish.notified().await;
        }
        Ok(())
    }

    async fn open(&self, key: &str, range: Range<u64>) -> Result<ByteStream, StorageError> {
        self.local.open(key, range).await
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.local.delete(key).await?;
        self.deleted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn disk_free_bytes(&self) -> Result<Option<u64>, StorageError> {
        self.disk_calls.fetch_add(1, Ordering::SeqCst);
        sleep(Duration::from_millis(25)).await;
        Ok(Some(self.measured_free.load(Ordering::SeqCst)))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_reservations_hold_across_a_delayed_capacity_probe() {
    let dir = tempfile::tempdir().unwrap();
    let storage = DelayedStorage::new(&dir, 10_000, false).await;
    let server = Server::start(config(&dir, 1000), Some(storage.clone())).await;
    let client = server.typed();
    let barrier = Barrier::new(8);
    let results = join_all((0..8).map(|index| {
        let client = &client;
        let barrier = &barrier;
        async move {
            barrier.wait().await;
            client
                .put_file(
                    &format!("upload-{index}"),
                    UploadBody::from_bytes(Bytes::from_static(&[1; 400])),
                    None,
                    None,
                )
                .await
        }
    }))
    .await;
    let mut accepted = 0;
    for result in results {
        match result {
            Ok(_) => accepted += 1,
            Err(error) => {
                assert!(!error.is_retryable());
                client_error(
                    error,
                    StatusCode::INSUFFICIENT_STORAGE,
                    "ERR_QUOTA_EXCEEDED",
                );
            }
        }
    }
    assert_eq!(accepted, 2);
    assert_eq!(storage.publishes.load(Ordering::SeqCst), 2);
    assert!(storage.disk_calls.load(Ordering::SeqCst) >= 4);
    assert_eq!(
        client.health().await.unwrap().quota.status,
        QuotaStatus::Warn
    );
    assert_eq!(client.health().await.unwrap().quota.used_bytes, 800);
    assert_eq!(files(&dir.path().join("blobs")).len(), 2);
    assert!(files(&dir.path().join("tmp")).is_empty());
    server.stop().await;
}

#[tokio::test]
async fn disk_floor_recheck_rolls_back_published_blob_after_disconnect() {
    let dir = tempfile::tempdir().unwrap();
    let storage = DelayedStorage::new(&dir, 2000, true).await;
    let mut config = config(&dir, 1000);
    config.free_floor_bytes = 1000;
    let server = Server::start(config, Some(storage.clone())).await;

    let request = |path: &str| {
        server
            .http
            .put(server.url("/v1/files"))
            .bearer_auth(TOKEN)
            .query(&[("path", path)])
            .header("x-content-sha256", sha256_hex(b"123"))
            .body("123")
    };
    let req = tokio::spawn(request("visible-error").send());
    timeout(Duration::from_secs(5), storage.publish_entered.notified())
        .await
        .unwrap();
    storage.measured_free.store(999, Ordering::SeqCst);
    storage.release_publish.notify_one();
    assert_error(
        req.await.unwrap().unwrap(),
        StatusCode::INSUFFICIENT_STORAGE,
        "ERR_DISK_FLOOR",
    )
    .await;
    assert_eq!(storage.deleted.load(Ordering::SeqCst), 1);

    let too_low = request("fails-admission").send().await.unwrap();
    assert_error(too_low, StatusCode::INSUFFICIENT_STORAGE, "ERR_DISK_FLOOR").await;
    assert_eq!(storage.publishes.load(Ordering::SeqCst), 1);

    storage.measured_free.store(2000, Ordering::SeqCst);
    let mut socket = TcpStream::connect(server.addr).await.unwrap();
    let request = format!(
        "PUT /v1/files?path=disconnected HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 3\r\nX-Content-SHA256: {}\r\n\r\n123",
        server.addr,
        sha256_hex(b"123"),
    );
    socket.write_all(request.as_bytes()).await.unwrap();
    timeout(Duration::from_secs(5), storage.publish_entered.notified())
        .await
        .unwrap();
    socket.set_zero_linger().unwrap();
    drop(socket);
    storage.measured_free.store(999, Ordering::SeqCst);
    storage.release_publish.notify_one();
    server.stop().await;
    assert_eq!(storage.deleted.load(Ordering::SeqCst), 2);
    assert!(files(&dir.path().join("blobs")).is_empty());
    assert!(files(&dir.path().join("tmp")).is_empty());
}

#[tokio::test]
async fn resumable_upload_persists_parts_enforces_quota_and_allows_retry() {
    let dir = tempfile::tempdir().unwrap();
    let total = 8 * 1024 * 1024 + 4;
    let server = Server::start(config(&dir, total + 1), None).await;
    let client = server.typed();
    let init = InitUploadRequest {
        path: "resumable/payload.bin".to_owned(),
        size_bytes: total,
        mime: None,
    };
    let upload = client.init_upload(&init).await.unwrap();
    assert_eq!(client.init_upload(&init).await.unwrap(), upload);
    let first = Bytes::from(vec![b'a'; upload.part_size as usize]);
    let last = Bytes::from_static(b"tail");
    let mut hash = Sha256Hasher::new();
    hash.update(&first);
    hash.update(&last);
    let final_hash = hash.finish();
    client_error(
        client
            .put_file(
                "no-room",
                UploadBody::from_bytes(Bytes::from_static(b"12")),
                None,
                None,
            )
            .await
            .unwrap_err(),
        StatusCode::INSUFFICIENT_STORAGE,
        "ERR_QUOTA_EXCEEDED",
    );
    client_error(
        client
            .put_part(&upload.upload_id, 0, UploadBody::from_bytes(last.clone()))
            .await
            .unwrap_err(),
        StatusCode::BAD_REQUEST,
        "ERR_BAD_REQUEST",
    );
    client_error(
        client
            .complete_upload(&upload.upload_id, &final_hash, None)
            .await
            .unwrap_err(),
        StatusCode::CONFLICT,
        "ERR_CONFLICT",
    );
    // Trailer arrives first using HTTP chunked framing; expected length is in session metadata.
    let chunked = reqwest::Body::wrap_stream(stream::iter([
        Ok::<_, std::io::Error>(last.slice(..2)),
        Ok(last.slice(2..)),
    ]));
    let part_url = server.url(&format!("/v1/uploads/{}/parts/2", upload.upload_id));
    let response = server
        .http
        .put(part_url)
        .bearer_auth(TOKEN)
        .header("x-content-sha256", sha256_hex(&last))
        .body(chunked)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(files(&dir.path().join("blobs")).len(), 1);
    server.stop().await;

    let server = Server::start(config(&dir, total + 1), None).await;
    let client = server.typed();
    assert_eq!(client.init_upload(&init).await.unwrap(), upload);
    let (a, b) = tokio::join!(
        client.put_part(&upload.upload_id, 1, UploadBody::from_bytes(first.clone())),
        client.put_part(&upload.upload_id, 2, UploadBody::from_bytes(last.clone())),
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(files(&dir.path().join("blobs")).len(), 2);
    client_error(
        client
            .complete_upload(&upload.upload_id, &sha256_hex(b"wrong"), None)
            .await
            .unwrap_err(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "ERR_CHECKSUM_MISMATCH",
    );
    client_error(
        client
            .complete_upload(&upload.upload_id, &final_hash, Some("\"no-match\""))
            .await
            .unwrap_err(),
        StatusCode::CONFLICT,
        "ERR_CONFLICT",
    );
    let response = client
        .complete_upload(&upload.upload_id, &final_hash, None)
        .await
        .unwrap();
    assert_eq!(response.item.sha256.as_deref(), Some(final_hash.as_str()));
    assert_eq!(response.quota.used_bytes, total);
    assert_eq!(files(&dir.path().join("blobs")).len(), 1);
    let bytes = client
        .download(&init.path, None)
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(bytes.len() as u64, total);
    assert_eq!(sha256_hex(&bytes), final_hash);

    let next = client
        .init_upload(&InitUploadRequest {
            path: "abort".to_owned(),
            size_bytes: 1,
            mime: None,
        })
        .await
        .unwrap();
    client
        .put_part(
            &next.upload_id,
            1,
            UploadBody::from_bytes(Bytes::from_static(b"z")),
        )
        .await
        .unwrap();
    client.abort_upload(&next.upload_id).await.unwrap();
    client_error(
        client.abort_upload(&next.upload_id).await.unwrap_err(),
        StatusCode::NOT_FOUND,
        "ERR_NOT_FOUND",
    );
    assert_eq!(files(&dir.path().join("blobs")).len(), 1);
    assert!(files(&dir.path().join("tmp")).is_empty());
    server.stop().await;
}

#[tokio::test]
async fn public_shares_are_read_only_expire_and_can_be_revoked() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(config(&dir, 200), None).await;
    let client = server.typed();
    put(&client, "docs/📄.txt", b"shared").await;
    let create = |ttl| CreateShareRequest {
        path: "docs/📄.txt".to_owned(),
        expires_in_seconds: ttl,
    };
    let revoked = client.create_share(&create(60)).await.unwrap();
    let expiring = client.create_share(&create(1)).await.unwrap();
    let expired_at = time::OffsetDateTime::parse(
        &expiring.expires_at,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert_eq!(client.list_shares().await.unwrap().items.len(), 2);

    let link = server.url(&revoked.url);
    let download = server
        .http
        .get(&link)
        .header("range", "bytes=-3")
        .send()
        .await
        .unwrap();
    assert_eq!(download.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(download.headers()["cache-control"], "private, no-store");
    assert_eq!(
        download.headers()["content-disposition"],
        "attachment; filename*=UTF-8''%F0%9F%93%84.txt"
    );
    assert_eq!(download.bytes().await.unwrap(), b"red"[..]);
    let attempted_write = server.http.put(&link).body("bad").send().await.unwrap();
    assert_error(
        attempted_write,
        StatusCode::METHOD_NOT_ALLOWED,
        "ERR_METHOD_NOT_ALLOWED",
    )
    .await;
    client.revoke_share(&revoked.id).await.unwrap();
    assert_error(
        server.http.get(&link).send().await.unwrap(),
        StatusCode::NOT_FOUND,
        "ERR_NOT_FOUND",
    )
    .await;
    assert_error(
        server
            .http
            .get(server.url("/v1/shares"))
            .send()
            .await
            .unwrap(),
        StatusCode::UNAUTHORIZED,
        "ERR_UNAUTHORIZED",
    )
    .await;

    let wait = expired_at - time::OffsetDateTime::now_utc();
    sleep(Duration::try_from(wait).unwrap_or_default() + Duration::from_millis(10)).await;
    assert_error(
        server
            .http
            .get(server.url(&expiring.url))
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "ERR_NOT_FOUND",
    )
    .await;
    assert!(client.list_shares().await.unwrap().items.is_empty());

    let deleted = client.create_share(&create(60)).await.unwrap();
    client.delete_file("docs/📄.txt", false).await.unwrap();
    put(&client, "docs/📄.txt", b"private file").await;
    assert_error(
        server
            .http
            .get(server.url(&deleted.url))
            .send()
            .await
            .unwrap(),
        StatusCode::NOT_FOUND,
        "ERR_NOT_FOUND",
    )
    .await;
    server.stop().await;
}

#[tokio::test]
async fn static_ui_and_opendal_backend_use_the_normal_server_entry_point() {
    let dir = tempfile::tempdir().unwrap();
    let web = tempfile::tempdir().unwrap();
    tokio::fs::write(web.path().join("index.html"), "<h1>TailDrive</h1>")
        .await
        .unwrap();
    let mut cfg = config(&dir, 1000);
    cfg.web_dir = Some(web.path().to_owned());
    let backing = opendal::Operator::new(opendal::services::Memory::default()).unwrap();
    let storage = Arc::new(S3Storage::from_operator(backing.clone()));
    let server = Server::start(cfg, Some(storage)).await;
    let index = server.http.get(server.url("/")).send().await.unwrap();
    assert_eq!(index.status(), StatusCode::OK);
    assert_eq!(index.text().await.unwrap(), "<h1>TailDrive</h1>");
    let error = server
        .http
        .get(server.url("/v1/unknown"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_error(error, StatusCode::NOT_FOUND, "ERR_NOT_FOUND").await;

    let client = server.typed();
    let health = client.health().await.unwrap();
    assert_eq!(health.storage_driver, "s3");
    assert_eq!(health.quota.disk_free_bytes, None);
    put(&client, "remote", b"object").await;
    let response = client.download("remote", Some("bytes=1-2")).await.unwrap();
    assert_eq!(response.bytes().await.unwrap(), b"bj"[..]);
    assert!(files(&dir.path().join("tmp")).is_empty());
    client.delete_file("remote", false).await.unwrap();
    assert!(
        backing
            .list_with("/")
            .recursive(true)
            .await
            .unwrap()
            .iter()
            .all(|entry| entry.metadata().is_dir())
    );
    server.stop().await;
}

#[tokio::test]
async fn updates_served_by_server_with_health_signal_and_fallback() {
    use std::collections::BTreeMap;
    use taildrive_core::update::{
        BuildInfo, ReleaseArtifact, ReleaseManifest, UpdateCheck, UpdaterClient,
        generate_signing_keypair, sign_manifest_bytes,
    };

    let (priv_b64, pub_b64) = generate_signing_keypair().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let releases_dir = dir.path().join("releases");
    tokio::fs::create_dir_all(releases_dir.join("artifacts"))
        .await
        .unwrap();

    let payload = b"taildrive-macos-bundle-zip-bytes";
    let payload_sha = sha256_hex(payload);
    tokio::fs::write(
        releases_dir.join("artifacts/TailDrive-macos-arm64-abcdef1.zip"),
        payload,
    )
    .await
    .unwrap();

    let mut artifacts = BTreeMap::new();
    artifacts.insert(
        "macos-app-arm64".to_owned(),
        ReleaseArtifact {
            url: "/v1/updates/artifacts/TailDrive-macos-arm64-abcdef1.zip".to_owned(),
            sha256: payload_sha,
            size: payload.len() as u64,
        },
    );
    let manifest = ReleaseManifest {
        schema_version: 1,
        channel: "main".to_owned(),
        version: "0.1.0-main.99+abcdef1".to_owned(),
        build_number: 99,
        commit_sha: "abcdef1234567890abcdef1234567890abcdef12".to_owned(),
        published_at: "2026-11-28T04:00:00Z".to_owned(),
        release_url: "https://github.com/ArturoKuang/taildrive/releases/tag/main-99".to_owned(),
        notes: "Local TailDrive server release".to_owned(),
        artifacts,
    };
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let sig = sign_manifest_bytes(&manifest_bytes, &priv_b64).unwrap();
    tokio::fs::write(releases_dir.join("manifest.json"), &manifest_bytes)
        .await
        .unwrap();
    tokio::fs::write(releases_dir.join("manifest.json.sig"), &sig)
        .await
        .unwrap();

    let mut cfg = config(&dir, 1000);
    cfg.releases_dir = Some(releases_dir);
    let server = Server::start(cfg, None).await;

    let health = server.typed().health().await.unwrap();
    assert!(health.server_version.is_some());
    assert_eq!(health.latest_build_number, Some(99));

    // Reject path traversal on /v1/updates/artifacts/{name}.
    let bad_name = server
        .http
        .get(server.url("/v1/updates/artifacts/..secret"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_error(bad_name, StatusCode::BAD_REQUEST, "ERR_BAD_REQUEST").await;

    // Server-first check resolves relative /v1/updates/artifacts/... and downloads with Bearer auth.
    let updater = UpdaterClient::new().unwrap();
    let current = BuildInfo {
        version: "0.1.0-main.98+0000000".to_owned(),
        channel: "main".to_owned(),
        build_number: 98,
        commit_sha: "0000000000000000000000000000000000000000".to_owned(),
        short_sha: "0000000".to_owned(),
        built_at: String::new(),
    };
    let base_url = format!("http://{}", server.addr);
    let check = updater
        .check_for_update_server_first(
            &current,
            "macos-app-arm64",
            Some(&base_url),
            Some(TOKEN),
            "http://127.0.0.1:9/unreachable/manifest.json",
            &pub_b64,
            false,
        )
        .await
        .unwrap();
    let release = match check {
        UpdateCheck::Available { release, .. } => release,
        UpdateCheck::UpToDate { .. } => panic!("expected server update"),
    };
    assert_eq!(
        release.download_url,
        format!("{base_url}/v1/updates/artifacts/TailDrive-macos-arm64-abcdef1.zip")
    );

    let dest = dir.path().join("downloaded.zip");
    updater
        .download_verified_artifact_with_auth(&release, &dest, Some(&base_url), Some(TOKEN), |_, _| {
            true
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), payload);

    // Fallback check: when primary server has no releases_dir (404), it falls back cleanly.
    let dir2 = tempfile::tempdir().unwrap();
    let empty_server = Server::start(config(&dir2, 1000), None).await;
    assert_eq!(
        empty_server.typed().health().await.unwrap().latest_build_number,
        None
    );

    let fallback_err = updater
        .check_for_update_server_first(
            &current,
            "macos-app-arm64",
            Some(&format!("http://{}", empty_server.addr)),
            Some(TOKEN),
            "http://127.0.0.1:9/fallback/manifest.json",
            &pub_b64,
            false,
        )
        .await;
    assert!(fallback_err.is_err());

    empty_server.stop().await;
    server.stop().await;
}

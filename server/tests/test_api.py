import unicodedata

import pytest
from conftest import AUTH, MakeClient, sha, staged_files, stored_blobs, upload
from fastapi.testclient import TestClient


def assert_error(response, status: int, code: str) -> dict:
    assert response.status_code == status, response.text
    error = response.json()["error"]
    assert error["code"] == code
    assert isinstance(error["message"], str) and error["message"]
    assert isinstance(error["retryable"], bool)
    assert isinstance(error["details"], dict)
    return error


# --- happy paths -----------------------------------------------------------


def test_health_reports_node_driver_and_quota(client: TestClient):
    body = client.get("/v1/health", headers=AUTH).json()

    assert body["status"] == "ok"
    assert body["storage_driver"] == "local-fs"
    assert body["quota"]["used_bytes"] == 0
    assert body["quota"]["limit_bytes"] == 10_000
    assert body["quota"]["remaining_bytes"] == 10_000
    assert body["quota"]["disk_free_bytes"] > 0
    assert body["quota"]["status"] == "ok"


def test_upload_then_download_round_trips_bytes_and_integrity_headers(client: TestClient):
    data = b"%PDF-1.7 fake spec"
    created = upload(client, "docs/spec.pdf", data)

    assert created.status_code == 201
    item = created.json()["item"]
    assert item["path"] == "docs/spec.pdf"
    assert item["name"] == "spec.pdf"
    assert item["type"] == "file"
    assert item["size_bytes"] == len(data)
    assert item["sha256"] == sha(data)
    assert item["etag"] == f'"{sha(data)}"'
    assert item["version"] == 1
    assert item["mime"] == "application/pdf"
    assert created.json()["quota"]["used_bytes"] == len(data)

    got = client.get("/v1/files/content", params={"path": "docs/spec.pdf"}, headers=AUTH)
    assert got.status_code == 200
    assert got.content == data
    assert got.headers["etag"] == f'"{sha(data)}"'
    assert got.headers["x-content-sha256"] == sha(data)
    assert got.headers["content-length"] == str(len(data))
    assert got.headers["content-type"] == "application/pdf"
    assert "spec.pdf" in got.headers["content-disposition"]


def test_overwrite_returns_200_and_bumps_version(client: TestClient, data_dir):
    upload(client, "a.txt", b"one")
    replaced = upload(client, "a.txt", b"two!")

    assert replaced.status_code == 200
    assert replaced.json()["item"]["version"] == 2
    assert replaced.json()["quota"]["used_bytes"] == 4
    assert client.get("/v1/files/content", params={"path": "a.txt"}, headers=AUTH).content == b"two!"
    assert len(stored_blobs(data_dir)) == 1  # the replaced blob is gone


def test_listing_shows_children_with_auto_created_parent_folders(client: TestClient):
    upload(client, "docs/q4/plan.md", b"# plan")

    root = client.get("/v1/files", headers=AUTH).json()
    docs = client.get("/v1/files", params={"path": "docs"}, headers=AUTH).json()

    assert root["path"] == ""
    assert [(i["name"], i["type"]) for i in root["items"]] == [("docs", "folder")]
    assert docs["path"] == "docs"
    assert [(i["path"], i["type"]) for i in docs["items"]] == [("docs/q4", "folder")]
    assert root["next_cursor"] is None


def test_listing_paginates_with_cursor(client: TestClient):
    for name in ["a", "b", "c", "d", "e"]:
        upload(client, f"{name}.txt", name.encode())

    seen, cursor = [], None
    while True:
        params = {"limit": 2, **({"cursor": cursor} if cursor else {})}
        page = client.get("/v1/files", params=params, headers=AUTH).json()
        seen += [i["name"] for i in page["items"]]
        cursor = page["next_cursor"]
        if cursor is None:
            break

    assert seen == ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]


def test_create_folder_then_duplicate_is_409(client: TestClient):
    created = client.post("/v1/folders", json={"path": "docs/q4"}, headers=AUTH)
    again = client.post("/v1/folders", json={"path": "docs/q4"}, headers=AUTH)

    assert created.status_code == 201
    assert created.json()["type"] == "folder"
    assert created.json()["path"] == "docs/q4"
    assert_error(again, 409, "ERR_ALREADY_EXISTS")


def test_range_request_returns_206_partial_content(client: TestClient):
    upload(client, "n.bin", b"0123456789")

    got = client.get("/v1/files/content", params={"path": "n.bin"}, headers={**AUTH, "Range": "bytes=2-5"})
    suffix = client.get("/v1/files/content", params={"path": "n.bin"}, headers={**AUTH, "Range": "bytes=-3"})
    bad = client.get("/v1/files/content", params={"path": "n.bin"}, headers={**AUTH, "Range": "bytes=50-"})

    assert got.status_code == 206
    assert got.content == b"2345"
    assert got.headers["content-range"] == "bytes 2-5/10"
    assert suffix.content == b"789"
    assert_error(bad, 416, "ERR_RANGE_NOT_SATISFIABLE")


def test_delete_file_frees_bytes_and_blob(client: TestClient, data_dir):
    upload(client, "docs/spec.pdf", b"x" * 300)

    deleted = client.delete("/v1/files", params={"path": "docs/spec.pdf"}, headers=AUTH)

    assert deleted.status_code == 200
    assert deleted.json()["deleted"] is True
    assert deleted.json()["freed_bytes"] == 300
    assert deleted.json()["quota"]["used_bytes"] == 0
    assert stored_blobs(data_dir) == []
    assert_error(client.get("/v1/files/content", params={"path": "docs/spec.pdf"}, headers=AUTH), 404, "ERR_NOT_FOUND")


def test_delete_non_empty_folder_needs_recursive(client: TestClient, data_dir):
    upload(client, "docs/a.txt", b"aa")
    upload(client, "docs/sub/b.txt", b"bbb")
    upload(client, "docs_other.txt", b"keep")  # shares the prefix, must survive

    refused = client.delete("/v1/files", params={"path": "docs"}, headers=AUTH)
    deleted = client.delete("/v1/files", params={"path": "docs", "recursive": "true"}, headers=AUTH)

    assert_error(refused, 409, "ERR_NOT_EMPTY")
    assert deleted.json()["freed_bytes"] == 5
    assert [i["path"] for i in client.get("/v1/files", headers=AUTH).json()["items"]] == ["docs_other.txt"]
    assert len(stored_blobs(data_dir)) == 1


def test_nfd_and_nfc_names_address_the_same_file(client: TestClient):
    nfd = unicodedata.normalize("NFD", "café.txt")
    nfc = unicodedata.normalize("NFC", "café.txt")
    upload(client, nfd, b"latte")

    assert client.get("/v1/files/content", params={"path": nfc}, headers=AUTH).content == b"latte"


# --- 400 path traversal ----------------------------------------------------

TRAVERSAL = ["../etc/passwd", "a/../../b", "/etc/passwd", "a\\..\\b", "a\x00b", "./a", "a//b", "a/", "..", "a/./b"]


@pytest.mark.parametrize("bad_path", TRAVERSAL)
def test_traversal_paths_rejected_on_every_endpoint(client: TestClient, data_dir, bad_path: str):
    params = {"path": bad_path}
    responses = [
        upload(client, bad_path, b"evil"),
        client.get("/v1/files", params=params, headers=AUTH),
        client.get("/v1/files/content", params=params, headers=AUTH),
        client.delete("/v1/files", params=params, headers=AUTH),
        client.post("/v1/folders", json=params, headers=AUTH),
    ]

    for response in responses:
        assert_error(response, 400, "ERR_INVALID_PATH")
    assert stored_blobs(data_dir) == []
    assert client.get("/v1/files", headers=AUTH).json()["items"] == []


def test_percent_encoded_traversal_is_decoded_then_rejected(client: TestClient):
    response = client.get("/v1/files/content?path=..%2F..%2Fetc%2Fpasswd", headers=AUTH)

    assert_error(response, 400, "ERR_INVALID_PATH")


def test_missing_path_is_rejected_for_file_operations(client: TestClient):
    assert_error(upload(client, "", b"x"), 400, "ERR_INVALID_PATH")
    assert_error(client.delete("/v1/files", headers=AUTH), 400, "ERR_INVALID_PATH")


# --- 401 auth --------------------------------------------------------------


@pytest.mark.parametrize(
    "headers",
    [
        {},
        {"Authorization": "Bearer wrong-token"},
        {"Authorization": "Bearer "},
        {"Authorization": "Basic dGVzdDp0ZXN0"},
        {"Tailscale-User-Login": "alice@ts.net"},  # forged: client is not loopback
    ],
    ids=["none", "wrong-token", "empty-token", "basic-scheme", "tailscale-header-off-loopback"],
)
def test_requests_without_valid_credentials_are_401(client: TestClient, data_dir, headers: dict):
    responses = [
        client.get("/v1/health", headers=headers),
        client.get("/v1/files", headers=headers),
        client.put("/v1/files", params={"path": "x.txt"}, content=b"x", headers={**headers, "X-Content-SHA256": sha(b"x")}),
        client.get("/v1/files/content", params={"path": "x.txt"}, headers=headers),
        client.delete("/v1/files", params={"path": "x.txt"}, headers=headers),
        client.post("/v1/folders", json={"path": "d"}, headers=headers),
    ]

    for response in responses:
        assert_error(response, 401, "ERR_UNAUTHORIZED")
    assert stored_blobs(data_dir) == []


def test_tailscale_identity_is_trusted_from_loopback(make_client: MakeClient):
    client = make_client(client_host="127.0.0.1")

    health = client.get("/v1/health", headers={"Tailscale-User-Login": "alice@ts.net"})

    assert health.status_code == 200
    assert health.json()["tailnet_user"] == "alice@ts.net"


def test_ipv6_loopback_is_trusted_too(make_client: MakeClient):
    client = make_client(client_host="::1")

    assert client.get("/v1/health", headers={"Tailscale-User-Login": "alice@ts.net"}).status_code == 200


def test_tailnet_peer_address_cannot_use_the_identity_header(make_client: MakeClient):
    client = make_client(client_host="100.64.0.7")

    assert_error(client.get("/v1/health", headers={"Tailscale-User-Login": "alice@ts.net"}), 401, "ERR_UNAUTHORIZED")


# --- 507 quota and disk floor ----------------------------------------------


def test_upload_over_quota_is_507_and_stores_nothing(make_client: MakeClient, data_dir):
    client = make_client(quota_bytes=1000)
    assert upload(client, "a.bin", b"a" * 600).status_code == 201

    over = upload(client, "b.bin", b"b" * 600)

    error = assert_error(over, 507, "ERR_QUOTA_EXCEEDED")
    assert error["retryable"] is False
    assert error["details"]["remaining_bytes"] == 400
    assert staged_files(data_dir) == []
    assert len(stored_blobs(data_dir)) == 1
    assert client.get("/v1/health", headers=AUTH).json()["quota"]["used_bytes"] == 600


def test_quota_counts_overwrites_by_size_delta(make_client: MakeClient):
    client = make_client(quota_bytes=1000)
    upload(client, "a.bin", b"a" * 600)

    grown = upload(client, "a.bin", b"a" * 900)

    assert grown.status_code == 200
    assert grown.json()["quota"]["used_bytes"] == 900


def test_quota_status_warns_at_80_percent_and_fills_at_95(make_client: MakeClient):
    client = make_client(quota_bytes=100)

    upload(client, "a", b"a" * 80)
    warn = client.get("/v1/health", headers=AUTH).json()["quota"]["status"]
    upload(client, "b", b"b" * 15)
    full = client.get("/v1/health", headers=AUTH).json()["quota"]["status"]

    assert (warn, full) == ("warn", "full")


def test_upload_that_would_breach_disk_floor_is_507(make_client: MakeClient, data_dir):
    client = make_client(free_floor_bytes=10**18)  # no real disk has an exabyte free

    response = upload(client, "a.bin", b"a" * 10)

    assert_error(response, 507, "ERR_DISK_FLOOR")
    assert staged_files(data_dir) == []
    assert stored_blobs(data_dir) == []
    assert client.get("/v1/health", headers=AUTH).json()["quota"]["status"] == "full"


# --- 422 checksum mismatch -------------------------------------------------


def test_checksum_mismatch_is_422_and_discards_upload(client: TestClient, data_dir):
    response = upload(client, "a.txt", b"actual bytes", digest=sha(b"different bytes"))

    error = assert_error(response, 422, "ERR_CHECKSUM_MISMATCH")
    assert error["details"]["actual_sha256"] == sha(b"actual bytes")
    assert staged_files(data_dir) == []
    assert stored_blobs(data_dir) == []
    assert client.get("/v1/files", headers=AUTH).json()["items"] == []
    assert client.get("/v1/health", headers=AUTH).json()["quota"]["used_bytes"] == 0


def test_checksum_mismatch_on_overwrite_keeps_previous_version(client: TestClient, data_dir):
    upload(client, "a.txt", b"v1")

    response = upload(client, "a.txt", b"v2-corrupt", digest=sha(b"v2"))

    assert_error(response, 422, "ERR_CHECKSUM_MISMATCH")
    assert client.get("/v1/files/content", params={"path": "a.txt"}, headers=AUTH).content == b"v1"
    assert len(stored_blobs(data_dir)) == 1


@pytest.mark.parametrize("digest", ["", "abc", "z" * 64])
def test_malformed_sha256_header_is_400(client: TestClient, digest: str):
    response = client.put("/v1/files", params={"path": "a.txt"}, content=b"x", headers={**AUTH, "X-Content-SHA256": digest})

    assert_error(response, 400, "ERR_BAD_REQUEST")


# --- 409 If-Match conflict -------------------------------------------------


def test_if_match_with_current_etag_succeeds(client: TestClient):
    etag = upload(client, "a.txt", b"v1").json()["item"]["etag"]

    response = upload(client, "a.txt", b"v2", **{"If-Match": etag})

    assert response.status_code == 200
    assert response.json()["item"]["version"] == 2


def test_if_match_with_stale_etag_is_409_and_keeps_current(client: TestClient, data_dir):
    stale = upload(client, "a.txt", b"v1").json()["item"]["etag"]
    upload(client, "a.txt", b"v2 from another device")

    response = upload(client, "a.txt", b"v3", **{"If-Match": stale})

    error = assert_error(response, 409, "ERR_CONFLICT")
    assert error["details"]["current_etag"] == f'"{sha(b"v2 from another device")}"'
    assert client.get("/v1/files/content", params={"path": "a.txt"}, headers=AUTH).content == b"v2 from another device"
    assert len(stored_blobs(data_dir)) == 1


def test_if_match_on_missing_file_is_409(client: TestClient):
    assert_error(upload(client, "new.txt", b"x", **{"If-Match": f'"{sha(b"x")}"'}), 409, "ERR_CONFLICT")


def test_put_over_a_folder_or_under_a_file_is_409(client: TestClient):
    upload(client, "docs/a.txt", b"a")

    assert_error(upload(client, "docs", b"x"), 409, "ERR_CONFLICT")
    assert_error(upload(client, "docs/a.txt/b.txt", b"x"), 409, "ERR_CONFLICT")


# --- envelope on framework errors ------------------------------------------


def test_unknown_route_and_bad_query_use_the_envelope(client: TestClient):
    assert_error(client.get("/v1/nope", headers=AUTH), 404, "ERR_NOT_FOUND")
    assert_error(client.get("/v1/files", params={"limit": 0}, headers=AUTH), 400, "ERR_BAD_REQUEST")
    assert_error(client.get("/v1/files", params={"cursor": "%%%"}, headers=AUTH), 400, "ERR_BAD_REQUEST")

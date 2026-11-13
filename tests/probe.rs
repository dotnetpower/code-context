use code_context::inline_budget::INLINE_BUDGET;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cctx_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cctx"))
}

fn spawn_test_http_server(parquet_bytes: Vec<u8>, png_bytes: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://127.0.0.1:{}", addr.port());

    let handle = std::thread::spawn(move || {
        for stream_res in listener.incoming().take(12) {
            let Ok(mut stream) = stream_res else {
                break;
            };
            let mut reader = BufReader::new(&stream);
            let mut req_line = String::new();
            if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                continue;
            }
            let mut range_hdr: Option<String> = None;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" || h == "\n" {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.trim().eq_ignore_ascii_case("range")
                {
                    range_hdr = Some(v.trim().to_string());
                }
            }
            let path = req_line.split_whitespace().nth(1).unwrap_or("/");
            if path.starts_with("/gg/Animalhosptl") {
                let body = json!({
                    "Animalhosptl": [
                        {"head": [{"list_total_count": 2747}, {"RESULT": {"CODE": "INFO-000", "MESSAGE": "정상 처리되었습니다."}}]},
                        {"row": [
                            {"SIGUN_NM": "용인시", "BIZPLC_NM": "1%애견동물병원", "REFINE_WGS84_LOGT": "127.112", "REFINE_WGS84_LAT": "37.285"},
                            {"SIGUN_NM": "수원시", "BIZPLC_NM": "튼튼동물병원", "REFINE_WGS84_LOGT": "127.028", "REFINE_WGS84_LAT": "37.263"}
                        ]}
                    ]
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            } else if path.starts_with("/gg/WalkCourse") {
                let body = json!({
                    "RESULT": {"CODE": "ERROR-310", "MESSAGE": "해당하는 서비스를 찾을 수 없습니다."}
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            } else if path.starts_with("/xml-err") {
                let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?><OpenAPI_ServiceResponse><cmmMsgHeader><errMsg>SERVICE ERROR</errMsg><returnAuthMsg>SERVICE_KEY_IS_NOT_REGISTERED_ERROR</returnAuthMsg><returnReasonCode>30</returnReasonCode></cmmMsgHeader></OpenAPI_ServiceResponse>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            } else if path.starts_with("/tile.png") {
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    png_bytes.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(&png_bytes);
            } else if path.starts_with("/stac/collection.json") {
                let body = json!({
                    "type": "FeatureCollection",
                    "features": [
                        {"id": "part-us-west", "bbox": [-122.5, 37.0, -121.5, 38.0]},
                        {"id": "part-kr-seoul", "bbox": [126.7, 37.4, 127.2, 37.7]},
                        {"id": "part-kr-jeju", "bbox": [126.1, 33.1, 126.9, 33.6]}
                    ]
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
            } else if path.starts_with("/remote.parquet") {
                let slice: &[u8] = if let Some(r) = range_hdr
                    && let Some(tail_s) = r.strip_prefix("bytes=-")
                    && let Ok(n) = tail_s.parse::<usize>()
                {
                    let start = parquet_bytes.len().saturating_sub(n);
                    &parquet_bytes[start..]
                } else {
                    &parquet_bytes
                };
                let hdr = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(hdr.as_bytes());
                let _ = stream.write_all(slice);
            } else if path.starts_with("/Stop") {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                break;
            } else {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nnot found");
            }
        }
    });
    (base, handle)
}

/// Encodes a minimal synthetic Apache Parquet file with Thrift Compact Protocol `FileMetaData`
/// containing 2 row groups (US-West and Korea) with `bbox.xmin/ymin/xmax/ymax` float64 stats.
fn build_synthetic_geoparquet() -> Vec<u8> {
    fn push_varint(buf: &mut Vec<u8>, mut n: u64) {
        loop {
            if n < 0x80 {
                buf.push(n as u8);
                break;
            }
            buf.push(((n & 0x7F) as u8) | 0x80);
            n >>= 7;
        }
    }
    fn push_zigzag_i32(buf: &mut Vec<u8>, v: i32) {
        let zz = ((v << 1) ^ (v >> 31)) as u32;
        push_varint(buf, zz as u64);
    }
    fn push_zigzag_i64(buf: &mut Vec<u8>, v: i64) {
        let zz = ((v << 1) ^ (v >> 63)) as u64;
        push_varint(buf, zz);
    }
    fn push_binary(buf: &mut Vec<u8>, bytes: &[u8]) {
        push_varint(buf, bytes.len() as u64);
        buf.extend_from_slice(bytes);
    }
    fn push_schema_elem(
        buf: &mut Vec<u8>,
        phys_type: Option<i32>,
        rep_type: Option<i32>,
        name: &str,
        num_children: Option<i32>,
        converted_type: Option<i32>,
    ) {
        let mut prev = 0i16;
        if let Some(pt) = phys_type {
            buf.push(((1 - prev) as u8) << 4 | 5);
            prev = 1;
            push_zigzag_i32(buf, pt);
        }
        if let Some(rt) = rep_type {
            buf.push(((3 - prev) as u8) << 4 | 5);
            prev = 3;
            push_zigzag_i32(buf, rt);
        }
        buf.push(((4 - prev) as u8) << 4 | 8);
        prev = 4;
        push_binary(buf, name.as_bytes());
        if let Some(nc) = num_children {
            buf.push(((5 - prev) as u8) << 4 | 5);
            prev = 5;
            push_zigzag_i32(buf, nc);
        }
        if let Some(ct) = converted_type {
            buf.push(((6 - prev) as u8) << 4 | 5);
            push_zigzag_i32(buf, ct);
        }
        buf.push(0); // STOP
    }
    fn push_double_col_chunk(buf: &mut Vec<u8>, path: &[&str], min_v: f64, max_v: f64) {
        // ColumnChunk field 3 = meta_data (STRUCT = 12)
        buf.push((3 << 4) | 12);
        // ColumnMetaData:
        // field 1 = type (DOUBLE = 5)
        buf.push((1 << 4) | 5);
        push_zigzag_i32(buf, 5);
        // field 3 (delta 2) = path_in_schema (LIST of BINARY)
        buf.push((2 << 4) | 9);
        buf.push(((path.len() as u8) << 4) | 8);
        for p in path {
            push_binary(buf, p.as_bytes());
        }
        // field 4 (delta 1) = codec (ZSTD = 6)
        buf.push((1 << 4) | 5);
        push_zigzag_i32(buf, 6);
        // field 6 (delta 2) = total_uncompressed_size
        buf.push((2 << 4) | 6);
        push_zigzag_i64(buf, 4096);
        // field 7 (delta 1) = total_compressed_size
        buf.push((1 << 4) | 6);
        push_zigzag_i64(buf, 1024);
        // field 12 (delta 5) = statistics (STRUCT = 12)
        buf.push((5 << 4) | 12);
        // Statistics field 5 = max_value, field 6 = min_value
        buf.push((5 << 4) | 8);
        push_binary(buf, &max_v.to_le_bytes());
        buf.push((1 << 4) | 8);
        push_binary(buf, &min_v.to_le_bytes());
        buf.push(0); // STOP Statistics
        buf.push(0); // STOP ColumnMetaData
        buf.push(0); // STOP ColumnChunk
    }
    fn push_row_group(buf: &mut Vec<u8>, rows: i64, bbox: [f64; 4]) {
        // field 1 = columns (LIST of 4 STRUCTs)
        buf.push((1 << 4) | 9);
        buf.push((4 << 4) | 12);
        push_double_col_chunk(buf, &["bbox", "xmin"], bbox[0], bbox[2]);
        push_double_col_chunk(buf, &["bbox", "ymin"], bbox[1], bbox[3]);
        push_double_col_chunk(buf, &["bbox", "xmax"], bbox[0], bbox[2]);
        push_double_col_chunk(buf, &["bbox", "ymax"], bbox[1], bbox[3]);
        // field 2 (delta 1) = total_byte_size
        buf.push((1 << 4) | 6);
        push_zigzag_i64(buf, 16384);
        // field 3 (delta 1) = num_rows
        buf.push((1 << 4) | 6);
        push_zigzag_i64(buf, rows);
        buf.push(0); // STOP RowGroup
    }

    let mut footer = Vec::new();
    // FileMetaData field 1 = version (2)
    footer.push((1 << 4) | 5);
    push_zigzag_i32(&mut footer, 2);
    // field 2 (delta 1) = schema (LIST of 7 SchemaElements: root, id, bbox group + 4 coords)
    footer.push((1 << 4) | 9);
    footer.push((7 << 4) | 12);
    push_schema_elem(&mut footer, None, None, "schema", Some(2), None);
    push_schema_elem(&mut footer, Some(6), Some(0), "id", None, Some(0));
    push_schema_elem(&mut footer, None, Some(1), "bbox", Some(4), None);
    push_schema_elem(&mut footer, Some(5), Some(0), "xmin", None, None);
    push_schema_elem(&mut footer, Some(5), Some(0), "ymin", None, None);
    push_schema_elem(&mut footer, Some(5), Some(0), "xmax", None, None);
    push_schema_elem(&mut footer, Some(5), Some(0), "ymax", None, None);
    // field 3 (delta 1) = num_rows (1500)
    footer.push((1 << 4) | 6);
    push_zigzag_i64(&mut footer, 1500);
    // field 4 (delta 1) = row_groups (LIST of 2 STRUCTs)
    footer.push((1 << 4) | 9);
    footer.push((2 << 4) | 12);
    push_row_group(&mut footer, 1000, [-122.5, 37.0, -121.0, 38.0]);
    push_row_group(&mut footer, 500, [126.5, 37.2, 127.3, 37.8]);
    // field 5 (delta 1) = key_value_metadata (1 entry: "geo")
    footer.push((1 << 4) | 9);
    footer.push((1 << 4) | 12);
    footer.push((1 << 4) | 8);
    push_binary(&mut footer, b"geo");
    footer.push((1 << 4) | 8);
    push_binary(
        &mut footer,
        br#"{"version":"1.0.0","primary_column":"geometry"}"#,
    );
    footer.push(0); // STOP KeyValue
    footer.push(0); // STOP FileMetaData

    let mut file = Vec::new();
    file.extend_from_slice(b"PAR1");
    file.extend_from_slice(b"dummy-column-pages");
    file.extend_from_slice(&footer);
    file.extend_from_slice(&(footer.len() as u32).to_le_bytes());
    file.extend_from_slice(b"PAR1");
    file
}

#[test]
fn pctx_multicall_http_matrix_stac_bbox_xml_tile_and_secret_redaction() {
    let dir = tempfile::tempdir().unwrap();
    let pctx_link = dir.path().join("pctx");
    #[cfg(unix)]
    std::os::unix::fs::symlink(cctx_bin(), &pctx_link).unwrap();
    #[cfg(windows)]
    std::fs::copy(cctx_bin(), &pctx_link).unwrap();

    let icon_bytes =
        std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("vscode-extension/resources/icon.png"))
            .unwrap();
    let parquet_bytes = build_synthetic_geoparquet();
    let (base, handle) = spawn_test_http_server(parquet_bytes.clone(), icon_bytes);

    // 1. Matrix HTTP probe via `pctx` symlink + secret env injection & redaction
    let out = Command::new(&pctx_link)
        .env("TEST_API_SECRET", "SuperSecretKey987654")
        .args([
            "http",
            &format!("{base}/gg/{{svc}}?Type=json"),
            "--var",
            "svc=Animalhosptl,WalkCourse",
            "--env-query",
            "serviceKey=TEST_API_SECRET",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let raw = String::from_utf8(out.stdout).unwrap();
    assert!(raw.len() <= INLINE_BUDGET);
    assert!(
        !raw.contains("SuperSecretKey987654"),
        "secret leaked in output: {raw}"
    );
    assert!(raw.contains("<REDACTED>"));
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["endpoints"], 2);
    assert_eq!(parsed["results"][0]["json"]["detected_total_count"], 2747);
    assert!(
        parsed["results"][0]["json"]["api_status"]
            .as_str()
            .unwrap()
            .contains("INFO-000")
    );
    assert!(
        parsed["results"][1]["json"]["api_status"]
            .as_str()
            .unwrap()
            .contains("ERROR-310")
    );

    // 2. Overture STAC bbox spatial intersection
    let stac_out = Command::new(&pctx_link)
        .args([
            "json",
            &format!("{base}/stac/collection.json"),
            "--bbox",
            "125.5,33.1,131.0,38.6",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(stac_out.status.success());
    let stac_val: Value = serde_json::from_slice(&stac_out.stdout).unwrap();
    assert_eq!(stac_val["item_count"], 3);
    assert_eq!(stac_val["matched_count"], 2);
    assert_eq!(
        stac_val["matched_ids"],
        json!(["part-kr-seoul", "part-kr-jeju"])
    );

    // 3. XML error & PNG tile batch
    let mixed_out = Command::new(cctx_bin())
        .args([
            "probe",
            "http",
            &format!("{base}/xml-err"),
            &format!("{base}/tile.png"),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(mixed_out.status.success());
    let mixed_val: Value = serde_json::from_slice(&mixed_out.stdout).unwrap();
    assert_eq!(mixed_val["results"][0]["kind"], "xml");
    assert_eq!(
        mixed_val["results"][0]["xml"]["status_fields"]["returnAuthMsg"],
        "SERVICE_KEY_IS_NOT_REGISTERED_ERROR"
    );
    assert_eq!(mixed_val["results"][1]["kind"], "image");
    assert_eq!(mixed_val["results"][1]["image"]["width"], 256);
    assert_eq!(mixed_val["results"][1]["image"]["height"], 256);

    // 4. Remote GeoParquet via HTTP Range + bbox row-group pruning
    let pq_out = Command::new(&pctx_link)
        .args([
            "table",
            &format!("{base}/remote.parquet"),
            "--bbox",
            "125.5,33.1,131.0,38.6",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        pq_out.status.success(),
        "{}",
        String::from_utf8_lossy(&pq_out.stderr)
    );
    let pq_val: Value = serde_json::from_slice(&pq_out.stdout).unwrap();
    assert_eq!(pq_val["num_rows"], 1500);
    assert_eq!(pq_val["row_groups"], 2);
    assert_eq!(pq_val["matched_row_groups"], 1);
    assert_eq!(pq_val["matched_rows_upper_bound"], 500);

    // Stop server cleanly
    let _ = Command::new(&pctx_link)
        .args(["http", &format!("{base}/Stop")])
        .output();
    let _ = handle.join();
}

#[test]
fn json_jsonl_offload_and_inline_budget_enforcement() {
    let dir = tempfile::tempdir().unwrap();

    // Offloaded tool output with trailing CCTX_TRACE footer and giant payload
    let offload_path = dir.path().join("1793-copilot-tool-output-sample.txt");
    let mut items = Vec::new();
    for i in 0..200 {
        items.push(json!({
            "id": format!("item-{i}"),
            "category": if i % 3 == 0 { "park" } else { "cafe" },
            "score": i,
            "api_key": "should-not-appear",
            "blob": "가나다라마바사".repeat(80),
        }));
    }
    let body = format!(
        "{}\nCCTX_TRACE {{\"trace_id\":\"abc\",\"total_ms\":2}}\n",
        json!({ "results": items })
    );
    std::fs::write(&offload_path, body).unwrap();

    let out = Command::new(cctx_bin())
        .args([
            "probe",
            "json",
            offload_path.to_str().unwrap(),
            "--where",
            "score>=150",
            "--count-by",
            "category",
            "--samples",
            "20",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stdout.len() <= INLINE_BUDGET);
    let s = String::from_utf8(out.stdout).unwrap();
    assert!(!s.contains("should-not-appear"));
    let val: Value = serde_json::from_str(&s).unwrap();
    assert_eq!(val["item_count"], 200);
    assert_eq!(val["matched_count"], 50);

    // Streaming JSONL
    let jsonl_path = dir.path().join("events.jsonl");
    std::fs::write(
        &jsonl_path,
        "{\"id\":\"a\",\"status\":\"passed\"}\n{\"id\":\"b\",\"status\":\"wrong\"}\n{\"id\":\"c\",\"status\":\"passed\"}\n",
    )
    .unwrap();
    let jl_out = Command::new(cctx_bin())
        .args([
            "probe",
            "json",
            jsonl_path.to_str().unwrap(),
            "--count-by",
            "status",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(jl_out.status.success());
    let jl_val: Value = serde_json::from_slice(&jl_out.stdout).unwrap();
    assert_eq!(jl_val["item_count"], 3);
    assert_eq!(jl_val["counts_by"][0], json!(["passed", 2]));
}

#[test]
fn table_local_parquet_csv_and_readonly_sqlite_guards() {
    let dir = tempfile::tempdir().unwrap();

    // Local Parquet
    let pq_path = dir.path().join("local.parquet");
    std::fs::write(&pq_path, build_synthetic_geoparquet()).unwrap();
    let pq_out = Command::new(cctx_bin())
        .args(["probe", "table", pq_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(pq_out.status.success());
    let pq_val: Value = serde_json::from_slice(&pq_out.stdout).unwrap();
    assert_eq!(pq_val["format"], "parquet");
    assert_eq!(pq_val["num_rows"], 1500);

    // CSV with quoted fields + sensitive column redaction
    let csv_path = dir.path().join("places.csv");
    std::fs::write(
        &csv_path,
        "name,city,secret_token\n\"Gaon, Lake\",Yongin,tok_live_12345\nCentral Park,Paju,tok_live_67890\n",
    )
    .unwrap();
    let csv_out = Command::new(cctx_bin())
        .args([
            "probe",
            "table",
            csv_path.to_str().unwrap(),
            "--count-by",
            "city",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(csv_out.status.success());
    let csv_text = String::from_utf8(csv_out.stdout).unwrap();
    assert!(!csv_text.contains("tok_live_12345"));
    let csv_val: Value = serde_json::from_str(&csv_text).unwrap();
    assert_eq!(csv_val["rows"], 2);

    // SQLite read-only guard tests
    assert!(code_context::probe::table_probe::validate_readonly_sql("SELECT count(*) FROM t").is_ok());
    assert!(
        code_context::probe::table_probe::validate_readonly_sql(
            "WITH cte AS (SELECT 1) SELECT * FROM cte;"
        )
        .is_ok()
    );
    for forbidden in [
        "DROP TABLE users",
        "SELECT 1; DELETE FROM users",
        "PRAGMA writable_schema = 1",
        "ATTACH DATABASE 'x.db' AS x",
        "WITH x AS (SELECT 1) UPDATE users SET a = 1",
    ] {
        assert!(
            code_context::probe::table_probe::validate_readonly_sql(forbidden).is_err(),
            "failed to block `{forbidden}`"
        );
    }

    // If `sqlite3` CLI is available on host, test end-to-end DB creation, unescaped object samples, & secret column redaction
    let db_path = dir.path().join("places.sqlite");
    if Command::new("sqlite3")
        .arg(&db_path)
        .arg("CREATE TABLE parks (id INTEGER PRIMARY KEY, name TEXT, api_key TEXT); INSERT INTO parks VALUES (1, 'Unjeong', 'sk_sqlite_secret_1'), (2, 'Gwanggyo', 'sk_sqlite_secret_2');")
        .status()
        .is_ok_and(|s| s.success())
    {
        let db_out = Command::new(cctx_bin())
            .args(["probe", "table", db_path.to_str().unwrap(), "--json"])
            .output()
            .unwrap();
        assert!(db_out.status.success());
        let db_text = String::from_utf8(db_out.stdout).unwrap();
        assert!(!db_text.contains("sk_sqlite_secret_1"), "SQLite sample leaked api_key: {db_text}");
        let db_val: Value = serde_json::from_str(&db_text).unwrap();
        assert_eq!(db_val["tables"][0]["name"], "parks");
        assert_eq!(db_val["tables"][0]["row_count"], 2);
        assert_eq!(db_val["tables"][0]["samples"][0]["api_key"], "<REDACTED>");
    }
}

#[test]
fn image_png_exposure_dimension_expectation_and_svg_audit() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let icon = manifest_dir.join("vscode-extension/resources/icon.png");
    let banner = manifest_dir.join("vscode-extension/resources/banner.png");
    let svg = manifest_dir.join("vscode-extension/resources/icon.svg");

    let out = Command::new(cctx_bin())
        .args([
            "probe",
            "image",
            icon.to_str().unwrap(),
            "--expect",
            "256x256",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let val: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(val["flagged"], 0);
    assert_eq!(val["items"][0]["width"], 256);
    assert_eq!(val["items"][0]["height"], 256);
    assert!(val["items"][0]["exposure"]["mean_lum"].as_f64().unwrap() > 0.0);

    // Mismatched dimension expectation flags cleanly
    let mismatch = Command::new(cctx_bin())
        .args([
            "probe",
            "image",
            banner.to_str().unwrap(),
            "--expect",
            "256x256",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(mismatch.status.success());
    let mval: Value = serde_json::from_slice(&mismatch.stdout).unwrap();
    assert_eq!(mval["flagged"], 1);
    assert!(
        mval["items"][0]["flags"][0]
            .as_str()
            .unwrap()
            .starts_with("DIMENSION_MISMATCH")
    );

    // SVG inspection
    let svg_out = Command::new(cctx_bin())
        .args(["probe", "image", svg.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(svg_out.status.success());
    let sval: Value = serde_json::from_slice(&svg_out.stdout).unwrap();
    assert_eq!(sval["items"][0]["format"], "SVG");
    assert!(sval["items"][0]["svg_elements"].as_u64().unwrap() > 0);
}

#[test]
fn round1_security_and_edge_case_regressions() {
    let dir = tempfile::tempdir().unwrap();

    // R1-C05: URL userinfo credentials must be scrubbed
    let redacted = code_context::probe::redact_url(
        "https://admin:TopSecretPass123@api.example.com/v1/items?serviceKey=RawKey999",
        &[],
    );
    assert!(!redacted.contains("TopSecretPass123"));
    assert!(!redacted.contains("RawKey999"));
    assert!(redacted.starts_with("https://<REDACTED>@api.example.com/v1/items?serviceKey=<REDACTED>"));

    // R1-C02: sqlite3 fileio functions (`writefile`, `readfile`, `edit`, `fts3_tokenizer`) must be rejected
    for dangerous in [
        "SELECT writefile('/tmp/x', 'bad')",
        "SELECT readfile('/etc/passwd')",
        "SELECT edit('a', 'vim')",
        "SELECT fts3_tokenizer('simple')",
    ] {
        assert!(
            code_context::probe::table_probe::validate_readonly_sql(dangerous).is_err(),
            "failed to reject `{dangerous}`"
        );
    }

    // R1-C04 + R1-C06 + R1-C07 + R1-C11:
    // - Negative coordinates in `--bbox -122.6,37.0,-121.0,38.5`
    // - Host offload preamble containing `[500 chars]` bracket noise before JSON
    // - Overture object `bbox: {xmin,ymin,xmax,ymax}` matched & bare screen `x,y` NOT falsely matched
    // - `--count-by api_key` redacts secret bucket values
    let wrapped_json = dir.path().join("offload-preview.txt");
    let payload = json!({
        "features": [
            {
                "id": "sf-overture-obj",
                "bbox": {"xmin": -122.45, "ymin": 37.75, "xmax": -122.39, "ymax": 37.80},
                "api_key": "sk-secret-alpha"
            },
            {
                "id": "ui-pixel-widget",
                "x": -122.42,
                "y": 37.77,
                "api_key": "sk-secret-beta"
            }
        ]
    });
    std::fs::write(
        &wrapped_json,
        format!("Output too large [first 500 chars]:\n{payload}\nCCTX_TRACE {{\"total_ms\":1}}\n"),
    )
    .unwrap();

    let out = Command::new(cctx_bin())
        .args([
            "probe",
            "json",
            wrapped_json.to_str().unwrap(),
            "--bbox",
            "-122.6,37.0,-121.0,38.5",
            "--count-by",
            "api_key",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("sk-secret-alpha"));
    assert!(!text.contains("sk-secret-beta"));
    let val: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(val["matched_count"], 1);
    assert_eq!(val["matched_ids"], json!(["sf-overture-obj"]));
    assert_eq!(val["counts_by"][0], json!(["<REDACTED>", 1]));

    // R1-C01: Cross-port redirect must strip Authorization / X-Api-Key headers and downgrade 302 POST -> GET
    let target_srv = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_port = target_srv.local_addr().unwrap().port();
    let redirect_srv = TcpListener::bind("127.0.0.1:0").unwrap();
    let redirect_port = redirect_srv.local_addr().unwrap().port();

    let t_handle = std::thread::spawn(move || {
        let (mut stream, _) = target_srv.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        let mut raw_req = String::new();
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                break;
            }
            raw_req.push_str(&l);
        }
        let _ = stream.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
        );
        raw_req
    });
    let r_handle = std::thread::spawn(move || {
        let (mut stream, _) = redirect_srv.accept().unwrap();
        let mut reader = BufReader::new(&stream);
        loop {
            let mut l = String::new();
            if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = stream.write_all(resp.as_bytes());
    });

    let redir_out = Command::new(cctx_bin())
        .args([
            "probe",
            "http",
            &format!("http://127.0.0.1:{redirect_port}/start"),
            "--method",
            "POST",
            "--body",
            "secret-payload",
            "-H",
            "Authorization: Bearer TopSecretRedirectToken",
            "-H",
            "X-Api-Key: CustomSecretHeader",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(redir_out.status.success());
    let _ = r_handle.join();
    let landed_req = t_handle.join().unwrap();
    assert!(landed_req.starts_with("GET /landed HTTP/1.1"));
    assert!(!landed_req.to_ascii_lowercase().contains("authorization:"));
    assert!(!landed_req.to_ascii_lowercase().contains("x-api-key:"));
}

#[test]
fn round2_hardening_regressions() {
    let dir = tempfile::tempdir().unwrap();

    // R2-C05: Lowercase percent-encoded (`%2b`, `%3d`) & JSON-escaped secrets scrubbed
    let secret = "Key+With\"Quote==".to_string();
    let scrubbed = code_context::probe::scrub_secrets(
        r#"err="Key+With\"Quote==" lower=Key%2bWith%22Quote%3d%3d upper=Key%2BWith%22Quote%3D%3D"#,
        &[secret],
    );
    assert!(!scrubbed.contains("Key+With"));
    assert!(!scrubbed.contains("Key%2b"));
    assert!(!scrubbed.contains("Key%2B"));
    assert_eq!(
        scrubbed.matches("<REDACTED>").count(),
        3,
        "unexpected scrubbed output: {scrubbed}"
    );

    // R2-C04: Parquet global min/max across BOTH row groups (-122.5 .. 127.3)
    let pq_path = dir.path().join("global_bounds.parquet");
    std::fs::write(&pq_path, build_synthetic_geoparquet()).unwrap();
    let pq_out = Command::new(cctx_bin())
        .args(["probe", "table", pq_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(pq_out.status.success());
    let pq_val: Value = serde_json::from_slice(&pq_out.stdout).unwrap();
    let cols = pq_val["columns"].as_array().unwrap();
    let xmin_col = cols.iter().find(|c| c["name"] == "bbox.xmin").unwrap();
    assert_eq!(xmin_col["min_sample"], "-122.500000");
    assert_eq!(xmin_col["max_sample"], "127.300000");

    // R2-C06: RFC-4180 multiline quoted CSV field containing embedded newline
    let csv_path = dir.path().join("multiline.csv");
    std::fs::write(
        &csv_path,
        "id,note\n1,\"First line\nSecond line\"\n2,\"Single line\"\n",
    )
    .unwrap();
    let csv_out = Command::new(cctx_bin())
        .args(["probe", "table", csv_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(csv_out.status.success());
    let csv_val: Value = serde_json::from_slice(&csv_out.stdout).unwrap();
    assert_eq!(csv_val["rows"], 2);
    assert!(
        csv_val["samples"][0]["note"]
            .as_str()
            .unwrap()
            .contains("First line\nSecond line")
    );

    // R2-C01 + R2-C09: `--header-only` PNG inspection & HTML5 lowercase `viewbox` SVG
    let icon = Path::new(env!("CARGO_MANIFEST_DIR")).join("vscode-extension/resources/icon.png");
    let expected_len = std::fs::metadata(&icon).unwrap().len();
    let hdr_out = Command::new(cctx_bin())
        .args([
            "probe",
            "image",
            icon.to_str().unwrap(),
            "--header-only",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(hdr_out.status.success());
    let hdr_val: Value = serde_json::from_slice(&hdr_out.stdout).unwrap();
    assert_eq!(hdr_val["items"][0]["bytes"], expected_len);
    assert!(hdr_val["items"][0].get("exposure").is_none());

    let svg_path = dir.path().join("html5.svg");
    std::fs::write(
        &svg_path,
        "<svg viewbox=\"0 0 320 180\"><rect width=\"320\" height=\"180\"/></svg>",
    )
    .unwrap();
    let svg_out = Command::new(cctx_bin())
        .args(["probe", "image", svg_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(svg_out.status.success());
    let svg_val: Value = serde_json::from_slice(&svg_out.stdout).unwrap();
    assert_eq!(svg_val["flagged"], 0);
    assert_eq!(svg_val["items"][0]["width"], 320);
    assert_eq!(svg_val["items"][0]["height"], 180);

    // R3-C02: RFC 3986 relative redirect resolution against current directory path
    assert_eq!(
        code_context::probe::http_probe::resolve_redirect_url(
            "https",
            "api.example.com",
            "/v1/parks/list?page=1",
            "detail?id=9",
        ),
        "https://api.example.com/v1/parks/detail?id=9"
    );

    // R3-C05: GeoJSON GeometryCollection bbox extraction
    let gc_path = dir.path().join("gc.geojson");
    std::fs::write(
        &gc_path,
        json!({
            "type": "FeatureCollection",
            "features": [{
                "id": "seoul-gc",
                "geometry": {
                    "type": "GeometryCollection",
                    "geometries": [
                        {"type": "Point", "coordinates": [126.978, 37.566]}
                    ]
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let gc_out = Command::new(cctx_bin())
        .args([
            "probe",
            "json",
            gc_path.to_str().unwrap(),
            "--bbox",
            "126.5,37.0,127.5,38.0",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(gc_out.status.success());
    let gc_val: Value = serde_json::from_slice(&gc_out.stdout).unwrap();
    assert_eq!(gc_val["matched_ids"], json!(["seoul-gc"]));

    // R3-C08: Trailing blank CSV header cell normalized to `_col_N`
    let blank_hdr_csv = dir.path().join("blank_hdr.csv");
    std::fs::write(&blank_hdr_csv, "name,,city\nCentral,x,Seoul\n").unwrap();
    let bh_out = Command::new(cctx_bin())
        .args(["probe", "table", blank_hdr_csv.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(bh_out.status.success());
    let bh_val: Value = serde_json::from_slice(&bh_out.stdout).unwrap();
    assert_eq!(bh_val["columns"][1]["name"], "_col_1");
}

#[test]
fn batch_and_log_spool_bm25_rrf_redaction_and_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let cctx_home = dir.path().join("cctx-state");
    std::fs::create_dir_all(&cctx_home).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cctx_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // Build a >120 KB unstructured log file with ANSI escapes, secrets, code fence, and 1 FATAL root cause
    let big_log = dir.path().join("service.log");
    let mut log_content = String::with_capacity(140_000);
    for i in 1..=1_600 {
        if i == 850 {
            log_content.push_str(
                "\x1b[31m2026-11-14T01:00:00Z FATAL [raft::wal] disk fsync EIO on segment 42 Authorization: Bearer sk-live-secret9876543210 API_KEY=my_top_secret\x1b[0m\n",
            );
            log_content.push_str("  --> src/storage/wal.rs:219:13\n");
            log_content.push_str("  = note: journal checkpoint aborted to preserve durability\n");
        } else if i == 1_200 {
            log_content.push_str("# Recovery Snippet\n```rust\nfn replay_wal(seg: u64) -> bool {\n    seg == 42\n}\n```\n");
        } else {
            log_content.push_str(&format!(
                "2026-11-14T00:00:{:02}Z INFO [http::worker] processed healthcheck request seq={i} status=200 latency_ms=1\n",
                i % 60
            ));
        }
    }
    assert!(log_content.len() > 120_000);
    std::fs::write(&big_log, &log_content).unwrap();

    // 1. Run `pctx batch` (via symlink) with two commands and two BM25+Trigram RRF queries in 1 call
    let pctx_link = dir.path().join(if cfg!(windows) { "pctx.exe" } else { "pctx" });
    #[cfg(unix)]
    std::os::unix::fs::symlink(cctx_bin(), &pctx_link).unwrap();
    #[cfg(windows)]
    std::fs::copy(cctx_bin(), &pctx_link).unwrap();

    let cat_cmd = if cfg!(windows) {
        format!("type \"{}\"", big_log.display())
    } else {
        format!("cat '{}'", big_log.display())
    };
    let batch_out = Command::new(&pctx_link)
        .env("CCTX_HOME", &cctx_home)
        .args([
            "batch",
            "-c",
            &cat_cmd,
            "-c",
            "echo \"deploy_step=completed status=ok\"",
            "-q",
            "raft wal fsync EIO",
            "-q",
            "replay_wal",
        ])
        .output()
        .unwrap();
    assert!(
        batch_out.status.success(),
        "{}",
        String::from_utf8_lossy(&batch_out.stderr)
    );
    assert!(
        batch_out.stdout.len() <= INLINE_BUDGET,
        "exceeded inline budget: {}",
        batch_out.stdout.len()
    );
    let batch_text = String::from_utf8(batch_out.stdout).unwrap();
    assert!(batch_text.contains("spool=@s1"), "{batch_text}");
    assert!(batch_text.contains("anomalies(fatal=1"));
    assert!(batch_text.contains("src/storage/wal.rs:219:13"));
    assert!(batch_text.contains("fn replay_wal(seg: u64)"));
    assert!(!batch_text.contains("sk-live-secret9876543210"));
    assert!(!batch_text.contains("my_top_secret"));
    assert!(batch_text.contains("<REDACTED>"));

    // 2. Query cached spool `@s1` and `@latest` via `pctx log` with ZERO command re-execution
    let spool_out = Command::new(&pctx_link)
        .env("CCTX_HOME", &cctx_home)
        .args(["log", "@latest", "-q", "deploy_step completed", "--json"])
        .output()
        .unwrap();
    assert!(spool_out.status.success());
    assert!(spool_out.stdout.len() <= INLINE_BUDGET);
    let spool_val: Value = serde_json::from_slice(&spool_out.stdout).unwrap();
    assert_eq!(spool_val["spool"], "@s1");
    assert!(spool_val["results"][0]["hit_count"].as_u64().unwrap() >= 1);

    // 3. Direct `--chunk 0` inspection on `@s1`
    let chunk_out = Command::new(&pctx_link)
        .env("CCTX_HOME", &cctx_home)
        .args(["log", "@s1", "--chunk", "0"])
        .output()
        .unwrap();
    assert!(chunk_out.status.success());
    assert!(chunk_out.stdout.len() <= INLINE_BUDGET);

    // 4. Timeout kills backgrounded subshell cleanly without hanging
    #[cfg(unix)]
    {
        let started = std::time::Instant::now();
        let timeout_out = Command::new(&pctx_link)
            .env("CCTX_HOME", &cctx_home)
            .args([
                "batch",
                "-c",
                "echo before_hang; sleep 15",
                "--timeout",
                "1",
                "--json",
            ])
            .output()
            .unwrap();
        assert!(timeout_out.status.success());
        assert!(started.elapsed().as_secs() < 8);
        let tv: Value = serde_json::from_slice(&timeout_out.stdout).unwrap();
        assert_eq!(tv["commands"][0]["timed_out"], true);
    }
}

#[test]
fn compaction_journal_state_budget_recall_and_multi_agent_hooks() {
    use code_context::journal::DEFAULT_STATE_BUDGET;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let cctx_home = dir.path().join("cctx-state");
    let repo = dir.path().join("workspace");
    std::fs::create_dir_all(&cctx_home).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cctx_home, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // 1. Record pinned decision, blocker, and secret-bearing failure
    let rec_dec = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args([
            "--root",
            repo.to_str().unwrap(),
            "journal",
            "record",
            "--kind",
            "decision",
            "--pin",
            "--summary",
            "Keep 7 native VS Code LM tools and zero extra MCP schemas",
            "--detail",
            "Adding extra MCP schemas adds ~2000 standing tokens per turn; pctx CLI avoids schema tax.",
            "--file",
            "vscode-extension/package.json",
        ])
        .output()
        .unwrap();
    assert!(rec_dec.status.success(), "{}", String::from_utf8_lossy(&rec_dec.stderr));

    let rec_blk = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args([
            "--root",
            repo.to_str().unwrap(),
            "journal",
            "record",
            "--kind",
            "blocker",
            "--summary",
            "컴팩션 이후에도 WAL 체크포인트 타임아웃 원인 유지 필요",
            "--detail",
            "Upstream token API_KEY=topsecret123 failed during stress run",
            "--file",
            "src/journal.rs",
        ])
        .output()
        .unwrap();
    assert!(rec_blk.status.success());

    // Add 30 extra findings to force budget pressure on the 2048B state block
    for idx in 1..=30 {
        let out = Command::new(cctx_bin())
            .env("CCTX_HOME", &cctx_home)
            .args([
                "--root",
                repo.to_str().unwrap(),
                "journal",
                "record",
                "--kind",
                "finding",
                "--summary",
                &format!(
                    "Measured benchmark scenario #{idx} with stable sub-millisecond warm latency and deterministic hashing"
                ),
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    // 2. Verify `resume` stays strictly <= 2048B (`DEFAULT_STATE_BUDGET`), scrubs secrets, and surfaces pinned + blocker
    let resume_out = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args([
            "--root",
            repo.to_str().unwrap(),
            "journal",
            "resume",
            "-q",
            "scenario 17 warm latency",
        ])
        .output()
        .unwrap();
    assert!(resume_out.status.success());
    let resume_block = String::from_utf8(resume_out.stdout).unwrap();
    let trimmed_block = resume_block.trim_end_matches('\n');
    assert!(
        trimmed_block.len() <= DEFAULT_STATE_BUDGET,
        "state block {}B exceeded 2048B: {trimmed_block}",
        trimmed_block.len()
    );
    assert!(trimmed_block.starts_with("<cctx_journal_state"));
    assert!(trimmed_block.ends_with("</cctx_journal_state>"));
    assert!(trimmed_block.contains("#1* [decision]"));
    assert!(trimmed_block.contains("#2 [blocker]"));
    assert!(trimmed_block.contains("[detail: pctx journal show 1]"));

    // 3. `show 2` scrubs secret `topsecret123` while preserving Korean text
    let show_out = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args(["--root", repo.to_str().unwrap(), "journal", "show", "2"])
        .output()
        .unwrap();
    assert!(show_out.status.success());
    let show_text = String::from_utf8(show_out.stdout).unwrap();
    assert!(!show_text.contains("topsecret123"));
    assert!(show_text.contains("<REDACTED>"));
    assert!(show_text.contains("컴팩션"));

    // 4. Korean BM25 + josa search via `cctx probe journal search`
    let search_out = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args([
            "probe",
            "journal",
            "--root",
            repo.to_str().unwrap(),
            "search",
            "컴팩션에서 체크포인트 타임아웃",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(search_out.status.success());
    let search_val: Value = serde_json::from_slice(&search_out.stdout).unwrap();
    assert!(search_val["results"][0]["hit_count"].as_u64().unwrap() >= 1);

    // 5. End-to-end Hook lifecycle:
    //    (a) PostToolUse edit coalescing (snake_case VS Code/Claude payload)
    let run_hook = |payload: &str, agent: &str| -> Value {
        let mut child = Command::new(cctx_bin())
            .env("CCTX_HOME", &cctx_home)
            .args(["journal", "hook", "--agent", agent])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        serde_json::from_slice(&out.stdout).unwrap()
    };

    let edit_file = repo.join("src/resident.rs");
    for _ in 0..3 {
        let p = json!({
            "hook_event_name": "PostToolUse",
            "session_id": "sess-hook",
            "cwd": repo.to_str().unwrap(),
            "tool_name": "Edit",
            "tool_input": { "file_path": edit_file.to_str().unwrap() },
            "tool_response": "updated"
        })
        .to_string();
        assert_eq!(run_hook(&p, "vscode"), json!({}));
    }

    for _ in 0..2 {
        let disconnected = json!({
            "hookEventName": "postToolUse",
            "sessionId": "sess-hook",
            "cwd": repo.to_str().unwrap(),
            "toolName": "cctxNativeFiles",
            "toolResult": {
                "resultType": "failure",
                "textResultForLlm": "No client with the advertised tool definition was connected to run cctxNativeFiles"
            }
        })
        .to_string();
        assert_eq!(run_hook(&disconnected, "vscode"), json!({}));
    }

    //    (b) PreCompact sets pending_compact -> next UserPromptSubmit injects <=2048B additionalContext once!
    let pre_compact = json!({
        "hook_event_name": "PreCompact",
        "session_id": "sess-hook",
        "cwd": repo.to_str().unwrap()
    })
    .to_string();
    assert_eq!(run_hook(&pre_compact, "vscode"), json!({}));

    let user_prompt = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "sess-hook",
        "cwd": repo.to_str().unwrap(),
        "prompt": "Continue fixing the WAL checkpoint issue"
    })
    .to_string();
    let injected = run_hook(&user_prompt, "vscode");
    let ctx_str = injected["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("expected additionalContext after PreCompact");
    assert!(ctx_str.len() <= DEFAULT_STATE_BUDGET);
    assert!(ctx_str.contains("edited_files: src/resident.rs"));
    assert!(ctx_str.contains("stay on code-context-code_* / test-context-test_* MCP"));
    assert_eq!(
        ctx_str
            .matches("stay on code-context-code_* / test-context-test_* MCP")
            .count(),
        1
    );
    assert!(ctx_str.contains("<cctx_journal_state"));

    // Subsequent UserPromptSubmit without another PreCompact injects nothing (zero per-turn overhead!)
    assert_eq!(run_hook(&user_prompt, "vscode"), json!({}));

    //    (c) Copilot CLI camelCase (`preCompact` -> `userPromptSubmitted`)
    let cli_compact = json!({
        "hookEventName": "preCompact",
        "sessionId": "sess-cli",
        "cwd": repo.to_str().unwrap()
    })
    .to_string();
    assert_eq!(run_hook(&cli_compact, "copilot-cli"), json!({}));

    let cli_prompt = json!({
        "hookEventName": "userPromptSubmitted",
        "sessionId": "sess-cli",
        "cwd": repo.to_str().unwrap(),
        "prompt": "status of package.json schema decision"
    })
    .to_string();
    let cli_injected = run_hook(&cli_prompt, "copilot-cli");
    assert!(
        cli_injected["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Keep 7 native VS Code LM tools")
    );

    //    (d) Malformed stdin fails open with `{}` and exit 0
    assert_eq!(run_hook("{not-valid-json", "auto"), json!({}));

    // 6. `hook-config` never registers `PreToolUse` (zero tool-call blocking / latency!)
    let cfg_out = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args(["--root", repo.to_str().unwrap(), "journal", "hook-config", "--agent", "vscode"])
        .output()
        .unwrap();
    assert!(cfg_out.status.success());
    let cfg_json: Value = serde_json::from_slice(&cfg_out.stdout).unwrap();
    assert!(cfg_json["hooks"].get("PreCompact").is_some());
    assert!(cfg_json["hooks"].get("PreToolUse").is_none());

    // 7. `purge --keep-last 2` preserves pinned #1 while trimming unpinned rows
    let purge_out = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args([
            "--root",
            repo.to_str().unwrap(),
            "journal",
            "purge",
            "--keep-last",
            "2",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(purge_out.status.success());
    let show_pinned = Command::new(cctx_bin())
        .env("CCTX_HOME", &cctx_home)
        .args(["--root", repo.to_str().unwrap(), "journal", "show", "1"])
        .output()
        .unwrap();
    assert!(show_pinned.status.success());
}


//! Drive the packed mcp-bridge component through `act run --mcp` with a real
//! MCP client (rmcp), replacing the python fastmcp suite (conftest.py +
//! test_info/test_no_session/test_session_lifecycle/test_credentials).
//!
//! The upstream is `e2e/stub-mcp-server.mjs` — the SAME node stub the python
//! suite used, deliberately strict about the dialect it serves. Started in
//! `modern` mode; `--require-token` makes it 401 every request whose
//! Authorization header does not match, which is how the credential tests
//! prove the token reached the upstream by OUTCOME, never by inspection.
//!
//! Grants: `--allow wasi:http --allow act:credentials` unconditionally — the
//! credentials class is checked when the component starts, not per call.
//!
//! Env: ACT — the act invocation (whitespace-split; `npx @actcore/act`, the
//!      justfile default, works), WASM — the packed component.

use std::path::PathBuf;
use std::process::{Stdio, Child};
use std::time::Duration;

use rmcp::{
    ServiceExt,
    model::CallToolRequestParams,
    transport::{TokioChildProcess, ConfigureCommandExt},
};
use serde_json::{Value, json};

/// `().serve(transport)` hands back the client-role service running over the
/// child process: role first, the unit client handler second.
type Client = rmcp::service::RunningService<rmcp::service::RoleClient, ()>;

fn act_argv() -> Vec<String> {
    std::env::var("ACT")
        .unwrap_or_else(|_| "act".into())
        .split_whitespace()
        .map(str::to_string)
        .collect()
}

fn wasm_path() -> PathBuf {
    PathBuf::from(std::env::var("WASM").unwrap_or_else(|_| {
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../target/wasm32-wasip2/release/mcp_bridge.wasm"
        )
        .into()
    }))
}

fn stub_server_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../e2e/stub-mcp-server.mjs")
}

// ---------------------------------------------------------------------------
// The stub MCP server
// ---------------------------------------------------------------------------

/// A running `stub-mcp-server.mjs`; killed on drop, unconditionally.
struct StubServer {
    child: Child,
    url: String,
}

impl Drop for StubServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn_stub(require_token: Option<&str>) -> StubServer {
    // Port picked the same way the python conftest picked one: above common
    // dev ports, below the ephemeral range, waited on before use.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is set")
        .subsec_nanos() as u64;
    let port = 10000 + (nanos * 7919) % 20000;
    let mut argv: Vec<String> = vec![
        "node".into(),
        stub_server_path().display().to_string(),
        "--port".into(),
        port.to_string(),
        "--mode".into(),
        "modern".into(),
    ];
    if let Some(token) = require_token {
        argv.push("--require-token".into());
        argv.push(token.into());
    }
    let mut child = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn stub-mcp-server.mjs (is node on PATH?)");

    // Wait for the port to actually accept connections — starting the
    // process and hoping costed another component a red CI run on this
    // exact class of race.
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port as u16)).is_ok() {
            return StubServer {
                child,
                url: format!("http://127.0.0.1:{port}/mcp"),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!("stub-mcp-server did not open port {port} in time");
}

// ---------------------------------------------------------------------------
// Clients
// ---------------------------------------------------------------------------

fn spawn_transport(extra: &[String]) -> TokioChildProcess {
    let argv = act_argv();
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.arg("run").arg(wasm_path()).arg("--mcp");
    cmd.args(["--allow", "wasi:http", "--allow", "act:credentials"]);
    cmd.args(extra);
    TokioChildProcess::new(cmd.configure(|_| {})).expect("spawn act run --mcp")
}

/// The suite-wide `client`: no credential store named, so `get-secret` finds
/// nothing — right for every test that names no `credential_key`.
async fn connect() -> Client {
    ().serve(spawn_transport(&[]))
        .await
        .expect("rmcp handshake with act run --mcp")
}

/// The `stored_client` fixture: the same grants plus a credential store.
async fn connect_stored(backend: &str) -> Client {
    ().serve(spawn_transport(&[
        "--credentials-backend".to_string(),
        backend.to_string(),
    ]))
    .await
    .expect("rmcp handshake with act run --mcp")
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn first_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|b| match b {
            rmcp::model::ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .next()
        .unwrap_or_default()
}

/// The python `expect_error` fixture: assert the kind on whichever path it
/// arrives, and optionally a substring of the human message.
async fn expect_error(
    client: &Client,
    tool: &str,
    arguments: Value,
    kind: &str,
    contains: Option<&str>,
    meta: Option<Value>,
) {
    let mut params = CallToolRequestParams::new(tool.to_string());
    if let Some(map) = arguments.as_object() {
        params = params.with_arguments(map.clone());
    }
    if let Some(meta) = meta {
        params.meta = Some(rmcp::model::RequestMetaObject(rmcp::model::MetaObject(
            meta.as_object().expect("meta is an object").clone(),
        )));
    }
    match client.call_tool(params).await {
        Err(rmcp::ServiceError::McpError(e)) => {
            let got = e
                .data
                .as_ref()
                .and_then(|d| d.get("dev.actcore/error-kind"))
                .and_then(|v| v.as_str());
            assert_eq!(got, Some(kind), "expected {kind} on the JSON-RPC error path");
            if let Some(contains) = contains {
                assert!(e.message.contains(contains), "expected {contains:?} in {:?}", e.message);
            }
        }
        Ok(result) => {
            assert_eq!(result.is_error, Some(true), "expected {tool} to fail");
            let got = result
                .meta
                .as_ref()
                .and_then(|m| m.0.get("dev.actcore/error-kind"))
                .and_then(|v| v.as_str());
            assert_eq!(got, Some(kind), "expected {kind} on the isError path");
            if let Some(contains) = contains {
                assert!(
                    first_text(&result).contains(contains),
                    "expected {contains:?} in {:?}",
                    first_text(&result)
                );
            }
        }
        Err(other) => panic!("unexpected transport failure: {other:?}"),
    }
}

fn meta_session_id(sid: &str) -> Option<Value> {
    Some(json!({ "std:session-id": sid }))
}

async fn call(client: &Client, tool: &str, arguments: Value, sid: &str) -> rmcp::model::CallToolResult {
    let mut params = CallToolRequestParams::new(tool.to_string());
    if let Some(map) = arguments.as_object() {
        params = params.with_arguments(map.clone());
    }
    params.meta = Some(rmcp::model::RequestMetaObject(rmcp::model::MetaObject(
        json!({"std:session-id": sid}).as_object().unwrap().clone(),
    )));
    client.call_tool(params).await.expect("call_tool")
}

async fn open_session(client: &Client, args: Value) -> String {
    let result = client
        .call_tool(
            CallToolRequestParams::new("open_session".to_string())
                .with_arguments(args.as_object().unwrap().clone()),
        )
        .await
        .expect("open_session");
    serde_json::from_str::<Value>(&first_text(&result))
        .expect("open_session reply is JSON")["id"]
        .as_str()
        .expect("reply carries an id")
        .to_string()
}

// ---------------------------------------------------------------------------
// test_info.py
// ---------------------------------------------------------------------------

#[test]
fn manifest_reports_name_and_version() {
    let argv = act_argv();
    let out = {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.args(["inspect", "component-manifest"])
            .arg(wasm_path())
            .output()
            .expect("run act inspect component-manifest")
    };
    assert!(out.status.success(), "inspect failed");
    let manifest: Value = serde_json::from_slice(&out.stdout).expect("manifest is JSON");
    assert_eq!(manifest["std"]["name"], "mcp-bridge");
    assert!(manifest["std"]["version"].is_string());
}

// ---------------------------------------------------------------------------
// test_no_session.py
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tools_list_shows_only_the_virtual_session_tools() {
    // "0 tools" has no literal MCP counterpart: the host always shows the
    // two synthesised virtuals for a session-provider component (ACT-SESSIONS
    // §6.1). The faithful translation of the underlying claim — no upstream
    // tool is reachable without a session — is that the list is exactly the
    // two virtuals, never more.
    let client = connect().await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["close_session", "open_session"]);
    client.cancel().await.ok();
}

#[tokio::test]
async fn call_without_session_id_is_invalid_args() {
    let client = connect().await;
    expect_error(
        &client,
        "anything",
        json!({}),
        "std:invalid-args",
        Some("std:session-id"),
        None,
    )
    .await;
    client.cancel().await.ok();
}

// ---------------------------------------------------------------------------
// test_session_lifecycle.py — one client, one session, walked end to end.
// Kept as one test: later steps depend on state (the session id) captured
// from an earlier one, the same way the hurl file it replaces was one file.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_lifecycle() {
    let stub = spawn_stub(None);
    let client = connect().await;

    // ── open-session args schema (driven by Config / schemars) ──
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let open_tool = tools
        .iter()
        .find(|t| t.name.as_ref() == "open_session")
        .expect("open_session is among the tools");
    assert_eq!(
        open_tool.input_schema.get("type").and_then(Value::as_str),
        Some("object")
    );
    assert!(
        open_tool.input_schema["properties"].get("url").is_some(),
        "open_session takes a url"
    );

    // ── Open a session against the upstream ──
    let sid = open_session(&client, json!({ "url": stub.url })).await;
    assert!(
        sid.starts_with("mcp_") && sid["mcp_".len()..].chars().all(|c| c.is_ascii_digit()),
        "hurl's `matches \"mcp_\\\\d+\"`: {sid}"
    );

    // ── tools/list: exactly the two virtuals, never the upstream's tools ──
    // The upstream's tools are reachable through the session — proven by the
    // `echo` call below, which invokes one by name despite it never
    // appearing in tools/list.
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let mut names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
    names.sort();
    assert_eq!(names, ["close_session", "open_session"]);

    // ── tools/call -> echo ──
    let result = call(&client, "echo", json!({"message": "World"}), &sid).await;
    assert_ne!(result.is_error, Some(true), "echo failed");
    assert!(first_text(&result).contains("World"));

    // ── Unknown tool: the stub folds "no such thing" onto -32602, and the
    //    bridge must still classify it as std:not-found (measured against
    //    the running component — a strictly stronger, still-true version of
    //    the hurl file's "any error"). ──
    expect_error(
        &client,
        "nonexistent_tool_xyz",
        json!({}),
        "std:not-found",
        None,
        meta_session_id(&sid),
    )
    .await;

    // ── Close; calls referencing the closed id are std:session-not-found ──
    client
        .call_tool(
            CallToolRequestParams::new("close_session".to_string()).with_arguments(
                json!({"session_id": sid}).as_object().unwrap().clone(),
            ),
        )
        .await
        .expect("close_session");

    expect_error(
        &client,
        "echo",
        json!({"message": "after close"}),
        "std:session-not-found",
        None,
        meta_session_id(&sid),
    )
    .await;

    client.cancel().await.ok();
}

// ---------------------------------------------------------------------------
// test_credentials.py — the upstream's bearer token is not a session
// argument. It lives in the host credential store, is NAMED by
// credential_key, and is fetched on the first tool call.
// ---------------------------------------------------------------------------

const TOKEN: &str = "stub-bearer-token";
const KEY: &str = "upstream";
// The same token, stored the other way: as the std:access-token member of a
// std:oauth2 map rather than as a plain string.
const OAUTH_KEY: &str = "upstream-oauth";
const EXPIRED_KEY: &str = "upstream-expired";

/// The module-scoped credential store: the stub's token stored three ways —
/// plain `mcp:token`, a `std:oauth2` map, and a map that expired in 2001.
/// `=std:oauth2` states the type a name cannot carry (ACT-AUTH §1.1.8).
fn credential_store() -> String {
    // The only legitimate skip: this `act` has no credential store. A
    // harness has no pytest.skip, so the absence is a panic naming the
    // substitution.
    let argv = act_argv();
    let probe = {
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).args(["secret", "--help"]);
        cmd.output().expect("run act secret --help")
    };
    assert!(
        probe.status.success(),
        "this `act` has no credential store (`act secret`); nothing to drive — \
         point ACT at a build with one, e.g. `npx @actcore/act`"
    );

    let root = tempfile::tempdir().expect("create credentials dir");
    let backend = format!("file:{}", root.path().display());
    let entries: [(&str, &str, Value); 3] = [
        (KEY, "mcp:token", json!({ "mcp:token": TOKEN })),
        (
            OAUTH_KEY,
            "mcp:oauth=std:oauth2",
            json!({ "mcp:oauth": { "std:access-token": TOKEN } }),
        ),
        (
            EXPIRED_KEY,
            "mcp:oauth=std:oauth2",
            json!({ "mcp:oauth": { "std:access-token": TOKEN, "std:expires-at": 1_000_000_000 } }),
        ),
    ];
    for (key, field, fields) in entries {
        let mut spawn = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .args([
                "secret",
                "set",
                wasm_path().to_str().unwrap(),
                "--key",
                key,
                "--field",
                field,
                "--fields-stdin",
                "--credentials-backend",
                &backend,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn act secret set");
        {
            use std::io::Write as _;
            spawn
                .stdin
                .take()
                .expect("stdin was piped")
                .write_all(fields.to_string().as_bytes())
                .expect("write fields payload");
        }
        let out = spawn.wait_with_output().expect("act secret set");
        assert!(
            out.status.success(),
            "`act secret set --key {key}` failed even though `act secret` exists:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    // The store dir must outlive every test: leaked on purpose, one per
    // process, exactly like the python module-scoped fixture.
    std::mem::forget(root);
    backend
}

#[tokio::test]
async fn open_session_schema_offers_nowhere_to_put_a_token() {
    // The property the whole migration exists to establish, asserted at the
    // surface an agent actually reads. `auth_token` used to be here.
    let client = connect().await;
    let tools = client.list_all_tools().await.expect("list_all_tools");
    let props = tools
        .iter()
        .find(|t| t.name.as_ref() == "open_session")
        .expect("open_session is among the tools")
        .input_schema["properties"]
        .as_object()
        .expect("properties is an object")
        .clone();
    assert!(props.contains_key("credential_key"));
    for forbidden in [
        "auth_token",
        "token",
        "api_key",
        "password",
        "secret",
        "authorization",
    ] {
        assert!(
            !props.contains_key(forbidden),
            "{forbidden} must never be a session argument"
        );
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_token_from_the_store_reaches_the_upstream() {
    // End to end: the stub refuses every request without the right bearer,
    // so a successful `echo` proves the header was built from the stored
    // field — without any test, log or server ever printing the token.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect_stored(&credential_store()).await;
    let sid = open_session(&client, json!({ "url": stub.url, "credential_key": KEY })).await;
    let result = call(&client, "echo", json!({"message": "World"}), &sid).await;
    assert_ne!(result.is_error, Some(true), "echo failed");
    assert!(first_text(&result).contains("World"));
    client
        .call_tool(
            CallToolRequestParams::new("close_session".to_string()).with_arguments(
                json!({"session_id": sid}).as_object().unwrap().clone(),
            ),
        )
        .await
        .expect("close_session");
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_oauth_credential_reaches_the_upstream_too() {
    // The same token stored as a std:oauth2 map, chosen by the field name
    // and nothing else — the one link in the chain neither repo can test on
    // its own.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect_stored(&credential_store()).await;
    let sid = open_session(
        &client,
        json!({ "url": stub.url, "credential_key": OAUTH_KEY }),
    )
    .await;
    let result = call(&client, "echo", json!({"message": "World"}), &sid).await;
    assert_ne!(result.is_error, Some(true), "echo failed");
    assert!(first_text(&result).contains("World"));
    client
        .call_tool(
            CallToolRequestParams::new("close_session".to_string()).with_arguments(
                json!({"session_id": sid}).as_object().unwrap().clone(),
            ),
        )
        .await
        .expect("close_session");
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_expired_oauth_token_is_refused_before_the_request_goes_out() {
    // std:expires-at is honoured HERE, not left to the upstream: a 401 reads
    // as "wrong token" and sends the operator to re-check the value. The
    // token itself is the one the stub accepts, so this fails only because
    // of the expiry.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect_stored(&credential_store()).await;
    let sid = open_session(
        &client,
        json!({ "url": stub.url, "credential_key": EXPIRED_KEY }),
    )
    .await;
    expect_error(
        &client,
        "echo",
        json!({"message": "x"}),
        "std:credential-required",
        Some("act login"),
        meta_session_id(&sid),
    )
    .await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_unauthenticated_upstream_needs_no_credential() {
    // credential_key is optional on purpose: a bridge pointed at a server
    // that wants no token asks the store for nothing.
    let stub = spawn_stub(None);
    let client = connect().await;
    let sid = open_session(&client, json!({ "url": stub.url })).await;
    let result = call(&client, "echo", json!({"message": "World"}), &sid).await;
    assert_ne!(result.is_error, Some(true), "echo failed");
    assert!(first_text(&result).contains("World"));
    client
        .call_tool(
            CallToolRequestParams::new("close_session".to_string()).with_arguments(
                json!({"session_id": sid}).as_object().unwrap().clone(),
            ),
        )
        .await
        .expect("close_session");
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_key_with_no_credential_behind_it_is_credential_required() {
    // `not-found` and `denied` collapse into one kind and one message. The
    // message has to carry the fix, so the command is asserted.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect_stored(&credential_store()).await;
    let sid = open_session(
        &client,
        json!({ "url": stub.url, "credential_key": "no-such-key" }),
    )
    .await;
    expect_error(
        &client,
        "echo",
        json!({"message": "x"}),
        "std:credential-required",
        Some("act secret set"),
        meta_session_id(&sid),
    )
    .await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_missing_store_entry_is_reported_on_the_first_call_not_at_open() {
    // `open_session` cannot fetch a credential — the host marks a session
    // live only after it returns (ACT-AUTH §1.1.4) — so it returns an id and
    // the FIRST tool call is where the credential problem surfaces.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect_stored(&credential_store()).await;
    let sid = open_session(
        &client,
        json!({ "url": stub.url, "credential_key": "no-such-key" }),
    )
    .await;
    assert!(sid.starts_with("mcp_"), "open must still succeed");
    expect_error(
        &client,
        "echo",
        json!({"message": "x"}),
        "std:credential-required",
        None,
        meta_session_id(&sid),
    )
    .await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn an_upstream_that_rejects_the_credential_poisons_the_session() {
    // A 401 is not transient: the key is fixed at open, and retrying can
    // only re-run get-secret — which can put a consent prompt in front of a
    // human on every call. Driven through the suite-wide client, which
    // names no store, so the bridge reaches an authenticated upstream with
    // no token at all. Both calls must fail the same way; the second is
    // answered from the poison flag.
    let stub = spawn_stub(Some(TOKEN));
    let client = connect().await;
    let sid = open_session(&client, json!({ "url": stub.url })).await;
    for _ in 0..2 {
        expect_error(
            &client,
            "echo",
            json!({"message": "x"}),
            "std:capability-denied",
            Some("credential_key"),
            meta_session_id(&sid),
        )
        .await;
    }
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_url_carrying_userinfo_is_refused_at_open() {
    // A URL is the other place a credential fits, and `url` is copied into
    // secret-request.resource, which is host-visible by contract.
    let client = connect().await;
    expect_error(
        &client,
        "open_session",
        json!({ "url": "https://user:hunter2-sentinel@127.0.0.1:9/mcp" }),
        "std:invalid-args",
        Some("userinfo"),
        None,
    )
    .await;
    client.cancel().await.ok();
}

#[tokio::test]
async fn a_credential_key_that_is_a_sentence_is_refused_at_open() {
    // The host pastes this string raw into the line a human reads while
    // deciding to release a credential.
    let client = connect().await;
    expect_error(
        &client,
        "open_session",
        json!({
            "url": "http://127.0.0.1:9/mcp",
            "credential_key": "prod (approved by your administrator)",
        }),
        "std:invalid-args",
        Some("credential_key"),
        None,
    )
    .await;
    client.cancel().await.ok();
}

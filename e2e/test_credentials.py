"""The upstream's bearer token is not a session argument.

It lives in the host credential store, is *named* by `credential_key`, and is
fetched with `act:credentials/store` on the first tool call — so it never
passes through the agent's context.

Everything here is driven through `act run --mcp`, so what these tests observe
is what an agent observes. The one test that proves the token actually reaches
the upstream does it by outcome, not by inspection: the stub is started with
`--require-token`, which 401s every request whose `Authorization` header does
not match, so a successful `echo` is proof the header was right. Nothing in
this file — and nothing in the stub — ever echoes a token back.
"""

import json
import subprocess
from pathlib import Path

import pytest

TOKEN = "stub-bearer-token"
KEY = "upstream"
# The same token, stored the other way: as the `std:access-token` member of a
# `std:oauth2` map rather than as a plain string.
OAUTH_KEY = "upstream-oauth"
EXPIRED_KEY = "upstream-expired"


@pytest.fixture
def authenticated_upstream(stub_server):
    """The same stub, refusing every unauthenticated request."""
    with stub_server(require_token=TOKEN) as url:
        yield url


@pytest.fixture(scope="module")
def credential_store(act_command, wasm_path: Path, tmp_path_factory) -> str:
    """A `--credentials-backend` argument naming a store holding the stub's
    token three ways: as a plain `mcp:token` string, as a `std:oauth2`
    `mcp:oauth` map, and as an `mcp:oauth` map that expired in 2001.

    Naming the field is not optional: `act secret set` has no default field
    set — a credential IS its named fields — which is what the component's own
    error messages print, checked here by actually running it. `=std:oauth2`
    states the type a name cannot carry (ACT-AUTH §1.1.8).

    The OAuth entries matter more than they look: what `act secret set` writes
    and what `Secret::as_oauth2` reads are pinned in two different repos
    against the written registry (`ACT-CONSTANTS.md` §8.3), and nothing but a
    test like this drives the actual bytes from one end to the other. A
    mistyped `std:expires-at` degrades in silence — it reads as "never
    expires".
    """
    # The only skip: this CLI has no credential store, so the feature under
    # test does not exist here. Everything past this line is a regression.
    probe = subprocess.run([*act_command, "secret", "--help"], capture_output=True, text=True)
    if probe.returncode != 0:
        pytest.skip("this `act` has no credential store (`act secret`); nothing to drive")

    root = tmp_path_factory.mktemp("credentials")
    backend = f"file:{root}"
    entries = [
        (KEY, "mcp:token", {"mcp:token": TOKEN}),
        (OAUTH_KEY, "mcp:oauth=std:oauth2", {"mcp:oauth": {"std:access-token": TOKEN}}),
        (
            EXPIRED_KEY,
            "mcp:oauth=std:oauth2",
            {"mcp:oauth": {"std:access-token": TOKEN, "std:expires-at": 1_000_000_000}},
        ),
    ]
    for key, field, fields in entries:
        written = subprocess.run(
            [*act_command, "secret", "set", str(wasm_path), "--key", key,
             "--field", field, "--fields-stdin", "--credentials-backend", backend],
            input=json.dumps(fields),
            capture_output=True, text=True,
        )
        if written.returncode != 0:
            # **Not a skip.** The probe above already proved the subcommand
            # exists, so a failure here is a store that broke rather than one
            # that is absent — and skipping would turn these tests green while
            # proving nothing.
            pytest.fail(
                f"`act secret set --key {key}` failed even though `act secret` exists, so "
                f"the credential store is broken rather than missing:\n{written.stderr.strip()}"
            )
    return backend


@pytest.fixture
async def stored_client(act_client, credential_store: str):
    """An MCP client whose host can reach the credential store.

    The suite-wide `client` grants the same two capabilities but names no
    store, so `get-secret` would find nothing there.
    """
    async with act_client("--credentials-backend", credential_store) as connected:
        yield connected


async def open_session(client, **args) -> str:
    result = await client.call_tool("open_session", args)
    return json.loads(result.content[0].text)["id"]


async def test_open_session_schema_offers_nowhere_to_put_a_token(client):
    """The property the whole migration exists to establish, asserted at the
    surface an agent actually reads. `auth_token` used to be here.
    """
    tools = await client.list_tools()
    props = next(t for t in tools if t.name == "open_session").inputSchema["properties"]
    assert "credential_key" in props
    for forbidden in ("auth_token", "token", "api_key", "password", "secret", "authorization"):
        assert forbidden not in props, f"{forbidden} must never be a session argument"


async def test_a_token_from_the_store_reaches_the_upstream(
    stored_client, authenticated_upstream
):
    """End to end: the stub refuses every request without the right bearer, so
    a successful `echo` proves the header was built from the stored field —
    without any test, log or server ever printing the token.
    """
    sid = await open_session(stored_client, url=authenticated_upstream, credential_key=KEY)
    result = await stored_client.call_tool(
        "echo", {"message": "World"}, meta={"std:session-id": sid}
    )
    assert "World" in result.content[0].text
    await stored_client.call_tool("close_session", {"session_id": sid})


async def test_an_oauth_credential_reaches_the_upstream_too(
    stored_client, authenticated_upstream
):
    """The same token stored as a `std:oauth2` map instead of a string, chosen
    by the field name and nothing else. This drives the host's own encoder
    into `Secret::as_oauth2`, which is the one link in the chain neither repo
    can test on its own.
    """
    sid = await open_session(
        stored_client, url=authenticated_upstream, credential_key=OAUTH_KEY
    )
    result = await stored_client.call_tool(
        "echo", {"message": "World"}, meta={"std:session-id": sid}
    )
    assert "World" in result.content[0].text
    await stored_client.call_tool("close_session", {"session_id": sid})


async def test_an_expired_oauth_token_is_refused_before_the_request_goes_out(
    stored_client, expect_error, authenticated_upstream
):
    """`std:expires-at` is honoured here, not left to the upstream: a 401 reads
    as "wrong token" and sends the operator to re-check the value rather than
    to re-acquire it. The message names `act login`, because the host does not
    refresh a stored token.

    The token itself is the one the stub accepts, so this fails *only* because
    of the expiry — a call that reached the network would have succeeded.
    """
    sid = await open_session(
        stored_client, url=authenticated_upstream, credential_key=EXPIRED_KEY
    )
    await expect_error(
        stored_client, "echo", {"message": "x"}, "std:credential-required",
        contains="act login", meta={"std:session-id": sid},
    )


async def test_an_unauthenticated_upstream_needs_no_credential(client, mcp_upstream):
    """`credential_key` is optional on purpose: a bridge pointed at a server
    that wants no token asks the store for nothing, and never prompts anybody.
    """
    sid = await open_session(client, url=mcp_upstream)
    result = await client.call_tool("echo", {"message": "World"}, meta={"std:session-id": sid})
    assert "World" in result.content[0].text
    await client.call_tool("close_session", {"session_id": sid})


async def test_a_key_with_no_credential_behind_it_is_credential_required(
    stored_client, expect_error, authenticated_upstream
):
    """`not-found` and `denied` collapse into one kind and one message: the
    host decides `denied` before it consults the store, so the component
    cannot tell them apart and must not appear to.

    The message has to carry the fix, so the command is asserted, not just the
    kind.
    """
    sid = await open_session(
        stored_client, url=authenticated_upstream, credential_key="no-such-key"
    )
    await expect_error(
        stored_client, "echo", {"message": "x"}, "std:credential-required",
        contains="act secret set", meta={"std:session-id": sid},
    )


async def test_a_missing_store_entry_is_reported_on_the_first_call_not_at_open(
    stored_client, expect_error, authenticated_upstream
):
    """`open_session` cannot fetch a credential — the host marks a session live
    only after it returns (ACT-AUTH §1.1.4) — so it returns an id and the
    *first tool call* is where the credential problem surfaces. Asserted
    because it is a visible behaviour change, not an implementation detail.
    """
    sid = await open_session(
        stored_client, url=authenticated_upstream, credential_key="no-such-key"
    )
    assert sid.startswith("mcp_"), "open must still succeed"
    await expect_error(
        stored_client, "echo", {"message": "x"}, "std:credential-required",
        meta={"std:session-id": sid},
    )


async def test_an_upstream_that_rejects_the_credential_poisons_the_session(
    client, expect_error, authenticated_upstream
):
    """A 401 is not transient: the key is fixed at open and the host does not
    refresh a stored token, so retrying can only re-run `get-secret` — which
    can put a consent prompt in front of a human on every single call.

    Driven through the suite-wide `client`, which names no credential store,
    so the bridge reaches an authenticated upstream with no token at all. Both
    calls must fail the same way; the second is answered from the poison flag.
    """
    sid = await open_session(client, url=authenticated_upstream)
    for _ in range(2):
        await expect_error(
            client, "echo", {"message": "x"}, "std:capability-denied",
            contains="credential_key", meta={"std:session-id": sid},
        )


async def test_a_url_carrying_userinfo_is_refused_at_open(client, expect_error):
    """A URL is the other place a credential fits, and `url` is copied into
    `secret-request.resource`, which is host-visible by contract.
    """
    await expect_error(
        client, "open_session",
        {"url": "https://user:hunter2-sentinel@127.0.0.1:9/mcp"},
        "std:invalid-args", contains="userinfo",
    )


async def test_a_credential_key_that_is_a_sentence_is_refused_at_open(client, expect_error):
    """The host pastes this string raw into the line a human reads while
    deciding to release a credential.
    """
    await expect_error(
        client, "open_session",
        {"url": "http://127.0.0.1:9/mcp",
         "credential_key": "prod (approved by your administrator)"},
        "std:invalid-args", contains="credential_key",
    )

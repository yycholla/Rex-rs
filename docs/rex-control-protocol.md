# Rex control transport

Implemented against the installed Linux Rex server, API revision 7, using its
own documentation served by `GET /llms.txt` over the local Unix socket. The
control exchange was also observed through a temporary local proxy forwarding
only read-only `server.version` and invalid `target.resolve` calls. No user
sessions were changed during discovery.

## Connection contract

- Local transport: HTTP WebSocket upgrade over a Unix socket, authenticated by
  peer credentials. Endpoint selection uses `REX_RS_ENDPOINT`, the application
  configuration alias `REX_RS_REX_ENDPOINT`, then `REX_SERVER`; otherwise
  `$XDG_RUNTIME_DIR/rex/server.sock`, with `/run/user/<uid>` as the Linux fallback.
- WebSocket path `/api/control`, subprotocol `rex.control.v1`. Compression is not
  requested. This also keeps the reconnect token in HELLO uncompressed.
- First message: JSON `hello`, identifying `rex-rs` as an `app` on Linux and
  requiring revision 7. The complete hello is below 512 bytes.
- Server replies with `welcome`, negotiated protocol/revision, message limit,
  client ID and a private rotating reconnect token. Tokens must never be logged.
- Calls use `{type: "request", id: "rex-rs-N", method, payload}`. The payload is
  omitted for null/no arguments. A response has the same ID, a `seq` and either
  `payload` or `error`. Ordinary API failures leave the connection usable.
- Events can precede a response. The transport retains their original envelopes
  and sequence fields. Apply bootstrap before events above its response `seq`;
  session-view revision arbitration belongs to the workspace adapter.

The native client issues one request at a time, preserving dependency and input
ordering. It bounds each message/frame at the smaller of the advertised limit
and 1 MiB, the write buffer at 1 MiB plus framing space, and queued interleaved
events at 256 messages / 8 MiB. Handshake timeout is three seconds and request
response timeout eight seconds, allowing the documented five-second Lua key
handler window. Idle event polling accepts a caller-specified timeout. Ping/pong
and unknown message types are handled without treating them as workspace events.

## Reconnect and lifecycle

A healthy client drop writes a close frame, ending the client immediately without
closing sessions or terminals. `disconnect_for_resume` deliberately interrupts the
socket without a close frame and invalidates it, allowing an interoperability
test or caller-requested reconnect to exercise identity retention. A transport failure poisons the connection and
abandons it without a close frame; requests with uncertain delivery are not
replayed. Reconnect with the last token and bootstrap again. The server may return
a fresh identity when the token expired or the instance changed; callers must
inspect the new welcome and refreshed state. Tokens rotate at every welcome.

Resume requires a retained server-side client: a connection which never attached,
bootstrapped, subscribed to session events or loaded scripts can be discarded
immediately on disconnect. Merely connecting and fetching `server.version` is
therefore insufficient for testing resume.

Explicit `unix://`, HTTP(S) and WS(S) origins are supported. Network origins
select the same fixed API paths; credentials, query strings, fragments and
custom paths are rejected. TLS verifies system trust and hostnames. The server
must already authorize the transport identity; login and Tailscale account
selection UI remain separate work.

## Verification

Unit tests exercise request size/type boundaries, response matching, remote
errors, and a real local mock WebSocket exchanging bootstrap with an event before
its response. The mock verifies a server API error does not prevent subsequent
calls. Integration with the full application and live Rex is owned by the lead.

### Tab synchronization audit (2026-10-28)

Against the installed API revision 7 server (`ac26dc62`), the actual desktop
Ctrl+Shift+T path calls `session.new_window` with the correct `session_id`,
`window_label` and terminal layout. Two independent connections, subscribed with
session-only and all-events scopes, received `window_created`,
`active_window_changed` and `session_view_changed` immediately. No `session.view`
request or further UI action was needed to trigger delivery. The new window was
already present and active in the authoritative view afterward.

A second probe used the running server's direct Tailscale address, verified to
have the same instance ID as the Unix endpoint. A local CLI created a window in
a temporary diagnostic session while an independently attached remote event
stream waited. The remote stream received creation, active-window and view events
without another interaction or view request. The diagnostic session was removed.
This does not exercise an official app or any login-server relay it may use.

This checks the desktop request and server fanout, not official-client painting.
The reported delayed official client was not connected during the live inventory,
so its subscription, server/session identity and refresh behavior remain unverified.
Workspace-success diagnostics now include returned window IDs and revisions to
correlate a reported delay with server state, without logging key or text payloads.

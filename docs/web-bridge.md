# Native web bridge (opt-in)

`cargo build --no-default-features --features ghostty --bin rex-web-bridge`

Run `rex-web-bridge --endpoint <operator-configured Rex endpoint>` as a gateway
child process. This executable is **not an HTTP server or authentication layer**.
The gateway must restrict endpoints and executable paths to trusted configuration,
authorize one owner, check write origins, bound child lifetimes and request rates,
and never expose the pipe or endpoint choice to unauthenticated clients.
It does not start Rex, create a session on attachment, or take size ownership.
Dropping the process detaches; Rex continues to own programs and workspace state.
Use only trusted Rex servers. No installation, deployment or service enablement
is performed by this change.

## NDJSON protocol

Each stdin line is a JSON object. Each stdout line is exactly one response:
`{"id":"request-id","ok":true,"result":...}` or
`{"id":"request-id","ok":false,"error":{"code":"rejected","message":"..."}}`.
Malformed input uses a null id and `invalid_request`. Diagnostics go to stderr.
IDs must be unique per process (maximum 80 bytes). Unknown fields/operations are
rejected. There are no unsolicited events. Poll snapshot to obtain updates.

Requests:

- `{id,op:"snapshot"}`
- `{id,op:"select",generation,sessionId,windowId?}`
- `{id,op:"input",generation,blockId,text,paste?}` (UTF-8, at most 8192 bytes; one printable character unless paste:true)
- `{id,op:"input",generation,blockId,key,modifiers?}` (native key event)
- `{id,op:"resize",generation,blockId,cols,rows}` (only existing size owner)
- `{id,op:"command",generation,action,sessionId?,windowId?,blockId?,title?,confirmed:true}`

Commands: `new-session`, `new-window`, `rename-session`, `rename-window`,
`split-right`, `split-down`, `close-block`, `close-window`, `close-session`,
`detach-block`, `restore-block`. These require explicit user intent; the bridge
cannot itself prove that intent. Selection never launches or takes ownership,
but window selection updates Rex's authoritative shared focus.

Input requires exactly one of text/key. Composed/multi-character strings require
`paste:true`; they are not a single native key event. Native key names are one ASCII alphanumeric
or punctuation character, or enter/backspace/delete/insert/tab/escape/up/down/left/
right/home/end/pageup/pagedown/space/f1 through f12. Modifiers when supplied contain
all four native boolean fields: shift,control,alt,super_key. `paste:true` invokes
native bracket-aware paste and supports multiline text; it also acknowledges only
queued. Do not encode key presses as ANSI escape strings.

`generation` is an **opaque string** containing a process incarnation and native
connection generation. A mutation must use the current snapshot generation.
Every target ID is the native remote string, not a transient pane index. Cross-
session window selection is rejected. Never replay mutations automatically.
`input` and `resize` return `{ack:"queued"}`, not a claim of remote application.
Commands/select return `{ack:"applied"}` only after native completion. A timeout
or completion interruption returns `uncertain`; inspect authoritative state rather
than retrying. Duplicate request IDs are rejected, including failed requests.
The gateway should additionally fence browser requests by child-process epoch.

Snapshot result:

```
{connected,generation,revision,serverId,activeSessionId,
 sessions:[{id,title,activeWindowId,windows:[{id,title,activeBlockId,zoomedBlockId,visibleBlockIds,
   layers:[{id,kind,bounds:[x,y,width,height],layout}]}]}],
 blocks:[{id,sessionId,title,owner,terminal,detached,status,inputAvailable,ownsSize}],
 frames:{[blockId]:{cols,rows,lines,cursor,foreground,background,imagesOmitted}},
 errors:[]}
```

Layout is null, `{type:"pane",blockId}`, or recursive
`{type:"split",axis:"horizontal"|"vertical",ratio,first,second}`.
`lines` is an array of rows containing native CellRun objects:
`{column,columns,text,foreground,background,style}`. RGB is `{r,g,b}`.
Style fields are `bold,italic,underline,underline_kind,underline_color,inverse,
invisible,faint,blink,strikethrough,overline`. Underline kind is
`None|Single|Double|Curly|Dotted|Dashed`. Cursor is null or
`{column,row,style,color,blinking,wide_tail}`, with style
`Block|Bar|Underline|BlockHollow`.
Render text as text, never HTML. The owned frame projection comes from the
existing WorkspaceClient decoder. Raw GHOSTSNP, control messages, image pixels,
clipboard operations, and externally requested client actions are never exposed
or executed. Images are deliberately omitted and flagged.

Limits: 32 KiB input line, 8 pending input lines, 16 MiB response, 262144 exported
cells, 100000 unique requests per process, native bounded workspace/cache limits.
An overlong input line terminates the bridge rather than buffering its remainder.
Output backpressure blocks the single consumer; the queue remains bounded.
There is no scrollback pagination or image rendering in this first protocol.
Resize uses 8x16 cell pixels and never implicitly takes ownership.

## Verification

`cargo test --no-default-features --features ghostty --bin rex-web-bridge`

`python3 scripts/web-bridge-smoke.py /absolute/path/to/rex-web-bridge`

The smoke runner starts a disposable installed Rex server with its own temporary
socket/config/state, creates only fixture sessions, verifies colored decoded
output, splits/tabs, reconnect/stale generation, detach/restore and cleanup.
It never connects to a user's default Rex server.

Window visibility is authoritative: `zoomedBlockId` is null when unzoomed;
`visibleBlockIds` contains the zoom target alone or all placed blocks, including
floating and unsupported content. Layers retain the complete layout so unzoom
can restore it. Consumers must honor visibility instead of rendering every layer.
`frames` exports only terminal blocks visible in the active session's active
window, never cached siblings hidden by zoom or cached frames from inactive tabs.
A visible block may have no frame while its native stream is loading.

# Rex terminal data implementation

The installed server's `GET /llms.txt` is the primary protocol reference. Fetch it
through the configured Unix socket; unlike public documentation, it describes the
running server's exact API revision. Discovery on 2026-10-28 used API revision 7.

`src/rex/terminal.rs` opens an uncompressed WebSocket at `/api/data` with
`rex.data.terminal.v1`. An attached persistent control connection must first call
`data.connect` for the block's `terminal` source. A HELLO consists of opcode 1
and the ticket's 16 decoded base64url bytes. A WELCOME has opcode 129, a 16-byte
resume token, three reserved zero bytes, a little-endian u32 grace interval and
a little-endian u64 channel nonce. A REJECT has opcode 130 and one reason byte.
Tokens and tickets must not be logged.

After WELCOME the binary WebSocket messages form one continuous byte stream;
message boundaries have no semantic meaning. The first bytes form a complete
Ghostty `GHOSTSNP` version-1 snapshot. Each record has a u16 tag, u32 payload size,
u32 CRC32C and payload, with little-endian fields. The empty tag-6 FINISH ends
the snapshot. We assemble records within a 64 MiB bound, check visible dimensions
before native import, and use Ghostty's decoder to validate checksums, ordering,
terminal state and parser continuation. Trailing bytes from the same WebSocket
message are fed as live data after import. The older published Ghostty Rust
bindings lack snapshot import; this is the concrete reason for the pinned
snapshot-capable binding update.

Kitty graphics records use tag 8 for image/placement metadata, tag 9 for pixels
and tag 10 for history placements. The vendored `libghostty-vt-sys` retains the
pinned wrapper/native base and applies six upstream snapshot commits plus a
static built-in PNG decoder. Patch identities and hashes are recorded in
`vendor/libghostty-vt-sys/PROVENANCE.json`. Patches apply only to an owned build
source copy. Snapshot checksums and native record validation remain mandatory.

Image pixels and placements cross the worker boundary as owned data. The UI
clips source rectangles and paints three native layers around cell backgrounds
and text. Unchanged text rows and image pixels retain their caches. Native image
storage is 16 MiB per screen; visible owned pixels and textures are each bounded
to 16 MiB, images to 4096 pixels per dimension, and placements to 4096. PNG decode
uses a 64 MiB transient allocation cap. File and shared-memory image transports
are disabled in replicas. Oversized/unavailable graphics produce an omission
message while text remains usable. Only visible live streams advance animation;
hidden panes release UI textures. Replicas never send graphics-query replies.

Live bytes include the program's VT output and Rex APC sequences:
`ESC _ rex1;<decimal nonce>;<JSON> ESC \\`. Only a matching nonce authorizes
resize, theme or child-exit messages. Unknown APCs, including look-alikes with a
wrong nonce, pass through the VT parser. Non-Rex APC content streams without
being accumulated, supporting large Kitty sequences. Matching Rex APCs have a
64 KiB client bound. Resize dimensions are limited to 65,536 visible cells.
Theme setters affect the presentation replica only. Unknown Rex types are skipped.
An authenticated `child_exited` ends program output. The client publishes any
pending final frame immediately, retains it for scrolling/copying, disables
process input and stops reading or resuming that completed data stream. A later
socket close therefore cannot replace the exit status with a connection error.
For other data failures, refresh the authoritative view before attempting resume;
tab/session removal can close the data socket before its control event arrives.

Ghostty remains on the IO worker that owns the stream. Only owned frames cross
threads. Rex's terminal owns query responses; the display replica never sends
its own replies to the process. Keys, paste and resize requests use the ordered
control connection, not the data socket (the server ignores post-WELCOME client
messages). Scrolling operates on this client's restored history.

A successful stream chunk increments the accepted byte count, including snapshot
and APC bytes. RESUME sends opcode 2, the latest 16-byte reconnect token, and that
u64 count. Its WELCOME must retain the channel nonce and rotates the token.
A refused resume requires discarding the replica and requesting a fresh ticket;
continuing from a text capture would not establish a faithful baseline. A decode
failure is nonresumable because it may already have applied part of a chunk; the
client reports it without repeatedly opening the same broken stream.

A disposable `rex-rs-stream-probe` session verified HELLO/WELCOME, initial binary
snapshot, resize APC and subsequent VT output. The probe destroyed its session
in a finally block. Existing user sessions were not changed. The integrated
`rex-native-smoke` probe provides repeatable checks against a separate server.

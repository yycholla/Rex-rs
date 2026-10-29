# Rex API 7 graphics snapshot fixture

Captured on 2026-10-29 from a temporary, isolated local Rex server. The only
program was a private Python process that emitted one black RGB pixel (Kitty
image ID 4242, placement two columns by two rows), followed by `image-ready`.
The fixture contains no user terminal contents, credentials, reconnect token,
or data ticket. The private server and program were stopped after capture.

Server version: `0.0.1793195882-gac26dc`, commit
`ac26dc62e1e92cc2e4eabde482e5400cf9a97961`, Linux amd64, API revision 7.
The snapshot came from the authoritative `/api/data` terminal stream after its
WELCOME message; it begins at `GHOSTSNP` and ends after the FINISH record.

The 21-byte tag-9 payload has an 18-byte header followed by three raw RGB bytes.
It has **no compression byte**. Upstream Ghostty commit
`7fdd2df67ac61762912468d3fe9676fd766c20be` matches this encoding. The earlier
`04f20eb16` decoder incorrectly consumed the first pixel byte as compression,
which either rejected the snapshot or omitted its image. A local roundtrip alone
could not reveal that mismatch.

The captured terminal had zero pixel dimensions. The regression supplies an
8-by-16 cell size after import, then checks pixels, image identity, placement,
retained text, and absence of process replies.

Size: 1309 bytes. SHA-256: `633424440b3cc1401747392c563058dc9b63f6d2317fd1a4fdd500b87b18e00f`.

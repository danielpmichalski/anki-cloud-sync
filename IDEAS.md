# Ideas

## JSON format for Anki collection (future consideration)

OSS libraries exist that can parse `.apkg`/`.anki2` into JSON (e.g. genanki, anki-export).
Could be useful if Sidecar API grows complex enough that direct SQLite queries become
maintenance burden, or if upstream Anki schema changes frequently.

**Current verdict:** premature. Sidecar API reads SQLite directly and serializes to JSON
in response handlers. Revisit if Anki schema instability becomes a real problem.

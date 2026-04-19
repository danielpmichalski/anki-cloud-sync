# TODO

## Sidecar API additions (required by anki-cloud issue #15)

- [ ] POST /internal/v1/decks/:id/notes/bulk
      Wire col.add_notes() from rslib/src/notes/mod.rs
      Request: {"notes":[{"fields":{},"tags":[],"noteTypeId":"str?"}]}
      Response: {"ids":["str"]} 201

- [ ] Pagination on GET /internal/v1/decks
      Add query params: limit (int, default 100, max 1000), cursor (last seen id as str)
      Response: add "nextCursor": "str|null" field

- [ ] Pagination on GET /internal/v1/decks/:id/notes
      Same limit/cursor pattern

- [ ] Pagination on GET /internal/v1/notes/search?q=
      Same limit/cursor pattern

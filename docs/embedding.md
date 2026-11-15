# Embedding the web app in a page

`apps/photocraft-web` can run inside an `<iframe>` of another page on the **same origin**, which
then owns the document: it hands PhotoCraft a `.pcraft`, and PhotoCraft hands the edited
document back when it is saved. Nothing is picked from disk or downloaded for that document.
This is for a site that keeps its users' documents itself (a design tool that wants PhotoCraft as
its advanced editor), not for opening files from the user's computer, which the standalone web
app does.

The code is `apps/photocraft-web/src/embed.rs`.

## Turning it on

Load the web app's page in an iframe with `?embed` in its URL:

```html
<iframe src="/photocraft/index.html?embed&lang=de"></iframe>
```

- `?embed` takes effect only in a frame whose parent is on the same origin. Anywhere else the app
  runs as the standalone web app and logs why.
- `?lang=<tag>` sets the interface language (as Preferences › Interface › Language does; `de`,
  `pt-BR`, …). It works outside embed mode too. A language PhotoCraft has no catalog for is
  ignored.
- `?cpu` and `?webgl` keep their meaning (see `docs/development.md`, Web build).

## Messages

Both sides use `window.postMessage`. Every message is a plain object with `photocraftEmbed: 1` and
a `type`. PhotoCraft reads messages only from its parent window on its own origin, and posts only
to its parent, on that origin. The host should check `event.source === iframe.contentWindow` and
`event.origin === location.origin` the same way.

PhotoCraft → host:

| `type` | Fields | When |
|---|---|---|
| `ready` | `pcraft` (the `.pcraft` format version it writes), `rev` (the commit it was built from, `PHOTOCRAFT_BUILD_SHA`, or `null`), `version` | The app runs and waits for `open`. |
| `opened` | `warnings` (strings) | The document is open. |
| `open-failed` | `error` | The document could not be opened. |
| `dirty` | `dirty` (boolean) | Whenever the document's unsaved state changes, and once after `opened`. |
| `document` | `name`, `revision`, `document` (an `ArrayBuffer`, transferred) | File › Save wrote the document: asked for by `save`, or the user's own File › Save. |
| `save-failed` | `error` | `save` could not write the document. |
| `snapshot-start` | `revision` | A recovery copy of that revision begins (below). |
| `snapshot-part` | `revision`, `path` (a bundle object path, `tiles/<blake3>.zst` or `blobs/<blake3>.zst`), `data` (`ArrayBuffer`, transferred) | One object of the copy the host does not hold yet, as the bundle stores it. |
| `snapshot` | `revision`, `manifest` (`ArrayBuffer`, the bundle's `manifest.json`, transferred), `paths` (every object path it names), `parts` (how many this copy sent), `ms` (main-thread time of all its steps), `maxSliceMs` (its longest step), `slices` | The copy is complete: the host holds every object of `paths`, from this copy's `snapshot-part`s or earlier ones. |
| `snapshot-failed` | `error` | A recovery copy could not be written. |
| `start-failed` | `error` | The app could not start (the browser has neither WebGPU nor WebGL2). Nothing follows. |

Host → PhotoCraft (post to `iframe.contentWindow` with `location.origin` as the target origin):

| `type` | Fields | Effect |
|---|---|---|
| `open` | `name` (a plain `.pcraft` file name), `document` (`ArrayBuffer`) or `parts` (`{manifest: ArrayBuffer, objects: [{path, data: ArrayBuffer}]}`, a recovery copy as the host kept it), `fonts` (optional, `[{family, data: ArrayBuffer}]`), `unsaved` (optional boolean) | Registers the fonts, then opens the document under `name`. File › Save writes it back in place, in PhotoCraft's own format. One document per app. `parts` are made into a bundle first; every object is checked against its content hash on the way in. With `unsaved: true` (a recovery copy) the document opens as unsaved changes: `dirty` is `true` from the start and only a save makes it saved. |
| `save` | | File › Save on that document, whichever tab is active. Answered with `document` or `save-failed`. |
| `saved` | `revision` | The host stored the document of that revision: it counts as saved (still unsaved if it was edited since). |
| `snapshot` | | Start a recovery copy now if the document has changes no copy holds yet (for a page going hidden). |
| `snapshot-reset` | | The host no longer holds the parts this app sent (its copy went, or a part could not be kept): the next copy sends every part again. |

A typical host:

1. waits for `ready`, then posts `open` (transferring the `ArrayBuffer`s);
2. keeps the last `dirty` and asks before its page is left while it is `true` (PhotoCraft's own
   "Leave site?" prompt is off in embed mode, so the user is asked once);
3. on its own Save button posts `save`, stores the `document` it gets, and posts `saved` with that
   document's `revision`. The user's own File › Save arrives as `document` the same way.

## Recovery copies

A browser does not always let a page ask before it goes: WebKit closes a tab whose page has not
answered within 50 ms without the "Leave site?" prompt, and a crash or a lost network asks
nothing. So while the document has unsaved changes, PhotoCraft posts `snapshot`s the host can keep
and hand back later:

- A copy starts once the changes have rested for 2 s (three times as long as the last copy's
  main-thread work, up to 15 s). A five-second pending-edit deadline keeps repeated edits from
  restarting that wait indefinitely; it is checked between pointer gestures, so a copy never
  starts during a drag. The host can ask for one at once (`snapshot`). A saved document gets none,
  and a revision is not copied twice.
- A copy never stalls the GUI. It is written a step per frame, about 6 ms of main-thread time
  (3 ms during a drag), against the document as it was at the copy's revision (editing goes on).
  Each step first hashes tiles and blobs whose hash the document's save cache does not know yet
  (an interrupted step keeps what it hashed), then compresses objects the host does not hold, one
  at a time, posting each as a `snapshot-part`. An object too large for one step gets a longer
  next step (doubled until it fits), so every copy completes; a blob over 1 MiB goes into its
  part uncompressed. `snapshot` closes the copy with the manifest and every path it names.
- Objects are content-addressed, so the host keeps each part as a record of its own: a small
  edit after a complete copy sends only the tiles it changed. The host keeps the newest complete
  copy per document and drops parts no copy names. It is never a save: `dirty` stays `true`.
- The host drops the copy once the document is saved (the app then forgets what the host held)
  or the user discards the changes. On the next open it offers it: `open` with the copy's
  `parts` and `unsaved: true` restores the work as unsaved changes, which the user then saves the
  normal way; the app counts the restored parts as held, so the next copy sends only changes.

## What embed mode changes

- Writes go only to the host's document. Save As under another name, Export As and Quick Export
  fail with a message; nothing is downloaded.
- File › Open, Place and drag and drop still read files from the user's computer.
- Fonts: the font menus offer only families from the host manifest (`fonts/manifest.txt`
  next to the page, see `packaging/web/README.md`, Fonts). While it is loading, or if it is
  missing or empty, no other families are offered. The bundled fonts
  stay installed: the interface and text already set in them still draw.
- The host's `fonts` in `open` are registered before the document opens, so its text lays out in
  them and the open names none of them missing.
- Page tabs: a document with artboards opens with one tab per artboard (Window › Pages), in page
  order (the artboards' order, bottom to top), titled with the artboard's name. Each page tab is
  a view of the one document: it is fitted to its artboard and draws only that artboard; new
  layers, Paste and Place from it land on its artboard. The document is still one document: `save`
  and the user's File › Save write it whole (the same bytes as without page tabs), `dirty` is the
  document's, and undo is shared. Closing a page tab hides that page's view only (Window › Pages
  shows it again); the last open page tab has no ×, so the host's document is never left without
  a view. A document without artboards shows its canvas in one tab, as before. Window › Pages ›
  Artboards as Tabs switches to the whole canvas and back.

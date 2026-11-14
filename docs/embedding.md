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
| `snapshot` | `revision`, `document` (an `ArrayBuffer`, transferred), `ms` (what writing it took) | A recovery copy of the document with its unsaved changes (below). |
| `snapshot-failed` | `error` | A recovery copy could not be written. |
| `start-failed` | `error` | The app could not start (the browser has neither WebGPU nor WebGL2). Nothing follows. |

Host → PhotoCraft (post to `iframe.contentWindow` with `location.origin` as the target origin):

| `type` | Fields | Effect |
|---|---|---|
| `open` | `name` (a plain `.pcraft` file name), `document` (`ArrayBuffer`), `fonts` (optional, `[{family, data: ArrayBuffer}]`), `unsaved` (optional boolean) | Registers the fonts, then opens the document under `name`. File › Save writes it back in place, in PhotoCraft's own format. One document per app. With `unsaved: true` (a recovery copy) the document opens as unsaved changes: `dirty` is `true` from the start and only a save makes it saved. |
| `save` | | File › Save on that document, whichever tab is active. Answered with `document` or `save-failed`. |
| `saved` | `revision` | The host stored the document of that revision: it counts as saved (still unsaved if it was edited since). |
| `snapshot` | | Post a `snapshot` now if the document has changes no snapshot holds yet (for a page going hidden). |

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

- A `snapshot` is posted once the changes have rested for 2 s (three times as long as the last
  snapshot took, up to 15 s), never during a pointer drag, never twice for one revision, and at
  once when the host posts `snapshot`. A saved document posts none.
- It is the whole document as a `.pcraft` without previews, written from the document's own save
  cache, so after the first one only the tiles changed since are compressed, and a File › Save
  after it reuses them. It is never a save: `dirty` stays `true`.
- The host keeps the newest one per document, drops it once the document is saved or the user
  discards the changes, and on the next open offers it: `open` with the copy and `unsaved: true`
  restores the work as unsaved changes, which the user then saves the normal way.

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

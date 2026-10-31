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

Host → PhotoCraft (post to `iframe.contentWindow` with `location.origin` as the target origin):

| `type` | Fields | Effect |
|---|---|---|
| `open` | `name` (a plain `.pcraft` file name), `document` (`ArrayBuffer`), `fonts` (optional, `[{family, data: ArrayBuffer}]`) | Registers the fonts, then opens the document under `name`. File › Save writes it back in place, in PhotoCraft's own format. One document per app. |
| `save` | | File › Save on that document, whichever tab is active. Answered with `document` or `save-failed`. |
| `saved` | `revision` | The host stored the document of that revision: it counts as saved (still unsaved if it was edited since). |

A typical host:

1. waits for `ready`, then posts `open` (transferring the `ArrayBuffer`s);
2. keeps the last `dirty` and asks before its page is left while it is `true` (PhotoCraft's own
   "Leave site?" prompt is off in embed mode, so the user is asked once);
3. on its own Save button posts `save`, stores the `document` it gets, and posts `saved` with that
   document's `revision`. The user's own File › Save arrives as `document` the same way.

## What embed mode changes

- Writes go only to the host's document. Save As under another name, Export As and Quick Export
  fail with a message; nothing is downloaded.
- File › Open, Place and drag and drop still read files from the user's computer.
- Fonts: when the host serves a font manifest (`fonts/manifest.txt` next to the page, see
  `packaging/web/README.md`, Fonts), the font menus offer only its families. The bundled fonts
  stay installed: the interface and text already set in them still draw.
- The host's `fonts` in `open` are registered before the document opens, so its text lays out in
  them and the open names none of them missing.

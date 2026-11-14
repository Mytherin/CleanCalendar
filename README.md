# CleanCalendar

A desktop calendar written in Rust that looks and behaves like Apple Calendar, with two-way Google Calendar sync.
Built on [egui/eframe](https://github.com/emilk/egui).

## Features

- **Day / Week / Month / Year** views with an Apple-style segmented control, a red "today" badge and a live current-time line.
- **Sidebar** with calendar visibility checkboxes, a right-click menu for color and delete, and a mini-month navigator.
- **Week/Day time grid:** overlapping events sit side by side; all-day and multi-day events span the all-day lane.
- **Direct manipulation:** drag on empty space to create an event, drag an event to move it, drag its bottom edge to resize. Everything snaps to 15 minutes.
- **Month view:** multi-day banners span across days, timed events show a colored dot and their time, and overflow collapses to "N more".
- **Event editor:** title, location, all-day, start/end date and time, calendar and notes.
- **Google Calendar:** OAuth sign-in in your browser (loopback + PKCE), imports all your calendars, and sends creates, edits, moves and deletes back to Google. Read-only calendars (holidays, shared calendars) are shown but can't be edited.
- Follows the system light/dark mode and uses the platform UI font (Segoe UI on Windows).

## Running

```sh
cargo run            # debug
cargo run --release  # optimized; no console window on Windows
cargo test
```

On Windows this project was set up with the `x86_64-pc-windows-gnu` toolchain plus WinLibs MinGW (for `dlltool`).
The MSVC toolchain works too if you have the Visual Studio C++ Build Tools.

## Keyboard shortcuts

| Keys | Action |
|---|---|
| Ctrl+1 / 2 / 3 / 4 | Day / Week / Month / Year |
| Ctrl+T | Go to today |
| Ctrl+N | New event |
| ← / → | Previous / next period |
| Delete / Backspace | Delete the selected event |
| Double-click | Open an event, or create one on empty space |
| Esc / Enter (in title) | Cancel / save in the editor |

## Connecting Google Calendar

Google requires every app to have its own OAuth client:

1. In [Google Cloud Console](https://console.cloud.google.com/), create a project and **enable the Google Calendar API**.
2. Configure the **OAuth consent screen** (External is fine) and add your Google account as a **test user**.
3. Go to **Credentials → Create credentials → OAuth client ID**, application type **Desktop app**.
4. In CleanCalendar click **Connect Google**, paste the client ID and secret, and click **Sign in with Google**.

### How sync works

- **Your changes** (create, edit, move, resize, delete, change calendar) are uploaded right away and marked *pending* until Google confirms them.
  If Google can't be reached, they stay pending and are retried every minute. The toolbar shows "N changes not yet on Google".
  Pending changes survive restarts, and a sync never overwrites an event that has unsent edits.
- **Changes from elsewhere** (phone, web, other people) are pulled on launch, every 5 minutes, and when you press ⟳.
  After the first full download, each calendar syncs incrementally using Google's sync tokens, so only changed events are fetched.
  A full re-download runs weekly, when Google expires a token, or after Google rejects a change.
  The full download covers 6 months back to 12 months ahead.
- **Conflicts:**
  - If you edit an event that someone else deleted on Google, their delete wins.
  - If Google rejects an edit (for example, no permission), the app goes back to Google's version.
  - Recurring events are edited one occurrence at a time.

## Data

Everything is stored as JSON in `%APPDATA%\CleanCalendar\CleanCalendar\data\store.json` on Windows (`~/Library/Application Support/...` on macOS, `~/.local/share/...` on Linux).
Set `CLEANCALENDAR_DATA=<dir>` to use a different folder.

> **Note:** the Google refresh token is stored in that file in plain text. Moving it into the OS keychain (e.g. with the `keyring` crate) is a good next step.

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Window setup |
| `src/app.rs` | App state, toolbar, sidebar, event editor, Google dialog, sync merge |
| `src/views.rs` | Month grid, day/week time grid (incl. drag interactions), year view, mini-month |
| `src/sync.rs` | Sync bookkeeping (no UI, no I/O): what to upload, retries, merging Google's changes |
| `src/google.rs` | OAuth flow, Calendar v3 REST client, background worker thread |
| `src/model.rs` | `Calendar`, `Event`, and the JSON `Store` |
| `src/theme.rs` | Apple-like palette for light and dark mode, fonts |
| `src/dates.rs` | Date helpers |

## Tests

`cargo test` runs everything offline:

- **`src/sync.rs`** replays sync scenarios step by step against the `SyncEngine`: offline edits retried until confirmed, editing during an upload, deleting before the first upload finishes, events deleted on Google, rejected edits, weekly full syncs, overlapping sync requests.
- **`src/google/http_tests.rs`** runs the real HTTP client against a scripted local fake server. It covers access-token caching, paging, full vs. incremental (sync-token) requests, falling back to a full sync on `410 Gone`, percent-encoded calendar ids, POST vs. PATCH bodies, all-day dates, and how errors are classified.
- Unit tests for dates, the week-view layout, and parsing Google's JSON.

Nothing here talks to Google. Sign-in and live sync still need a manual or live check against a real account.

## Ideas for next steps

- Recurring events created locally (Google recurring events already show, expanded into instances)
- Search, event alerts/notifications, and an inspector-style popover next to the event
- Editing a whole recurring series ("this and all future events")
- Keychain storage for tokens; additional providers (iCloud via CalDAV, Outlook)

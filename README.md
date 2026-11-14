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

After sign-in the app syncs on launch and every 5 minutes, and you can press ⟳ to sync immediately.
The sync window is 6 months back to 12 months ahead.

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
| `src/google.rs` | OAuth flow, Calendar v3 REST client, background sync worker thread |
| `src/model.rs` | `Calendar`, `Event`, and the JSON `Store` |
| `src/theme.rs` | Apple-like palette for light and dark mode, fonts |
| `src/dates.rs` | Date helpers |

## Ideas for next steps

- Recurring events created locally (Google recurring events already show, expanded into instances)
- Search, event alerts/notifications, and an inspector-style popover next to the event
- Incremental sync with Google `syncToken`s instead of re-fetching the window
- Keychain storage for tokens; additional providers (iCloud via CalDAV, Outlook)

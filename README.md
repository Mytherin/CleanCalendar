# CleanCalendar

A desktop calendar written in Rust that looks and behaves like Apple Calendar, with two-way Google Calendar sync.
Built on [egui/eframe](https://github.com/emilk/egui).

## Features

- **Day / Week / Month / Year** views with an Apple-style segmented control, a red "today" badge and a live current-time line.
- **Sidebar** with calendar visibility checkboxes, a right-click menu for color and delete, and a mini-month navigator.
- **Week/Day time grid:** overlapping events sit side by side; all-day and multi-day events span the all-day lane.
- **Direct manipulation:** drag on empty space to create an event, drag an event to move it, drag its bottom edge to resize. Everything snaps to 15 minutes.
- **Right-click menu** like Apple Calendar's: Get Info, move to another calendar, Delete, Cut, Copy, Paste, Duplicate, go to the next/previous occurrence of a repeating event, and e-mail the event (or its series). Right-click on empty space to create an event or paste there.
- **Month view:** multi-day banners span across days, timed events show a colored dot and their time, and overflow collapses to "N more".
- **Event editor:** title, location, all-day, start/end date and time, time zone, repeat, calendar, invitees and notes.
- **Repeating events:** every N days, weeks (on chosen weekdays), months or years, ending never, on a date, or after a number of times. Edit or delete one occurrence or the whole series; dragging an occurrence moves just that one.
- **Time zones:** pin an event to a zone (“09:00 New York”) and it shows at the right local time wherever you are, also across daylight-saving changes; or leave it *floating* to keep the same wall-clock time everywhere.
- **Settings** (⚙ in the toolbar, or Ctrl+,), laid out like macOS Calendar's:
  - *General:* 5 or 7 days per week, the first day of the week (or the Windows region setting), scrolling the week view by week or by day, working hours (shaded outside), how many hours fit on screen, the default event length, the default calendar, and whether to show Google's birthdays and holidays calendars.
  - *Accounts:* add (+) or remove (−) the Google account, switch it off, give it a name, and choose how often it refreshes (every minute to hourly, or manually).
  - *Advanced:* shade busy days in the year view, and show week numbers in the week and month views.
- **Invitees:** on Google calendars, type addresses into "Add Invitees" (Enter, comma or semicolon adds one). Google emails the invitations, updates and cancellations, and each guest's reply (accepted, maybe, declined, no response) is shown next to their name.
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
| Ctrl+, | Settings |
| Ctrl+N | New event |
| ← / → | Previous / next period |
| Delete / Backspace | Delete the selected event |
| Ctrl+X / C / V | Cut / copy the selected event, paste it (onto the selected event's day, at its own time) |
| Ctrl+D | Duplicate the selected event |
| Ctrl+] / Ctrl+[ | Next / previous occurrence of a repeating event |
| Double-click | Open an event, or create one on empty space |
| Right-click | The event's menu, or New Event / Paste on empty space |
| Esc / Enter (in title) | Cancel / save in the editor |

## Connecting Google Calendar

Google requires every app to have its own OAuth client:

1. In [Google Cloud Console](https://console.cloud.google.com/), create a project and **enable the Google Calendar API**.
2. Configure the **OAuth consent screen** (External is fine) and add your Google account as a **test user**.
3. Go to **Credentials → Create credentials → OAuth client ID**, application type **Desktop app**.
4. In CleanCalendar open **Settings → Accounts**, click **+**, choose Google, paste the client ID and secret, and click **Sign in with Google**.

### How sync works

- **Your changes** (create, edit, move, resize, delete, change calendar) are uploaded right away and marked *pending* until Google confirms them.
  If Google can't be reached, they stay pending and are retried every minute. The toolbar shows "N changes not yet on Google".
  Pending changes survive restarts, and a sync never overwrites an event that has unsent edits.
- **Changes from elsewhere** (phone, web, other people) are pulled on launch, every 5 minutes (change it under Settings → Accounts → Refresh Calendars), and when you press ⟳.
  After the first full download, each calendar syncs incrementally using Google's sync tokens, so only changed events are fetched.
  A full re-download runs weekly, when Google expires a token, or after Google rejects a change.
  The full download covers 6 months back to 12 months ahead.
- **Conflicts:** every upload says which version of the event it was based on (Google's `etag`), so an edit can never silently overwrite a newer change made elsewhere. Instead:
  - **Both sides edited the event:** the edits are merged field by field (title, location, notes, time). If both changed the same field, your edit wins and the app tells you.
  - **An edit always beats a delete.** If you edit an event that was deleted elsewhere, your version is recreated. If you delete an event that was edited elsewhere, it isn't deleted and comes back with their change.
  - **A retried create can't duplicate an event:** new events get their Google id up front, so a retry after a lost reply finds the event already there.
  - If Google rejects an edit (for example, no permission), the app goes back to Google's version.
  - Repeating events made here are uploaded as a series (`RRULE`) with their time zone; Google then sends them back as separate occurrences, which replace the local copy.
  - Google’s repeating events are edited one occurrence at a time; “Delete All Events” deletes the whole series.

## Data

Data is kept in two SQLite databases, split by who owns it:

- **`data.db`: things only this computer has.** Your settings, Google sign-in settings (client id, account), the calendar list (with your colours and which are shown), events on local calendars, and changes to Google events that haven't been uploaded yet. Lives in `%APPDATA%\CleanCalendar\` on Windows (`~/Library/Application Support/CleanCalendar/` on macOS, `~/.local/share/cleancalendar/` on Linux).
- **`cache.db`: a copy of Google.** Google events as last downloaded, plus the sync tokens. Lives in `%LOCALAPPDATA%\CleanCalendar\` on Windows (`~/Library/Caches/CleanCalendar/` on macOS, `~/.cache/cleancalendar/` on Linux), so it isn't carried around in a roaming profile. It is safe to delete: the next sync downloads everything again. If it can't be read, it is started over automatically.
- **The Google refresh token** is kept in the OS credential store (Windows Credential Manager, macOS Keychain, Secret Service on Linux) under `CleanCalendar` / `google-refresh-token`, not in either file. If no credential store is available it falls back to `data.db` (with a warning), and moves into the credential store once one works.

The cache is attached to the same connection, so each save commits to both files at once. Only rows that changed are written. A `store.json` from an older version is imported on first start and renamed to `store.json.migrated`. Files in the folders older versions used (`%APPDATA%\CleanCalendar\CleanCalendar\data\`, `~/Library/Application Support/com.CleanCalendar.CleanCalendar/`) are moved over on first start.

Set `CLEANCALENDAR_DATA=<dir>` to put both files in one folder of your choice (the token still goes to the credential store, under an entry named after that folder).
Set `CLEANCALENDAR_START=month:2026-11-14` (or just `day`, `week`, `month`, `year`) to open on a given view and date, which is handy together with `CLEANCALENDAR_DATA` for screenshots and testing by hand.

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Window setup |
| `src/app.rs` | App state, toolbar, sidebar, event editor, sync merge |
| `src/app/settings_window.rs` | The Settings window (General, Accounts with the add-account sheet, Advanced) |
| `src/settings.rs` | The user's preferences and their defaults |
| `src/views.rs` | Painting: month grid, day/week time grid, year view, mini-month |
| `src/layout.rs` | Pure geometry: where events and cells go, and what each screen area means to the pointer |
| `src/interact.rs` | Pointer interaction (clicks, double-clicks, drags) as a testable state machine |
| `src/sync.rs` | Sync bookkeeping (no UI, no I/O): what to upload, retries, merging Google's changes |
| `src/google.rs` | OAuth flow, Calendar v3 REST client, background worker thread |
| `src/model.rs` | `Calendar`, `Event`, and the in-memory `Store` |
| `src/recurrence.rs` | Repeat rules, expanding a series into occurrences, time zone conversion, `RRULE` |
| `src/db.rs` | SQLite storage: persistent `data.db` and disposable `cache.db` |
| `src/theme.rs` | Apple-like palette for light and dark mode, fonts |
| `src/dates.rs` | Date helpers |

## Tests

There are three levels of tests.

**1. Offline: `cargo test`.** Runs everything that needs no network:
- **`src/sync.rs`** replays sync scenarios step by step against the `SyncEngine`:
  - offline edits retried until confirmed, editing during an upload, deleting before the first upload finishes;
  - conflicts: merging, same-field clashes, edit vs. delete in both directions;
  - rejected edits, weekly full syncs, overlapping sync requests.
- **`src/google/http_tests.rs`** runs the real HTTP client against a scripted local fake server:
  - access-token caching, paging, and full vs. incremental (sync-token) requests, including the fallback to a full sync on `410 Gone`;
  - `If-Match` and `412` conflicts, and duplicate-proof creates;
  - percent-encoded calendar ids, request bodies, and how errors are classified.
- **`src/interact.rs` and `src/layout.rs`** drive the pointer state machine against real layouts.
- Unit tests for dates and for parsing Google's JSON.

**2. Against a Google emulator.** These are `#[ignore]`d by default. They run the real sign-in flow (with a script standing in for the browser), the HTTP client and the sync engine against [`emulate`](https://www.npmjs.com/package/emulate):

```sh
npx emulate@0.13.1 --service google --seed testdata/emulate-seed.yaml   # listens on 127.0.0.1:4000
CLEANCALENDAR_GOOGLE_URL=http://127.0.0.1:4000 cargo test emulator_tests -- --ignored
```

The emulator can't update events (`PATCH`) or issue sync tokens, so edits and conflicts are covered by the other two levels. A canary test fails once the emulator gains `PATCH`.

You can also point the app itself at the emulator: run it with `CLEANCALENDAR_GOOGLE_URL=http://127.0.0.1:4000` and a separate `CLEANCALENDAR_DATA` folder, then sign in as the seeded user.

**3. Against real Google.** These are `#[ignore]`d too. They play two copies of the app ("laptop" and "phone") against one account to create real conflicts. Each test works in a temporary calendar that it deletes afterwards. You need an OAuth client of type *Desktop app*, and a Google account that is a test user of it. Keep the credentials out of the repo:

```sh
export CLEANCALENDAR_LIVE_CLIENT_ID=...  CLEANCALENDAR_LIVE_CLIENT_SECRET=...
# once: opens the browser to approve access, saves the token to the file
CLEANCALENDAR_LIVE_TOKEN_FILE=/path/outside/repo cargo test live_sign_in -- --ignored --nocapture
export CLEANCALENDAR_LIVE_REFRESH_TOKEN=$(cat /path/outside/repo)
cargo test live_tests -- --ignored --test-threads 1 --skip live_sign_in
```

## Ideas for next steps

- Recurring events created locally (Google recurring events already show, expanded into instances)
- Search, event alerts/notifications, and an inspector-style popover next to the event
- Editing the rule of a series that came from Google, and “this and all future events”
- Monthly rules like “the second Tuesday”
- Keychain storage for tokens; additional providers (iCloud via CalDAV, Outlook)

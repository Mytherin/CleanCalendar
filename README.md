# CleanCalendar

A desktop calendar written in Rust that looks and behaves like Apple Calendar, with two-way sync for iCloud (and any other CalDAV server) and Google Calendar.
Built on [egui/eframe](https://github.com/emilk/egui).

## Features

- **Day / Week / Month / Year** views with an Apple-style segmented control, a red "today" badge and a live current-time line.
- **Sidebar** with calendar visibility checkboxes, a right-click menu for color and delete, and a mini-month navigator.
- **Week/Day time grid:** overlapping events sit side by side; all-day and multi-day events span the all-day lane.
- **Direct manipulation:** drag on empty space to create an event, drag an event to move it, drag its bottom edge to resize. Everything snaps to 15 minutes.
- **Right-click menu** like Apple Calendar's: Get Info, move to another calendar, Delete, Cut, Copy, Paste, Duplicate, go to the next/previous occurrence of a repeating event, and e-mail the event (or its series). Right-click on empty space to create an event or paste there.
- **Month view:** multi-day banners span across days, timed events show a colored dot and their time, and overflow collapses to "N more".
- **Search** (Ctrl+F, or the magnifier at the right of the toolbar), like Apple Calendar's: matches every word of the query against titles, locations, notes and invitees on the shown calendars, and lists the results (every occurrence of a repeating event) beside the calendar, scrolled to what's coming up. Click a result to go to it, double-click to open it, Enter goes to the next upcoming one, Esc clears the search.
- **Event editor:** title, location, all-day, start/end date and time, time zone, repeat, calendar, invitees and notes.
- **Location suggestions, offline:** add the countries you want under Settings → Locations, and typing a location suggests towns, streets and named places (shops, museums, stations, …). A typed house number goes with the street ("Damrak 70"); arrow keys and Enter pick a suggestion. The map pin next to the location opens it in Google Maps. Searches never leave your computer: each country is a small prebuilt index of OpenStreetMap data (7 MB for the Netherlands), downloaded once from this project's GitHub release. With no countries installed, the location field offers to download some.
- **Repeating events:** every N days, weeks (on chosen weekdays), months or years, ending never, on a date, or after a number of times. Edit or delete one occurrence or the whole series; dragging an occurrence moves just that one.
- **Time zones:** pin an event to a zone (“09:00 New York”) and it shows at the right local time wherever you are, also across daylight-saving changes; or leave it *floating* to keep the same wall-clock time everywhere.
- **Settings** (⚙ in the toolbar, or Ctrl+, — ⌘, on macOS), laid out like macOS Calendar's:
  - *General:* 5 or 7 days per week, the first day of the week (or the system's region setting), scrolling the week view by week or by day, working hours (shaded outside), how many hours fit on screen, the default event length, the default calendar, and whether to show Google's birthdays and holidays calendars.
  - *Accounts:* add (+) or remove (−) the Google account, switch it off, give it a name, and choose how often it refreshes (every minute to hourly, or manually).
  - *Locations:* the countries whose location data is installed (update or remove each), downloads in progress, and a search for countries to add.
  - *Advanced:* shade busy days in the year view, show week numbers in the week and month views, and how much of the past to keep from synced calendars (all of it by default, or the last 5 years, 2 years, 1 year or 6 months).
- **Invitees:** on Google calendars, type addresses into "Add Invitees" (Enter, comma or semicolon adds one). Google emails the invitations, updates and cancellations, and each guest's reply (accepted, maybe, declined, no response) is shown next to their name.
- **iCloud and CalDAV** (Settings → Accounts → +): sign in with your Apple ID and an app-specific password (or any CalDAV server: Fastmail, Nextcloud, Radicale, …). All your calendars are imported, Reminders lists are skipped, and creates, edits, moves and deletes go back to the server. Repeating events from other apps show every occurrence, whatever their rule.
- **Several accounts at once:** iCloud, other CalDAV servers and Google side by side, each in its own group in the sidebar.
- **Google Calendar:** OAuth sign-in in your browser (loopback + PKCE), imports all your calendars, and sends creates, edits, moves and deletes back to Google. Read-only calendars (holidays, shared calendars) are shown but can't be edited.
- Follows the system light/dark mode and uses the platform UI font (Segoe UI on Windows, San Francisco on macOS, DejaVu Sans on Linux). The window, taskbar and tray icons (Windows) or Dock icon (macOS) show today's date.

## Running

```sh
cargo run            # debug
cargo run --release  # optimized; no console window on Windows
cargo test
```

On Windows this project was set up with the `x86_64-pc-windows-gnu` toolchain plus WinLibs MinGW (for `dlltool`).
The MSVC toolchain works too if you have the Visual Studio C++ Build Tools.

The executable is `target/release/CleanCalendar.exe`. On Windows, `build.rs` embeds `assets/CleanCalendar.ico` and version info into it (no `rc.exe`/`windres` needed). The icon is rendered by the same code as the live date icon; to regenerate it run `cargo test date_icon::tests::write_exe_icon -- --ignored`.

## Keyboard shortcuts

| Keys | Action |
|---|---|
| Ctrl+1 / 2 / 3 / 4 | Day / Week / Month / Year |
| Ctrl+T | Go to today |
| Ctrl+F | Search |
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

## Connecting iCloud

1. At [account.apple.com](https://account.apple.com/account/manage) go to **Sign-In and Security → App-Specific Passwords** and create one (e.g. "CleanCalendar"). Apple doesn't let other apps use your normal Apple ID password.
2. In CleanCalendar open **Settings** (⚙) → **Accounts**, click **+**, choose **iCloud**, enter your Apple ID and the app-specific password, and click **Sign In**.

For another CalDAV server choose **Other CalDAV server** and enter its address (e.g. `https://caldav.fastmail.com/`); the calendars are found from there (`/.well-known/caldav`, the user's principal, then the calendar home). The password is kept in the OS credential store.

## Connecting Google Calendar

In CleanCalendar open **Settings → Accounts**, click **+**, choose Google, and click **Sign in with Google**. Your browser opens on Google's sign-in page; once you allow access, CleanCalendar gets a token for your calendars. It never sees your Google password, and you can revoke access any time under **Security → Third-party apps** in your Google Account. Google doesn't let calendar apps sign in with a password, so there's no username/password form like iCloud's.

CleanCalendar ships its own OAuth client (type *Desktop app*). For an installed app Google treats the client secret as public; PKCE and the loopback redirect are what protect the sign-in. To use your own client instead, expand **Use your own OAuth client** on the sign-in sheet and paste its ID and secret, or build with `CLEANCALENDAR_GOOGLE_CLIENT_ID` and `CLEANCALENDAR_GOOGLE_CLIENT_SECRET` set. To make one: in [Google Cloud Console](https://console.cloud.google.com/) create a project, enable the **Google Calendar API**, configure the **OAuth consent screen**, and create **Credentials → OAuth client ID** of type **Desktop app**.

Google keeps using Google's own REST API rather than its CalDAV endpoint: it's richer (colours, sync tokens, structured JSON) and needs the same OAuth sign-in anyway.

### How sync works

Every account, whatever the service, goes through the same sync engine (`src/sync.rs`) and worker (`src/remote.rs`); a service only lists calendars and events, and saves, fetches and deletes one event (`Remote` in `src/remote.rs`). So everything below holds for iCloud, other CalDAV servers and Google alike.

- **Your changes** (create, edit, move, resize, delete, change calendar) are uploaded right away and marked *pending* until the server confirms them.
  If the server can't be reached, they stay pending and are retried every minute. The toolbar shows "N changes not yet synced".
  Pending changes survive restarts, and a sync never overwrites an event that has unsent edits.
- **Changes from elsewhere** (phone, web, other people) are pulled on launch, every 5 minutes (change it under Settings → Accounts → Refresh Calendars), and when you press ⟳.
  After the first full download, each calendar syncs incrementally (Google's sync tokens; CalDAV's `sync-collection` report, RFC 6578), so only changed events are fetched.
  A full re-download runs weekly, when the server expires a token, or after it rejects a change.
  The full download covers all past events (or as far back as **Settings → Advanced → Keep past events** says) up to the end of the 13th month ahead; repeating events are kept as their occurrences, so they need an end. Changing that setting downloads everything again.
- **Conflicts:** every upload says which version of the event it was based on (its `etag`, sent as `If-Match`), so an edit can never silently overwrite a newer change made elsewhere. Instead:
  - **Both sides edited the event:** the edits are merged field by field (title, location, notes, time). If both changed the same field, your edit wins and the app tells you.
  - **An edit always beats a delete.** If you edit an event that was deleted elsewhere, your version is recreated. If you delete an event that was edited elsewhere, it isn't deleted and comes back with their change.
  - **A retried create can't duplicate an event:** new events get their id up front (Google's event id; CalDAV's resource name and `If-None-Match: *`), so a retry after a lost reply finds the event already there.
  - If the server rejects an edit (for example, a read-only calendar), the app goes back to the server's version.
  - Repeating events made here are uploaded as a series (`RRULE`) with their time zone; they then come back as separate occurrences, which replace the local copy.
  - Repeating events from a server are edited one occurrence at a time; “Delete All Events” deletes the whole series.
- **CalDAV specifics:** a CalDAV event is a whole `.ics` file. Saving reads the server's copy first and only replaces what the app owns (title, location, notes, times, repeat rule), so alarms, attendees and other apps' properties are kept. Editing one occurrence of a series writes a `RECURRENCE-ID` override; deleting one adds an `EXDATE`. Series are expanded into occurrences here (with the [`rrule`](https://crates.io/crates/rrule) crate), so any RFC 5545 rule shows correctly. Pinned time zones are written as `TZID`s with a `VTIMEZONE` generated from the zone database.

## Data

Data is kept in two SQLite databases, split by who owns it:

- **`data.db`: things only this computer has.** Your settings, account settings (Google client id and account, CalDAV servers and user names), the calendar list (with your colours and which are shown), events on local calendars, and changes to synced events that haven't been uploaded yet. Lives in `%APPDATA%\CleanCalendar\` on Windows (`~/Library/Application Support/CleanCalendar/` on macOS, `~/.local/share/cleancalendar/` on Linux).
- **`cache.db`: a copy of the servers.** Synced events as last downloaded, plus the sync tokens. Lives in `%LOCALAPPDATA%\CleanCalendar\` on Windows (`~/Library/Caches/CleanCalendar/` on macOS, `~/.cache/cleancalendar/` on Linux), so it isn't carried around in a roaming profile. It is safe to delete: the next sync downloads everything again. If it can't be read, it is started over automatically.
- **Secrets** (the Google refresh token, CalDAV passwords) are kept in the OS credential store (Windows Credential Manager, macOS Keychain, Secret Service on Linux) under `CleanCalendar` / `google-refresh-token` and `CleanCalendar` / `caldav-<id>-password`, not in either file. If no credential store is available they fall back to `data.db` (with a warning), and move into the credential store once one works. Disconnecting an account deletes its secret.

The cache is attached to the same connection, so each save commits to both files at once. Only rows that changed are written. A `store.json` from an older version is imported on first start and renamed to `store.json.migrated`. Files in the folders older versions used (`%APPDATA%\CleanCalendar\CleanCalendar\data\`, `~/Library/Application Support/com.CleanCalendar.CleanCalendar/`) are moved over on first start.

Set `CLEANCALENDAR_DATA=<dir>` to put both files in one folder of your choice (the token still goes to the credential store, under an entry named after that folder).
Set `CLEANCALENDAR_START=month:2026-11-14` (or just `day`, `week`, `month`, `year`) to open on a given view and date, which is handy together with `CLEANCALENDAR_DATA` for screenshots and testing by hand.
Set `CLEANCALENDAR_SEARCH=dentist` to open with that search already typed in.

Installed location data (one `<country>.places` file per country) lives in a `places` folder next to `cache.db` (or in `CLEANCALENDAR_DATA`), and can be deleted at any time.

## Location data

The countries offered under Settings → Locations are prebuilt indexes attached to a GitHub release (`v0.1`). The list of them (file, size, date and SHA-256 of each) is built into the app from `src/places/countries.json`, so the app downloads only the index of each country it adds, checks it against that checksum, and searches it locally. `CLEANCALENDAR_PLACES_URL=<url>` points the app at another copy of the files (e.g. a local test server).

An index is built from Geofabrik's OpenStreetMap extract of the country: towns and villages, streets, and named places (shops, museums, stations, offices, …), each placed in its nearest town. It is stored by column (kind, name, street, house number, town), each dictionary-encoded and zstd-compressed, with an [fst](https://docs.rs/fst) map from every search word to its entries. The Netherlands' 1.4 GB extract becomes a 7 MB index of 546,000 entries; a search takes well under a millisecond.

To build or refresh the indexes (the OpenStreetMap reader is only compiled with the `build-places` feature, so it isn't in the app):

```sh
cargo run --release --features build-places -- --build-places places-out             # every country
cargo run --release --features build-places -- --build-places places-out netherlands # some
cargo run --release --features build-places -- --build-places --list                 # country ids
```

Each country's extract is downloaded, indexed and deleted again, and `places-out/manifest.json` is updated after each one; countries already built are skipped unless `--force` is given, so an interrupted run carries on where it stopped. Then attach the indexes to the release (replacing the old files), and build the new list into the app:

```sh
gh release upload v0.1 places-out/*.places --clobber
cp places-out/manifest.json src/places/countries.json
```

Upload before shipping the new list: an app whose list names a file or checksum the release doesn't have can't install that country. (`cargo test live_release -- --ignored` installs Monaco from the real release; set `CLEANCALENDAR_TEST_COUNTRY` for another.)

Map data © OpenStreetMap contributors, under the [Open Database License](https://opendatacommons.org/licenses/odbl/); the app shows this credit under its suggestions.

## Code layout

| File | Purpose |
|---|---|
| `src/main.rs` | Window setup |
| `src/app.rs` | App state, toolbar, sidebar, event editor, Accounts window, sync glue |
| `src/app/settings_window.rs` | The Settings window (General, Accounts with the add-account sheet, Locations, Advanced) |
| `src/app/search_ui.rs` | The toolbar's search field and the results list |
| `src/search.rs` | Searching events (no UI) |
| `src/editor.rs` | The event editor, including the location field's suggestions and Google Maps pin |
| `src/places.rs` | Location data: installed countries, downloads from the release, searching, the editor's lookup |
| `src/places/index.rs` | The compressed per-country index: writing, loading, prefix search and ranking |
| `src/places/osm.rs`, `src/places/build.rs` | `--build-places` only: reading OpenStreetMap extracts and building the published indexes |
| `src/settings.rs` | The user's preferences and their defaults |
| `src/views.rs` | Painting: month grid, day/week time grid, year view, mini-month |
| `src/layout.rs` | Pure geometry: where events and cells go, and what each screen area means to the pointer |
| `src/interact.rs` | Pointer interaction (clicks, double-clicks, drags) as a testable state machine |
| `src/sync.rs` | Sync bookkeeping (no UI, no I/O), shared by all services: what to upload, retries, merging changes |
| `src/remote.rs` | What all services share: the `Remote` trait, commands and replies, the background worker, conflict and token-expiry handling |
| `src/google.rs` | Google: OAuth flow and the Calendar v3 REST client |
| `src/caldav.rs` | CalDAV (iCloud and others): discovery and the WebDAV client |
| `src/caldav/ical.rs` | iCalendar: parsing and writing, series expansion, overrides, `VTIMEZONE` |
| `src/caldav/xml.rs` | WebDAV XML: multistatus responses and request bodies |
| `src/model.rs` | `Calendar`, `Event`, and the in-memory `Store` |
| `src/recurrence.rs` | Repeat rules, expanding a series into occurrences, time zone conversion, `RRULE` |
| `src/db.rs` | SQLite storage: persistent `data.db` and disposable `cache.db` |
| `src/theme.rs` | Apple-like palette for light and dark mode, fonts |
| `src/dates.rs` | Date helpers |

## Tests

There are four levels of tests.

**1. Offline: `cargo test`.** Runs everything that needs no network:
- **`src/sync.rs`** replays sync scenarios step by step against the `SyncEngine`:
  - offline edits retried until confirmed, editing during an upload, deleting before the first upload finishes;
  - conflicts: merging, same-field clashes, edit vs. delete in both directions;
  - rejected edits, weekly full syncs, overlapping sync requests.
- **`src/google/http_tests.rs`** runs the real HTTP client against a scripted local fake server:
  - access-token caching, paging, and full vs. incremental (sync-token) requests, including the fallback to a full sync on `410 Gone`;
  - `If-Match` and `412` conflicts, and duplicate-proof creates;
  - percent-encoded calendar ids, request bodies, and how errors are classified.
- **`src/caldav/tests.rs`** runs the CalDAV client and the sync engine against an in-memory CalDAV server (`src/caldav/fake.rs`) shaped like iCloud: discovery on one host with the calendar home on another, Basic auth, a Reminders list and a read-only calendar to skip or respect, expiring sync tokens, `If-Match`/`If-None-Match` preconditions, and replies without an `ETag`. Two "devices" (laptop and phone, each with its own store and engine) share the account, covering merges, edit vs. delete, offline changes, moving between calendars, overrides and `EXDATE`s on series, and properties the app doesn't show surviving an edit.
- **`src/caldav/ical/tests.rs`** covers iCalendar parsing and folding, series expansion (including rules like "the second Tuesday"), daylight saving, overrides, and generated `VTIMEZONE`s.
- **`src/remote.rs`** tests the shared worker logic against a scripted service; `src/sync.rs` has tests with Google and CalDAV accounts side by side.
- **`src/interact.rs` and `src/layout.rs`** drive the pointer state machine against real layouts.
- Unit tests for dates, WebDAV XML, and parsing Google's JSON.

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

**4. Against a real CalDAV server.** `src/caldav/server_tests.rs` (also `#[ignore]`d) plays the laptop/phone scenarios against a real server, each in a temporary calendar it creates with `MKCALENDAR` and deletes afterwards. With [Radicale](https://radicale.org/) locally:

```sh
pip install radicale
python -m radicale --storage-filesystem-folder=/tmp/radicale --auth-type=none --server-hosts=127.0.0.1:5232
CLEANCALENDAR_CALDAV_URL=http://127.0.0.1:5232/ CLEANCALENDAR_CALDAV_USER=tester CLEANCALENDAR_CALDAV_PASSWORD=x \
    cargo test caldav::server_tests -- --ignored --test-threads 1
```

Or against iCloud: `CLEANCALENDAR_CALDAV_URL=https://caldav.icloud.com/`, your Apple ID, and an app-specific password (never commit it).

## Ideas for next steps

- Search, event alerts/notifications, and an inspector-style popover next to the event
- Editing the rule of a series that came from a server, and “this and all future events”
- Monthly rules like “the second Tuesday” in the editor (they already show when they come from a server)
- Outlook / Microsoft 365

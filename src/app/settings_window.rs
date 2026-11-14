//! The Settings window, laid out like macOS Calendar's: a row of tabs with
//! icons (General, Accounts, Advanced), forms with right-aligned labels, and
//! an "Add Account" sheet that asks for the provider first.

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, FontId, Key, Layout, Order, Painter, Pos2, RichText, Sense, Stroke,
    TextFormat, Ui, Vec2,
};

use super::App;
use crate::dates::{fmt_hour, CLOCK_24H};
use crate::editor::Outcome;
use crate::caldav;
use crate::model::{CalDavAccount, Event, Store};
use crate::remote::{Cmd, GOOGLE};
use crate::settings::{
    self, duration_label, history_label, refresh_label, weekday_from_monday, weekday_name, Settings, WeekScroll,
    DURATION_CHOICES, HISTORY_CHOICES, REFRESH_CHOICES,
};
use crate::theme::{self, Palette, ACCENT, RED};

const WINDOW_W: f32 = 560.0;
/// Width of the right-aligned label column in the General tab.
const LABEL_W: f32 = 200.0;
const COMBO_W: f32 = 230.0;
const ROW_H: f32 = 26.0;
const GOOGLE_BLUE: Color32 = Color32::from_rgb(0x42, 0x85, 0xF4);
/// The "Add Account" sheet, and the label column of its forms.
const SHEET_W: f32 = 460.0;
const SHEET_LABEL_W: f32 = 130.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    General,
    Accounts,
    Advanced,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::General, Tab::Accounts, Tab::Advanced];

    fn title(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Accounts => "Accounts",
            Tab::Advanced => "Advanced",
        }
    }
}

/// The providers on the "Add Account" sheet, in its order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Provider {
    ICloud,
    Exchange,
    Google,
    Yahoo,
    Aol,
    /// Any other CalDAV server.
    CalDav,
}

impl Provider {
    const ALL: [Provider; 6] =
        [Provider::ICloud, Provider::Exchange, Provider::Google, Provider::Yahoo, Provider::Aol, Provider::CalDav];

    /// Where a CalDAV service's discovery starts (`None`: typed in, or not CalDAV).
    fn server(self) -> Option<&'static str> {
        match self {
            Provider::ICloud => Some(caldav::ICLOUD),
            Provider::Yahoo => Some(YAHOO),
            Provider::Aol => Some(AOL),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Provider::ICloud => "iCloud",
            Provider::Exchange => "Microsoft Exchange",
            Provider::Google => "Google",
            Provider::Yahoo => "Yahoo",
            Provider::Aol => "AOL",
            Provider::CalDav => "CalDAV",
        }
    }

    /// Which service a CalDAV account is on.
    fn of(account: &CalDavAccount) -> Provider {
        Provider::of_server(&account.server)
    }

    /// Which service a CalDAV server address belongs to.
    fn of_server(server: &str) -> Provider {
        [Provider::ICloud, Provider::Yahoo, Provider::Aol]
            .into_iter()
            .find(|p| p.server() == Some(server))
            .unwrap_or(Provider::CalDav)
    }
}

const YAHOO: &str = "https://caldav.calendar.yahoo.com/";
const AOL: &str = "https://caldav.aol.com/";

/// The steps of the "Add Account" sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddStep {
    /// "Choose a Calendar account provider…"
    Provider(Option<Provider>),
    /// The chosen provider's sign-in.
    SignIn(Provider),
}

/// "Account Type" on the "Add a CalDAV account" sheet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CalDavKind {
    /// The server is found from the email address.
    #[default]
    Automatic,
    /// A server address.
    Manual,
    /// Server address, path and port.
    Advanced,
}

/// What's typed into a CalDAV sign-in.
struct CalDavForm {
    kind: CalDavKind,
    username: String,
    password: String,
    server: String,
    path: String,
    port: String,
    ssl: bool,
    /// The account being signed in, until it's added (or fails).
    pending: Option<String>,
}

impl Default for CalDavForm {
    fn default() -> Self {
        CalDavForm {
            kind: CalDavKind::Automatic,
            username: String::new(),
            password: String::new(),
            server: String::new(),
            path: String::new(),
            port: String::new(),
            ssl: true,
            pending: None,
        }
    }
}

impl CalDavForm {
    /// Where discovery starts. `Err("")` while the form is incomplete;
    /// otherwise the error says what's wrong.
    fn server_url(&self) -> Result<String, String> {
        match self.kind {
            CalDavKind::Automatic => {
                let email = self.username.trim();
                let domain = match email.rsplit_once('@') {
                    Some((name, domain)) if !name.is_empty() && domain.contains('.') && !domain.ends_with('.') => {
                        domain.to_ascii_lowercase()
                    }
                    _ => return Err(String::new()),
                };
                match domain.as_str() {
                    "icloud.com" | "me.com" | "mac.com" => Ok(caldav::ICLOUD.into()),
                    "aol.com" => Ok(AOL.into()),
                    d if d.starts_with("yahoo.") || d.starts_with("ymail.") => Ok(YAHOO.into()),
                    "gmail.com" | "googlemail.com" => {
                        Err("For a Gmail address, choose Google in the list of providers.".into())
                    }
                    d => Ok(format!("https://{d}/")),
                }
            }
            CalDavKind::Manual => match self.server.trim() {
                "" => Err(String::new()),
                s => caldav::normalize_server(s).map(String::from).map_err(|e| e.to_string()),
            },
            CalDavKind::Advanced => {
                let host = self.server.trim().trim_end_matches('/');
                if host.is_empty() {
                    return Err(String::new());
                }
                if host.contains("://") || host.contains('/') {
                    return Err("Enter just the server's name as Server Address, e.g. dav.example.com.".into());
                }
                let port = match self.port.trim() {
                    "" => String::new(),
                    p => match p.parse::<u16>() {
                        Ok(n) if n > 0 => format!(":{n}"),
                        _ => return Err("Port must be a number from 1 to 65535.".into()),
                    },
                };
                let path = self.path.trim().trim_start_matches('/');
                let scheme = if self.ssl { "https" } else { "http" };
                let url = format!("{scheme}://{host}{port}/{path}");
                caldav::normalize_server(&url).map(String::from).map_err(|e| e.to_string())
            }
        }
    }
}

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    tab: Tab,
    adding: Option<AddStep>,
    /// The account shown in the Accounts tab.
    selected: Option<String>,
    /// The account the "Remove" sheet asks about.
    confirm_remove: Option<String>,
    form: CalDavForm,
    /// The provider sheet's help text is showing.
    help: bool,
}

impl App {
    pub(super) fn open_settings(&mut self) {
        // Like clicking outside it: an open popover keeps its changes.
        if matches!(self.editor.as_ref().map(|ed| ed.placement), Some(crate::editor::Placement::Popover { .. })) {
            self.finish_editor(Outcome::Done);
        }
        self.settings_ui.open = true;
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.settings_ui.open {
            return;
        }
        let before = self.store.settings.clone();
        let sheet = self.settings_ui.adding.is_some() || self.settings_ui.confirm_remove.is_some();
        let caldav_before = self.store.caldav.clone();
        let tab = self.settings_ui.tab;
        let mut open = true;
        egui::Window::new(RichText::new(tab.title()).font(theme::bold_font(14.0)))
            .id(egui::Id::new("settings-window"))
            .collapsible(false)
            .resizable(false)
            .pivot(Align2::CENTER_CENTER)
            .default_pos(ctx.screen_rect().center())
            .fixed_size(Vec2::new(WINDOW_W, 0.0))
            .interactable(!sheet)
            .open(&mut open)
            .show(ctx, |ui| {
                self.tab_bar(ui, p);
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(6.0);
                match tab {
                    Tab::General => self.general_tab(ui, p),
                    Tab::Accounts => self.accounts_tab(ui, p),
                    Tab::Advanced => advanced_tab(ui, &mut self.store.settings),
                }
                ui.add_space(4.0);
            });
        self.add_account_sheet(ctx, p);
        self.remove_account_sheet(ctx, p);

        // Esc closes the topmost sheet, else the window (not while a
        // drop-down is open: Esc closes that first).
        if ctx.input(|i| i.key_pressed(Key::Escape)) && !ctx.memory(|m| m.any_popup_open()) {
            if self.settings_ui.confirm_remove.is_some() {
                self.settings_ui.confirm_remove = None;
            } else if self.settings_ui.adding.is_some() {
                self.settings_ui.adding = None;
            } else {
                open = false;
            }
        }
        if !open {
            self.settings_ui = SettingsUi { tab, ..Default::default() };
        }
        if self.store.settings != before {
            self.settings_changed(&before);
            ctx.request_repaint();
        } else if self.store.caldav != caldav_before {
            // A CalDAV account's description (the sidebar shows it).
            self.store.save();
        }
    }

    /// Saves changed settings and brings the rest of the app in line.
    fn settings_changed(&mut self, before: &Settings) {
        let s = self.store.settings.clone();
        s.apply();
        if (s.day_start, s.hours_shown) != (before.day_start, before.hours_shown) {
            self.scroll_to = Some(s.scroll_hour());
        }
        if (s.week_start, s.days_per_week, s.week_scroll) != (before.week_start, before.days_per_week, before.week_scroll)
        {
            self.week_first = None;
        }
        // Calendars that were just hidden can't keep the selection or editor.
        let shown = |app: &App, id: &str| app.store.event(id).is_some_and(|e| app.store.is_visible(&e.calendar_id));
        if self.selected.as_deref().is_some_and(|id| !shown(self, id)) {
            self.selected = None;
        }
        if let Some(ed) = &self.editor {
            if !self.store.calendar(&ed.draft.calendar_id).is_some_and(|c| self.store.is_listed(c)) {
                self.finish_editor(Outcome::Done);
            }
        }
        // Download again for the new range: more history, or less kept.
        let history_changed = s.history_months != before.history_months;
        if history_changed {
            for st in self.store.sync_state.values_mut() {
                st.token = None;
            }
        }
        self.store.save();
        if (s.google_enabled && !before.google_enabled) || history_changed {
            self.sync();
        }
    }

    // ---- Tab bar ------------------------------------------------------------

    fn tab_bar(&mut self, ui: &mut Ui, p: &Palette) {
        let size = Vec2::new(86.0, 56.0);
        let gap = 6.0;
        let total = Tab::ALL.len() as f32 * (size.x + gap) - gap;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.add_space(((ui.available_width() - total) / 2.0).max(0.0));
            for tab in Tab::ALL {
                let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                let selected = tab == self.settings_ui.tab;
                if selected {
                    ui.painter().rect(rect, 8.0, p.selection, Stroke::new(1.0_f32, p.grid));
                } else if resp.hovered() {
                    ui.painter().rect_filled(rect, 8.0, p.selection.gamma_multiply(0.6));
                }
                let color = if selected { RED } else { p.secondary };
                tab_icon(ui.painter(), tab, rect.center() - Vec2::new(0.0, 9.0), color);
                ui.painter().text(
                    rect.center() + Vec2::new(0.0, 15.0),
                    Align2::CENTER_CENTER,
                    tab.title(),
                    theme::font(12.5),
                    color,
                );
                if resp.clicked() {
                    self.settings_ui.tab = tab;
                }
            }
        });
    }

    // ---- General ------------------------------------------------------------

    fn general_tab(&mut self, ui: &mut Ui, p: &Palette) {
        let calendars: Vec<(String, String, Color32)> =
            self.store.writable_calendars().map(|c| (c.id.clone(), c.name.clone(), theme::rgb(c.color))).collect();
        let s = &mut self.store.settings;

        form_row(ui, p, LABEL_W, "Days per week", |ui| {
            combo(ui, "days-per-week", &mut s.days_per_week, &[(5, "5".into()), (7, "7".into())], COMBO_W);
        });
        form_row(ui, p, LABEL_W, "Start week on", |ui| {
            let system = format!("System Setting ({})", weekday_name(settings::system_week_start()));
            // Sunday first, like macOS.
            let days = [6u8, 0, 1, 2, 3, 4, 5].map(|n| (Some(n), weekday_name(weekday_from_monday(n)).to_owned()));
            let options: Vec<(Option<u8>, String)> = std::iter::once((None, system)).chain(days).collect();
            combo(ui, "week-start", &mut s.week_start, &options, COMBO_W);
        });
        if s.week_start.is_none() {
            note_row(ui, p, "Calendar uses the first day of the week from your region settings.");
        }
        form_row(ui, p, LABEL_W, "Scroll in week view by", |ui| {
            let options = [(WeekScroll::Week, "Week".into()), (WeekScroll::Day, "Day".into())];
            combo(ui, "week-scroll", &mut s.week_scroll, &options, COMBO_W);
        });

        section_break(ui);
        form_row(ui, p, LABEL_W, "Day starts at", |ui| {
            let options: Vec<(u8, String)> = (0..s.day_end).map(|h| (h, hour_label(h))).collect();
            combo(ui, "day-start", &mut s.day_start, &options, COMBO_W);
        });
        form_row(ui, p, LABEL_W, "Day ends at", |ui| {
            let options: Vec<(u8, String)> = (s.day_start + 1..=24).map(|h| (h, hour_label(h))).collect();
            combo(ui, "day-end", &mut s.day_end, &options, COMBO_W);
        });
        form_row(ui, p, LABEL_W, "Show", |ui| {
            let options: Vec<(u8, String)> = (6..=24).map(|h| (h, h.to_string())).collect();
            combo(ui, "hours-shown", &mut s.hours_shown, &options, 64.0);
            ui.label("hours at a time");
        });
        form_row(ui, p, LABEL_W, "Default event duration", |ui| {
            let options: Vec<(u32, String)> = DURATION_CHOICES.iter().map(|&m| (m, duration_label(m))).collect();
            combo(ui, "event-duration", &mut s.event_minutes, &options, COMBO_W);
        });

        section_break(ui);
        form_row(ui, p, LABEL_W, "Default Calendar", |ui| {
            // A calendar that's gone (or hidden) falls back to the selection.
            let current = s.default_calendar.as_ref().and_then(|id| calendars.iter().find(|c| &c.0 == id));
            let text = match current {
                Some((_, name, color)) => RichText::new(format!("● {name}")).color(*color),
                None => RichText::new("Selected calendar"),
            };
            egui::ComboBox::new("default-calendar", "").selected_text(text).width(COMBO_W).height(300.0).show_ui(
                ui,
                |ui| {
                    ui.selectable_value(&mut s.default_calendar, None, "Selected calendar");
                    ui.separator();
                    for (id, name, color) in &calendars {
                        let label = RichText::new(format!("● {name}")).color(*color);
                        ui.selectable_value(&mut s.default_calendar, Some(id.clone()), label);
                    }
                },
            );
        });

        section_break(ui);
        ui.checkbox(&mut s.show_birthdays, "Show Birthdays calendar")
            .on_hover_text("Google's birthdays calendar, made from your contacts");
        ui.checkbox(&mut s.show_holidays, "Show Holidays calendar")
            .on_hover_text("Holiday calendars you've added to your Google account");
    }

    // ---- Accounts -----------------------------------------------------------

    /// Settings → Accounts; with no accounts yet, straight to "Add Account".
    pub(super) fn open_accounts(&mut self) {
        self.open_settings();
        self.settings_ui.tab = Tab::Accounts;
        if self.account_ids().is_empty() && self.settings_ui.adding.is_none() {
            self.settings_ui.adding = Some(AddStep::Provider(None));
        }
    }

    /// Every signed-in account, Google first (as in the sidebar).
    fn account_ids(&self) -> Vec<String> {
        let google = self.store.google.is_connected().then(|| GOOGLE.to_owned());
        google.into_iter().chain(self.store.caldav.iter().map(|a| a.id.clone())).collect()
    }

    fn provider_of(&self, account: &str) -> Provider {
        if account == GOOGLE {
            return Provider::Google;
        }
        self.store.caldav_account(account).map_or(Provider::CalDav, Provider::of)
    }

    /// An account's name and user name, as the account list shows them.
    fn account_titles(&self, account: &str) -> (String, String) {
        if account == GOOGLE {
            let name = match self.store.settings.google_description.trim() {
                "" => "Google".to_owned(),
                d => d.to_owned(),
            };
            return (name, self.store.google.account_email.clone().unwrap_or_else(|| "Google".into()));
        }
        match self.store.caldav_account(account) {
            Some(a) if a.label.trim().is_empty() => (default_label(Provider::of(a), &a.server), a.username.clone()),
            Some(a) => (a.label.clone(), a.username.clone()),
            None => ("CalDAV".into(), String::new()),
        }
    }

    fn accounts_tab(&mut self, ui: &mut Ui, p: &Palette) {
        let ids = self.account_ids();
        if !self.settings_ui.selected.as_ref().is_some_and(|s| ids.contains(s)) {
            self.settings_ui.selected = ids.first().cloned();
        }
        let selected = self.settings_ui.selected.clone();
        let height = 300.0;
        ui.horizontal_top(|ui| {
            // The account list, with + and − below it.
            ui.vertical(|ui| {
                let (list, _) = ui.allocate_exact_size(Vec2::new(180.0, height), Sense::hover());
                ui.painter().rect(list, 6.0, p.sidebar, Stroke::new(1.0_f32, p.grid));
                for (i, id) in ids.iter().enumerate() {
                    let row = egui::Rect::from_min_size(list.min + Vec2::new(0.0, i as f32 * 52.0), Vec2::new(list.width(), 52.0))
                        .shrink(3.0);
                    if row.bottom() > list.bottom() {
                        break;
                    }
                    let resp = ui.interact(row, ui.id().with(("account-row", id)), Sense::click());
                    if selected.as_ref() == Some(id) {
                        ui.painter().rect_filled(row, 5.0, p.selection);
                    } else if resp.hovered() {
                        ui.painter().rect_filled(row, 5.0, p.selection.gamma_multiply(0.5));
                    }
                    if resp.clicked() {
                        self.settings_ui.selected = Some(id.clone());
                    }
                    let (name, subtitle) = self.account_titles(id);
                    account_badge(ui.painter(), self.provider_of(id), &name, Pos2::new(row.left() + 22.0, row.center().y), 15.0);
                    let text_x = row.left() + 46.0;
                    let max_w = row.right() - text_x - 4.0;
                    let painter = ui.painter().with_clip_rect(row);
                    painter.galley(Pos2::new(text_x, row.top() + 7.0), one_line(ui, &name, theme::font(14.0), p.text, max_w), p.text);
                    painter.galley(
                        Pos2::new(text_x, row.top() + 26.0),
                        one_line(ui, &subtitle, theme::font(12.0), p.secondary, max_w),
                        p.secondary,
                    );
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let small = |text: &str| egui::Button::new(RichText::new(text).size(15.0)).min_size(Vec2::new(28.0, 22.0));
                    if ui.add(small("+")).on_hover_text("Add an account").clicked() {
                        self.settings_ui.adding = Some(AddStep::Provider(None));
                    }
                    if ui.add_enabled(selected.is_some(), small("−")).on_hover_text("Remove this account").clicked() {
                        self.settings_ui.confirm_remove = selected.clone();
                    }
                });
            });

            // The selected account's details, in a card filling the rest of the row.
            let margin = 14.0;
            let card_w = ui.available_width() - 2.0 * margin;
            let card = egui::Frame::none()
                .fill(p.card)
                .rounding(8.0)
                .stroke(Stroke::new(1.0_f32, p.grid))
                .inner_margin(egui::Margin::same(margin));
            card.show(ui, |ui| {
                ui.set_width(card_w);
                ui.set_min_height(height - 2.0 * margin);
                ui.with_layout(Layout::top_down(Align::Min), |ui| match selected.as_deref() {
                    None => {
                        ui.add_space(height / 2.0 - 40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No accounts").font(theme::font(15.0)).color(p.placeholder));
                            ui.label(RichText::new("Click + to add an iCloud, Google or CalDAV account.").color(p.secondary));
                        });
                    }
                    Some(GOOGLE) => self.google_details(ui, p),
                    Some(id) => self.caldav_details(ui, p, id),
                });
            });
        });
    }

    fn google_details(&mut self, ui: &mut Ui, p: &Palette) {
        let email = self.store.google.account_email.clone().unwrap_or_default();
        let s = &mut self.store.settings;
        let label_w = 130.0;
        ui.horizontal(|ui| {
            ui.add_space(label_w + 8.0);
            ui.checkbox(&mut s.google_enabled, "Enable this account");
        });
        ui.add_space(4.0);
        let field_w = ui.available_width() - label_w - 8.0;
        form_row(ui, p, label_w, "Description", |ui| {
            ui.add(egui::TextEdit::singleline(&mut s.google_description).hint_text("Google").desired_width(field_w));
        });
        form_row(ui, p, label_w, "User Name", |ui| {
            let mut shown = email.clone();
            ui.add_enabled(false, egui::TextEdit::singleline(&mut shown).desired_width(field_w));
        });
        form_row(ui, p, label_w, "Refresh Calendars", |ui| {
            let options: Vec<(u32, String)> = REFRESH_CHOICES.iter().map(|&m| (m, refresh_label(m))).collect();
            combo(ui, "refresh", &mut s.refresh_minutes, &options, field_w - 8.0);
        });
        ui.add_space(10.0);
        let note = if s.google_enabled {
            "Your changes are sent to Google right away. If Google can't be reached they're kept and retried every minute."
        } else {
            "This account is switched off: its calendars are hidden and it isn't synced."
        };
        ui.label(RichText::new(note).small().color(p.secondary));
    }

    fn caldav_details(&mut self, ui: &mut Ui, p: &Palette, id: &str) {
        let refresh = &mut self.store.settings.refresh_minutes;
        let Some(a) = self.store.caldav.iter_mut().find(|a| a.id == id) else { return };
        let provider = Provider::of(a);
        let label_w = 130.0;
        let field_w = ui.available_width() - label_w - 8.0;
        form_row(ui, p, label_w, "Description", |ui| {
            let fallback = default_label(provider, &a.server);
            let resp = ui.add(egui::TextEdit::singleline(&mut a.label).hint_text(&fallback).desired_width(field_w));
            if resp.lost_focus() && a.label.trim().is_empty() {
                a.label = fallback;
            }
        });
        form_row(ui, p, label_w, "User Name", |ui| {
            let mut shown = a.username.clone();
            ui.add_enabled(false, egui::TextEdit::singleline(&mut shown).desired_width(field_w));
        });
        if provider == Provider::CalDav {
            form_row(ui, p, label_w, "Server Address", |ui| {
                let mut shown = a.server.clone();
                ui.add_enabled(false, egui::TextEdit::singleline(&mut shown).desired_width(field_w));
            });
        }
        form_row(ui, p, label_w, "Refresh Calendars", |ui| {
            let options: Vec<(u32, String)> = REFRESH_CHOICES.iter().map(|&m| (m, refresh_label(m))).collect();
            combo(ui, "refresh", refresh, &options, field_w - 8.0);
        });
        ui.add_space(10.0);
        if a.password.is_none() {
            ui.label(
                RichText::new("The password for this account is missing, so it isn't synced. Remove the account and add it again.")
                    .small()
                    .color(RED),
            );
        } else {
            let service = if provider == Provider::CalDav { "the server" } else { provider.name() };
            ui.label(
                RichText::new(format!(
                    "Your changes are sent to {service} right away. If it can't be reached they're kept and retried every minute."
                ))
                .small()
                .color(p.secondary),
            );
        }
    }

    /// "Choose a Calendar account provider…", then that provider's sign-in.
    fn add_account_sheet(&mut self, ctx: &egui::Context, p: &Palette) {
        // A CalDAV sign-in finished: show the new account (or, when it
        // failed, the error under the form).
        if let Some(id) = self.settings_ui.form.pending.clone() {
            if self.store.caldav_account(&id).is_some() {
                self.settings_ui.form = CalDavForm::default();
                if matches!(self.settings_ui.adding, Some(AddStep::SignIn(_))) {
                    self.settings_ui.adding = None;
                    self.settings_ui.selected = Some(id);
                }
            } else if !self.engine.is_connecting() {
                self.settings_ui.form.pending = None;
            }
        }
        let Some(step) = self.settings_ui.adding else { return };
        if step == AddStep::SignIn(Provider::Google) && self.store.google.is_connected() {
            // Signed in: the new account shows in the list.
            self.settings_ui.adding = None;
            self.settings_ui.selected = Some(GOOGLE.to_owned());
            return;
        }
        let mut next = Some(step);
        let mut connect = None;
        egui::Window::new("Add Account")
            .id(egui::Id::new("add-account"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(SHEET_W, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                match step {
                    AddStep::Provider(choice) => next = self.provider_step(ui, p, choice),
                    AddStep::SignIn(provider) => {
                        sheet_header(ui, p, provider);
                        ui.add_space(14.0);
                        let ready = match provider {
                            Provider::Google => self.google_form(ui, p),
                            _ => self.caldav_form(ui, p, provider),
                        };
                        ui.add_space(18.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let size = Vec2::new(90.0, 24.0);
                            let label = if provider == Provider::Google { "Sign in with Google" } else { "Sign In" };
                            let btn = default_button(label, ready).min_size(size);
                            let enter = ready && ui.input(|i| i.key_pressed(Key::Enter));
                            if ui.add_enabled(ready, btn).clicked() || enter {
                                connect = Some(provider);
                            }
                            if ui.add(egui::Button::new("Cancel").min_size(size)).clicked() {
                                next = None;
                            }
                        });
                    }
                }
            });
        self.settings_ui.adding = next;
        match connect {
            None => {}
            Some(Provider::Google) => {
                self.status = None;
                self.store.save();
                let g = &self.store.google;
                let (client_id, client_secret) = crate::google::client(&g.client_id, &g.client_secret);
                self.worker.send(Cmd::ConnectGoogle { client_id, client_secret });
            }
            Some(provider) => {
                let f = &mut self.settings_ui.form;
                let server = match provider.server() {
                    Some(s) => s.to_owned(),
                    None => match f.server_url() {
                        Ok(s) => s,
                        Err(_) => return,
                    },
                };
                let account = CalDavAccount {
                    id: CalDavAccount::new_id(),
                    label: default_label(Provider::of_server(&server), &server),
                    server,
                    username: f.username.trim().to_owned(),
                    password: Some(f.password.clone()),
                };
                f.pending = Some(account.id.clone());
                self.status = None;
                self.engine.connect_caldav(account);
                self.flush();
            }
        }
    }

    /// The provider list; returns the sheet's next step.
    fn provider_step(&mut self, ui: &mut Ui, p: &Palette, mut choice: Option<Provider>) -> Option<AddStep> {
        ui.label(RichText::new("Choose a Calendar account provider…").font(theme::bold_font(14.0)).color(p.text));
        ui.add_space(10.0);
        let google_taken = self.store.google.is_connected();
        let window = ui.visuals().window_fill;
        let mut go = false;
        for provider in Provider::ALL {
            let unavailable = match provider {
                Provider::Exchange => Some("Microsoft Exchange isn't supported yet"),
                Provider::Google if google_taken => Some("Only one Google account is supported"),
                _ => None,
            };
            let h = if provider == Provider::CalDav { 36.0 } else { 44.0 };
            let sense = if unavailable.is_some() { Sense::hover() } else { Sense::click() };
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), sense);
            let radio = Pos2::new(rect.left() + 112.0, rect.center().y);
            let fill = if choice == Some(provider) {
                ACCENT
            } else {
                theme::blend(window, p.text, if resp.hovered() && unavailable.is_none() { 0.24 } else { 0.14 })
            };
            ui.painter().circle_filled(radio, 8.0, fill);
            if choice == Some(provider) {
                ui.painter().circle_filled(radio, 3.0, Color32::WHITE);
            }
            let galley = ui.fonts(|f| f.layout_job(provider_wordmark(provider, p, unavailable.is_some())));
            let pos = Pos2::new(radio.x + 22.0, rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(pos, galley, p.text);
            match unavailable {
                Some(why) => {
                    resp.on_hover_text(why);
                }
                None => {
                    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                    if resp.clicked() {
                        choice = Some(provider);
                    }
                    if resp.double_clicked() {
                        go = true;
                    }
                }
            }
        }
        if self.settings_ui.help {
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "CleanCalendar syncs with iCloud, Google, Yahoo, AOL, and other CalDAV servers such as Fastmail and \
                     Nextcloud. Microsoft Exchange isn't supported yet.",
                )
                .small()
                .color(p.secondary),
            );
        }
        ui.add_space(18.0);
        let mut next = Some(AddStep::Provider(choice));
        ui.horizontal(|ui| {
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
            let fill = theme::blend(window, p.text, if resp.hovered() { 0.24 } else { 0.14 });
            ui.painter().circle_filled(r.center(), 12.0, fill);
            ui.painter().text(r.center(), Align2::CENTER_CENTER, "?", theme::font(14.0), p.text);
            if resp.on_hover_text("Which accounts can I add?").clicked() {
                self.settings_ui.help = !self.settings_ui.help;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let size = Vec2::new(90.0, 24.0);
                let btn = default_button("Continue", choice.is_some()).min_size(size);
                if ui.add_enabled(choice.is_some(), btn).clicked() || (choice.is_some() && ui.input(|i| i.key_pressed(Key::Enter))) {
                    go = true;
                }
                if ui.add(egui::Button::new("Cancel").min_size(size)).clicked() {
                    next = None;
                }
            });
        });
        if let (true, Some(provider)) = (go, choice) {
            // A new sign-in starts empty, unless one is still under way.
            if self.settings_ui.form.pending.is_none() {
                self.settings_ui.form = CalDavForm::default();
            }
            if matches!(self.status, Some((_, true))) {
                self.status = None;
            }
            next = Some(AddStep::SignIn(provider));
        }
        next
    }

    /// Google's sign-in form; returns whether "Sign in" can be pressed.
    fn google_form(&mut self, ui: &mut Ui, p: &Palette) -> bool {
        ui.label("Your browser opens so you can sign in to Google and allow CleanCalendar to use your calendars.");
        ui.add_space(4.0);
        ui.label(
            RichText::new("CleanCalendar never sees your password, and you can revoke access any time in your Google Account.")
                .color(p.secondary),
        );
        ui.add_space(10.0);
        let g = &mut self.store.google;
        let field_w = ui.available_width() - SHEET_LABEL_W - 16.0;
        egui::CollapsingHeader::new("Use your own OAuth client")
            .id_salt("google-own-client")
            .default_open(!g.client_id.trim().is_empty())
            .show(ui, |ui| {
                ui.label(
                    RichText::new("Optional. Create a “Desktop app” OAuth client with the Google Calendar API enabled.")
                        .color(p.secondary),
                );
                ui.hyperlink_to("Open Google Cloud Console", "https://console.cloud.google.com/apis/credentials");
                ui.add_space(4.0);
                form_row(ui, p, SHEET_LABEL_W, "Client ID", |ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut g.client_id)
                            .hint_text("CleanCalendar's own")
                            .desired_width(field_w - 8.0),
                    );
                });
                form_row(ui, p, SHEET_LABEL_W, "Client Secret", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut g.client_secret).password(true).desired_width(field_w - 8.0));
                });
            });
        let busy = self.worker.busy > 0;
        sheet_status(ui, p, busy, "Waiting for the browser…", &self.status, None);
        !busy
    }

    /// A CalDAV sign-in (iCloud, Yahoo, AOL, or any server); returns
    /// whether "Sign In" can be pressed.
    fn caldav_form(&mut self, ui: &mut Ui, p: &Palette, provider: Provider) -> bool {
        let f = &mut self.settings_ui.form;
        let busy = f.pending.is_some();
        let field_w = ui.available_width() - SHEET_LABEL_W - 16.0;
        let intro = match provider {
            Provider::ICloud => Some((
                "Sign in with your Apple ID and an app-specific password, not your Apple ID password.",
                "Create an app-specific password",
                "https://account.apple.com/account/manage",
            )),
            Provider::Yahoo => Some((
                "Sign in with your Yahoo email address and an app password (Account Security → Generate app password).",
                "Open Yahoo Account Security",
                "https://login.yahoo.com/account/security",
            )),
            Provider::Aol => Some((
                "Sign in with your AOL email address and an app password (Account Security → Generate app password).",
                "Open AOL Account Security",
                "https://login.aol.com/account/security",
            )),
            _ => None,
        };
        if let Some((text, link, url)) = intro {
            ui.label(RichText::new(text).color(p.secondary));
            ui.hyperlink_to(link, url);
            ui.add_space(10.0);
        }
        let generic = provider == Provider::CalDav;
        let mut first = None;
        ui.add_enabled_ui(!busy, |ui| {
            if generic {
                form_row(ui, p, SHEET_LABEL_W, "Account Type", |ui| {
                    let options = [
                        (CalDavKind::Automatic, "Automatic".to_owned()),
                        (CalDavKind::Manual, "Manual".to_owned()),
                        (CalDavKind::Advanced, "Advanced".to_owned()),
                    ];
                    combo(ui, "caldav-kind", &mut f.kind, &options, field_w);
                });
            }
            let user_label = match provider {
                Provider::ICloud => "Apple ID",
                Provider::CalDav if f.kind != CalDavKind::Automatic => "User Name",
                _ => "Email Address",
            };
            form_row(ui, p, SHEET_LABEL_W, user_label, |ui| {
                let hint = if provider == Provider::ICloud { "name@icloud.com" } else { "name@example.com" };
                first = Some(ui.add(egui::TextEdit::singleline(&mut f.username).hint_text(hint).desired_width(field_w - 8.0)));
            });
            let password_label = if generic { "Password" } else { "App Password" };
            form_row(ui, p, SHEET_LABEL_W, password_label, |ui| {
                let hint = if provider == Provider::ICloud { "xxxx-xxxx-xxxx-xxxx" } else { "Required" };
                ui.add(egui::TextEdit::singleline(&mut f.password).password(true).hint_text(hint).desired_width(field_w - 8.0));
            });
            if generic && f.kind != CalDavKind::Automatic {
                form_row(ui, p, SHEET_LABEL_W, "Server Address", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.server).hint_text("example.com").desired_width(field_w - 8.0));
                });
            }
            if generic && f.kind == CalDavKind::Advanced {
                form_row(ui, p, SHEET_LABEL_W, "Server Path", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.path).hint_text("/calendars/user/").desired_width(field_w - 8.0));
                });
                form_row(ui, p, SHEET_LABEL_W, "Port", |ui| {
                    ui.add(egui::TextEdit::singleline(&mut f.port).hint_text(if f.ssl { "443" } else { "80" }).desired_width(56.0));
                    ui.add_space(14.0);
                    ui.checkbox(&mut f.ssl, "Use SSL");
                });
            }
        });
        // Start typing in the first field.
        if let Some(first) = first {
            if ui.memory(|m| m.focused().is_none()) && !busy {
                first.request_focus();
            }
        }
        let server = if generic { f.server_url() } else { Ok(String::new()) };
        let problem = match &server {
            Err(e) if !e.is_empty() => Some(e.as_str()),
            _ => None,
        };
        sheet_status(ui, p, busy, "Finding your calendars…", &self.status, problem);
        !busy && server.is_ok() && !f.username.trim().is_empty() && !f.password.is_empty()
    }

    /// Asks before removing an account (signing out of it).
    fn remove_account_sheet(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(id) = self.settings_ui.confirm_remove.clone() else { return };
        let ids = self.account_ids();
        if !ids.contains(&id) {
            self.settings_ui.confirm_remove = None;
            return;
        }
        let (name, user) = self.account_titles(&id);
        let who = if user.is_empty() || user == name { name } else { format!("{name} ({user})") };
        let provider = self.provider_of(&id);
        let service = if provider == Provider::CalDav { "the server" } else { provider.name() };
        let on_account = |s: &Store, ev: &Event| s.remote_of(ev).is_some_and(|c| c.account == id);
        let unsynced = self.store.events.iter().filter(|e| e.pending && on_account(&self.store, e)).count()
            + self.store.pending_deletes.iter().filter(|d| d.calendar.account == id).count();
        let mut remove = false;
        let mut cancel = false;
        egui::Window::new("Remove Account")
            .id(egui::Id::new("remove-account"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(380.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("Remove the account “{who}”?")).font(theme::bold_font(14.0)));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "Its calendars are removed from this computer. They stay on {service}, and come back if you add the account again."
                    ))
                    .color(p.secondary),
                );
                if unsynced > 0 {
                    let s = if unsynced == 1 { "" } else { "s" };
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("{unsynced} change{s} not yet sent to {service} will be lost.")).color(RED));
                }
                ui.add_space(12.0);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button(RichText::new("Remove").color(RED)).clicked() {
                        remove = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel || remove {
            self.settings_ui.confirm_remove = None;
        }
        if remove {
            if self.editor.as_ref().is_some_and(|ed| on_account(&self.store, &ed.draft)) {
                self.editor = None;
            }
            self.engine.disconnect(&mut self.store, &id);
            if id == GOOGLE {
                // Account settings start over for the next account.
                let s = &mut self.store.settings;
                s.google_enabled = true;
                s.google_description.clear();
            }
            self.settings_ui.selected = None;
            self.selected = None;
            self.flush();
        }
    }
}

// ---- Advanced ---------------------------------------------------------------

fn advanced_tab(ui: &mut Ui, s: &mut Settings) {
    ui.add_space(2.0);
    ui.checkbox(&mut s.year_heatmap, "Show events in year view")
        .on_hover_text("Shade each day in the year view by how busy it is");
    ui.checkbox(&mut s.week_numbers, "Show week numbers")
        .on_hover_text("Show the week number in the week and month views");
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label("Keep past events:");
        let options: Vec<(u32, String)> = HISTORY_CHOICES.iter().map(|&m| (m, history_label(m))).collect();
        combo(ui, "history", &mut s.history_months, &options, 120.0);
    })
    .response
    .on_hover_text("How much of the past to download from iCloud, CalDAV and Google calendars (and to search)");
    ui.add_space(2.0);
}

// ---- Widgets ----------------------------------------------------------------

/// A form row: a right-aligned "Label:" in a column `label_w` wide, then
/// the row's controls.
fn form_row(ui: &mut Ui, p: &Palette, label_w: f32, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(label_w, ROW_H), Sense::hover());
        add(ui);
        // Centered on the row's controls (a drop-down is taller than text).
        let y = ui.min_rect().center().y;
        ui.painter().text(Pos2::new(rect.right(), y), Align2::RIGHT_CENTER, format!("{label}:"), theme::font(13.5), p.text);
    });
}

/// Small print under a form row's controls.
fn note_row(ui: &mut Ui, p: &Palette, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(LABEL_W + ui.spacing().item_spacing.x);
        ui.label(RichText::new(text).small().color(p.secondary));
    });
}

fn section_break(ui: &mut Ui) {
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(4.0);
}

/// A drop-down choosing one of `options` (value, label).
fn combo<T: PartialEq + Clone>(ui: &mut Ui, id: &str, value: &mut T, options: &[(T, String)], width: f32) {
    let current = options.iter().find(|(v, _)| v == value).map(|(_, l)| l.clone()).unwrap_or_default();
    egui::ComboBox::new(id, "").selected_text(current).width(width).height(300.0).show_ui(ui, |ui| {
        for (v, label) in options {
            ui.selectable_value(value, v.clone(), label);
        }
    });
}

/// "08:00" for an hour of the day; 24 is the end of the day.
fn hour_label(h: u8) -> String {
    match h {
        24 if CLOCK_24H => "24:00".into(),
        24 => "Midnight".into(),
        h => fmt_hour(h as u32),
    }
}

fn one_line(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap.max_width = max_w.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|f| f.layout_job(job))
}

/// The tab icons: a gear, an at sign, and two gears (like macOS's).
fn tab_icon(painter: &Painter, tab: Tab, center: Pos2, color: Color32) {
    match tab {
        Tab::General => {
            painter.text(center, Align2::CENTER_CENTER, "⚙", theme::font(22.0), color);
        }
        Tab::Accounts => {
            painter.text(center, Align2::CENTER_CENTER, "@", theme::font(24.0), color);
        }
        Tab::Advanced => {
            painter.text(center + Vec2::new(-5.0, -3.0), Align2::CENTER_CENTER, "⚙", theme::font(17.0), color);
            painter.text(center + Vec2::new(7.0, 5.0), Align2::CENTER_CENTER, "⚙", theme::font(13.0), color);
        }
    }
}

/// A round white badge with a blue "G", standing in for Google's logo.
fn google_badge(painter: &Painter, center: Pos2, radius: f32) {
    painter.circle_filled(center, radius, Color32::WHITE);
    painter.text(center, Align2::CENTER_CENTER, "G", theme::bold_font(radius * 1.25), GOOGLE_BLUE);
}

/// What a new CalDAV account is called: the service, or the server's host.
fn default_label(provider: Provider, server: &str) -> String {
    match provider {
        Provider::CalDav => caldav::normalize_server(server)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_else(|| server.to_owned()),
        p => p.name().to_owned(),
    }
}

/// The sign-in sheet's icon and title ("Add a CalDAV account").
fn sheet_header(ui: &mut Ui, p: &Palette, provider: Provider) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        match provider {
            Provider::CalDav => calendar_icon(ui.painter(), rect),
            _ => account_badge(ui.painter(), provider, provider.name(), rect.center(), 17.0),
        }
        ui.add_space(8.0);
        let title = match provider {
            Provider::ICloud => "Add an iCloud account",
            Provider::Google => "Add a Google account",
            Provider::Yahoo => "Add a Yahoo account",
            Provider::Aol => "Add an AOL account",
            _ => "Add a CalDAV account",
        };
        ui.label(RichText::new(title).font(theme::bold_font(15.0)).color(p.text));
    });
}

/// Under a sign-in form: a spinner while signing in, else what's wrong.
fn sheet_status(ui: &mut Ui, p: &Palette, busy: bool, busy_text: &str, status: &Option<(String, bool)>, problem: Option<&str>) {
    let error = problem.or(match status {
        Some((msg, true)) => Some(msg.as_str()),
        _ => None,
    });
    if !busy && error.is_none() {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(SHEET_LABEL_W + ui.spacing().item_spacing.x);
        ui.vertical(|ui| {
            if busy {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(busy_text).color(p.secondary));
                });
            } else if let Some(error) = error {
                ui.label(RichText::new(error).color(RED));
            }
        });
    });
}

/// A small round logo for an account in the list and on its sign-in sheet.
fn account_badge(painter: &Painter, provider: Provider, name: &str, center: Pos2, r: f32) {
    let white = Color32::WHITE;
    match provider {
        Provider::Google => google_badge(painter, center, r),
        Provider::ICloud => {
            // A white cloud on sky blue.
            painter.circle_filled(center, r, Color32::from_rgb(0x3E, 0x9B, 0xF6));
            let at = |x: f32, y: f32| center + Vec2::new(x * r, y * r);
            painter.circle_filled(at(-0.32, 0.12), 0.28 * r, white);
            painter.circle_filled(at(0.02, -0.1), 0.38 * r, white);
            painter.circle_filled(at(0.36, 0.14), 0.26 * r, white);
            painter.rect_filled(egui::Rect::from_min_max(at(-0.32, 0.12), at(0.36, 0.4)), 0.0, white);
        }
        Provider::Yahoo => {
            painter.circle_filled(center, r, Color32::from_rgb(0x60, 0x01, 0xD2));
            painter.text(center, Align2::CENTER_CENTER, "Y!", theme::bold_font(r * 0.95), white);
        }
        Provider::Aol => {
            painter.circle_filled(center, r, Color32::from_gray(0x20));
            painter.text(center, Align2::CENTER_CENTER, "Aol.", theme::bold_font(r * 0.62), white);
        }
        Provider::Exchange | Provider::CalDav => {
            painter.circle_filled(center, r, Color32::from_rgb(0x8E, 0x8E, 0x93));
            let initial: String = name.chars().next().map_or_else(|| "@".into(), |c| c.to_uppercase().collect());
            painter.text(center, Align2::CENTER_CENTER, initial, theme::bold_font(r * 1.1), white);
        }
    }
}

/// A calendar page: a red band over a white sheet ruled into days.
fn calendar_icon(painter: &Painter, rect: egui::Rect) {
    let r = rect.shrink(2.0);
    painter.rect_filled(r, 6.0, Color32::WHITE);
    let band = egui::Rect::from_min_size(r.min, Vec2::new(r.width(), r.height() * 0.24));
    painter.rect_filled(band, egui::Rounding { nw: 6.0, ne: 6.0, sw: 0.0, se: 0.0 }, Color32::from_rgb(0xE5, 0x5B, 0x55));
    let grid = egui::Rect::from_min_max(Pos2::new(r.left() + 5.0, band.bottom() + 4.0), r.max - Vec2::new(5.0, 4.0));
    let (cols, rows) = (6, 4);
    let cell = Vec2::new(grid.width() / cols as f32, grid.height() / rows as f32);
    for row in 0..rows {
        for col in 0..cols {
            let min = grid.min + Vec2::new(col as f32 * cell.x, row as f32 * cell.y);
            let dot = egui::Rect::from_min_size(min, cell).shrink(0.9);
            painter.rect_filled(dot, 0.5, Color32::from_gray(0xC8));
        }
    }
}

const GOOGLE_LETTERS: [(char, Color32); 6] = {
    const RED_G: Color32 = Color32::from_rgb(0xEA, 0x43, 0x35);
    const YELLOW: Color32 = Color32::from_rgb(0xFB, 0xBC, 0x05);
    const GREEN: Color32 = Color32::from_rgb(0x34, 0xA8, 0x53);
    [('G', GOOGLE_BLUE), ('o', RED_G), ('o', YELLOW), ('g', GOOGLE_BLUE), ('l', GREEN), ('e', RED_G)]
};

/// A provider's name as it appears on the provider sheet, set in type to
/// look like its logo; faded when it can't be chosen.
fn provider_wordmark(provider: Provider, p: &Palette, dim: bool) -> LayoutJob {
    let fade = |c: Color32| if dim { c.gamma_multiply(0.35) } else { c };
    let mut job = LayoutJob::default();
    let mut add = |text: &str, font: FontId, color: Color32, italics: bool| {
        job.append(text, 0.0, TextFormat { font_id: font, color: fade(color), italics, ..Default::default() });
    };
    match provider {
        Provider::ICloud => add("iCloud", theme::font(30.0), p.text, false),
        Provider::Exchange => add("Microsoft Exchange", theme::font(25.0), Color32::from_rgb(0x2F, 0x7B, 0xD8), false),
        Provider::Google => {
            for (ch, color) in GOOGLE_LETTERS {
                add(&ch.to_string(), theme::bold_font(30.0), color, false);
            }
        }
        Provider::Yahoo => add("yahoo!", theme::bold_font(26.0), p.text, true),
        Provider::Aol => add("Aol.", theme::bold_font(28.0), p.text, false),
        Provider::CalDav => add("Other CalDAV Account…", theme::font(14.5), p.text, false),
    }
    job
}

/// A sheet's default button: blue when it can be pressed, grey otherwise
/// (egui keeps a disabled button's fill).
fn default_button(label: &str, ready: bool) -> egui::Button<'static> {
    if ready {
        egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(ACCENT)
    } else {
        egui::Button::new(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: CalDavKind, username: &str, server: &str) -> CalDavForm {
        CalDavForm { kind, username: username.into(), server: server.into(), ..CalDavForm::default() }
    }

    #[test]
    fn automatic_finds_the_server_from_the_email_address() {
        let auto = |email: &str| form(CalDavKind::Automatic, email, "").server_url();
        assert_eq!(auto("me@fastmail.com"), Ok("https://fastmail.com/".into()));
        assert_eq!(auto("Me@Example.ORG "), Ok("https://example.org/".into()));
        assert_eq!(auto("me@icloud.com"), Ok(caldav::ICLOUD.into()));
        assert_eq!(auto("me@mac.com"), Ok(caldav::ICLOUD.into()));
        assert_eq!(auto("me@yahoo.co.uk"), Ok(YAHOO.into()));
        assert_eq!(auto("me@aol.com"), Ok(AOL.into()));
        // Not an address yet: nothing to say.
        for partial in ["", "me", "me@", "@example.com", "me@example", "me@example."] {
            assert_eq!(auto(partial), Err(String::new()), "{partial}");
        }
        assert!(auto("me@gmail.com").unwrap_err().contains("Google"));
    }

    #[test]
    fn manual_takes_the_server_address_as_typed() {
        let manual = |server: &str| form(CalDavKind::Manual, "me", server).server_url();
        assert_eq!(manual("dav.example.com"), Ok("https://dav.example.com/".into()));
        assert_eq!(manual("http://nas.local:5232/me"), Ok("http://nas.local:5232/me/".into()));
        assert_eq!(manual("  "), Err(String::new()));
    }

    #[test]
    fn advanced_builds_the_url_from_its_parts() {
        let mut f = form(CalDavKind::Advanced, "me", "dav.example.com");
        assert_eq!(f.server_url(), Ok("https://dav.example.com/".into()));
        f.path = "calendars/me".into();
        f.port = "8443".into();
        assert_eq!(f.server_url(), Ok("https://dav.example.com:8443/calendars/me/".into()));
        f.ssl = false;
        f.path = "/dav/".into();
        f.port = String::new();
        assert_eq!(f.server_url(), Ok("http://dav.example.com/dav/".into()));
        f.port = "http".into();
        assert!(f.server_url().unwrap_err().contains("Port"));
        f.port = String::new();
        f.server = "https://dav.example.com".into();
        assert!(f.server_url().unwrap_err().contains("Server Address"));
    }

    #[test]
    fn accounts_are_named_after_their_service() {
        assert_eq!(Provider::of_server(caldav::ICLOUD), Provider::ICloud);
        assert_eq!(Provider::of_server("https://dav.example.com/"), Provider::CalDav);
        assert_eq!(default_label(Provider::ICloud, caldav::ICLOUD), "iCloud");
        assert_eq!(default_label(Provider::CalDav, "https://dav.example.com:8443/x/"), "dav.example.com");
    }
}

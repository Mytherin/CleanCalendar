//! The Settings window, laid out like macOS Calendar's: a row of tabs with
//! icons (General, Accounts, Locations, Advanced), forms with right-aligned
//! labels, and an "Add Account" sheet that asks for the provider first.

use eframe::egui::{
    self, text::LayoutJob, Align, Align2, Color32, FontId, Key, Layout, Order, Painter, Pos2, RichText, Sense, Stroke,
    TextFormat, Ui, Vec2,
};

use super::App;
use crate::dates::{fmt_hour, CLOCK_24H};
use crate::editor::Outcome;
use crate::model::{Event, Store};
use crate::places::{self, Phase};
use crate::remote::{Cmd, GOOGLE};
use crate::settings::{
    self, duration_label, refresh_label, weekday_from_monday, weekday_name, Settings, WeekScroll, DURATION_CHOICES,
    REFRESH_CHOICES,
};
use crate::theme::{self, Palette, ACCENT, RED};

const WINDOW_W: f32 = 560.0;
/// Width of the right-aligned label column in the General tab.
const LABEL_W: f32 = 200.0;
const COMBO_W: f32 = 230.0;
const ROW_H: f32 = 26.0;
const GOOGLE_BLUE: Color32 = Color32::from_rgb(0x42, 0x85, 0xF4);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    General,
    Accounts,
    Locations,
    Advanced,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::General, Tab::Accounts, Tab::Locations, Tab::Advanced];

    fn title(self) -> &'static str {
        match self {
            Tab::General => "General",
            Tab::Accounts => "Accounts",
            Tab::Locations => "Locations",
            Tab::Advanced => "Advanced",
        }
    }
}

/// The steps of the "Add Account" sheet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AddStep {
    /// "Choose a Calendar account provider…"
    Provider { google: bool },
    /// Google needs the user's own OAuth client, then a browser sign-in.
    Google,
}

#[derive(Default)]
pub struct SettingsUi {
    pub open: bool,
    tab: Tab,
    adding: Option<AddStep>,
    confirm_remove: bool,
    /// The search text for regions to download.
    region_filter: String,
}

impl App {
    pub(super) fn open_settings(&mut self) {
        // Like clicking outside it: an open popover keeps its changes.
        if matches!(self.editor.as_ref().map(|ed| ed.placement), Some(crate::editor::Placement::Popover { .. })) {
            self.finish_editor(Outcome::Done);
        }
        self.settings_ui.open = true;
    }

    /// Opens Settings at `tab`.
    pub(super) fn open_settings_at(&mut self, tab: Tab) {
        self.settings_ui.tab = tab;
        self.open_settings();
    }

    pub(super) fn settings_window(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.settings_ui.open {
            return;
        }
        let before = self.store.settings.clone();
        let sheet = self.settings_ui.adding.is_some() || self.settings_ui.confirm_remove;
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
                    Tab::Locations => self.locations_tab(ui, p),
                    Tab::Advanced => advanced_tab(ui, &mut self.store.settings),
                }
                ui.add_space(4.0);
            });
        self.add_account_sheet(ctx, p);
        self.remove_account_sheet(ctx, p);

        // Esc closes the topmost sheet, else the window (not while a
        // drop-down is open: Esc closes that first).
        if ctx.input(|i| i.key_pressed(Key::Escape)) && !ctx.memory(|m| m.any_popup_open()) {
            if self.settings_ui.confirm_remove {
                self.settings_ui.confirm_remove = false;
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
        self.store.save();
        if s.google_enabled && !before.google_enabled {
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

    fn accounts_tab(&mut self, ui: &mut Ui, p: &Palette) {
        let connected = self.store.google.is_connected();
        let email = self.store.google.account_email.clone().unwrap_or_default();
        let height = 300.0;
        ui.horizontal_top(|ui| {
            // The account list, with + and − below it.
            ui.vertical(|ui| {
                let (list, _) = ui.allocate_exact_size(Vec2::new(180.0, height), Sense::hover());
                ui.painter().rect(list, 6.0, p.sidebar, Stroke::new(1.0_f32, p.grid));
                if connected {
                    let row = egui::Rect::from_min_size(list.min, Vec2::new(list.width(), 52.0)).shrink(3.0);
                    ui.painter().rect_filled(row, 5.0, p.selection);
                    google_badge(ui.painter(), Pos2::new(row.left() + 22.0, row.center().y), 15.0);
                    let name = match self.store.settings.google_description.trim() {
                        "" => "Google",
                        d => d,
                    };
                    let text_x = row.left() + 46.0;
                    let max_w = row.right() - text_x - 4.0;
                    let painter = ui.painter().with_clip_rect(row);
                    painter.galley(
                        Pos2::new(text_x, row.top() + 7.0),
                        one_line(ui, name, theme::font(14.0), p.text, max_w),
                        p.text,
                    );
                    let subtitle = if email.is_empty() { "Google" } else { email.as_str() };
                    painter.galley(
                        Pos2::new(text_x, row.top() + 26.0),
                        one_line(ui, subtitle, theme::font(12.0), p.secondary, max_w),
                        p.secondary,
                    );
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 2.0;
                    let small = |text: &str| egui::Button::new(RichText::new(text).size(15.0)).min_size(Vec2::new(28.0, 22.0));
                    let add = ui.add_enabled(!connected, small("+"));
                    let add = if connected { add.on_disabled_hover_text("Only one Google account is supported") } else { add };
                    if add.on_hover_text("Add an account").clicked() {
                        self.settings_ui.adding = Some(AddStep::Provider { google: false });
                    }
                    if ui.add_enabled(connected, small("−")).on_hover_text("Remove this account").clicked() {
                        self.settings_ui.confirm_remove = true;
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
                ui.with_layout(Layout::top_down(Align::Min), |ui| {
                    if !connected {
                        ui.add_space(height / 2.0 - 40.0);
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("No accounts").font(theme::font(15.0)).color(p.placeholder));
                            ui.label(RichText::new("Click + to add a Google account.").color(p.secondary));
                        });
                        return;
                    }
                    let s = &mut self.store.settings;
                    let label_w = 130.0;
                    ui.horizontal(|ui| {
                        ui.add_space(label_w + 8.0);
                        ui.checkbox(&mut s.google_enabled, "Enable this account");
                    });
                    ui.add_space(4.0);
                    let field_w = ui.available_width() - label_w - 8.0;
                    form_row(ui, p, label_w, "Description", |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut s.google_description)
                                .hint_text("Google")
                                .desired_width(field_w),
                        );
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
                    let note = if !s.google_enabled {
                        "This account is switched off: its calendars are hidden and it isn't synced.".to_owned()
                    } else {
                        "Your changes are sent to Google right away. If Google can't be reached they're kept and retried every minute."
                            .to_owned()
                    };
                    ui.label(RichText::new(note).small().color(p.secondary));
                });
            });
        });
    }

    /// "Choose a Calendar account provider…", then Google's sign-in.
    fn add_account_sheet(&mut self, ctx: &egui::Context, p: &Palette) {
        let Some(step) = self.settings_ui.adding else { return };
        if self.store.google.is_connected() {
            // Signed in: the new account shows in the list.
            self.settings_ui.adding = None;
            return;
        }
        let mut next = Some(step);
        let mut connect = false;
        egui::Window::new("Add Account")
            .id(egui::Id::new("add-account"))
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .order(Order::Foreground)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(Vec2::new(440.0, 0.0))
            .show(ctx, |ui| {
                ui.add_space(6.0);
                match step {
                    AddStep::Provider { mut google } => {
                        ui.label(RichText::new("Choose a Calendar account provider…").font(theme::bold_font(14.0)));
                        ui.add_space(14.0);
                        ui.horizontal(|ui| {
                            ui.add_space(110.0);
                            if ui.radio(google, "").clicked() {
                                google = true;
                            }
                            let logo = ui.label(google_wordmark(30.0)).interact(Sense::click());
                            if logo.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                                google = true;
                            }
                        });
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            ui.add_space(110.0);
                            ui.label(
                                RichText::new("Google is the only provider CleanCalendar can sync with so far.")
                                    .small()
                                    .color(p.secondary),
                            );
                        });
                        ui.add_space(18.0);
                        next = Some(AddStep::Provider { google });
                        sheet_buttons(ui, "Continue", google, &mut next, AddStep::Google);
                    }
                    AddStep::Google => {
                        ui.label(RichText::new("Add a Google account").font(theme::bold_font(14.0)));
                        ui.add_space(4.0);
                        ui.label("CleanCalendar talks to Google with your own OAuth client:");
                        ui.add_space(4.0);
                        for line in [
                            "1. In Google Cloud Console, create a project and enable the Google Calendar API.",
                            "2. Configure the OAuth consent screen and add yourself as a test user.",
                            "3. Create Credentials → OAuth client ID → Application type “Desktop app”.",
                            "4. Paste the client ID and secret below, then sign in.",
                        ] {
                            ui.label(RichText::new(line).color(p.secondary));
                        }
                        ui.hyperlink_to("Open Google Cloud Console", "https://console.cloud.google.com/apis/credentials");
                        ui.add_space(8.0);
                        let g = &mut self.store.google;
                        egui::Grid::new("google-grid").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                            ui.label("Client ID");
                            ui.add(egui::TextEdit::singleline(&mut g.client_id).desired_width(300.0));
                            ui.end_row();
                            ui.label("Client secret");
                            ui.add(egui::TextEdit::singleline(&mut g.client_secret).password(true).desired_width(300.0));
                            ui.end_row();
                        });
                        let busy = self.worker.busy > 0;
                        if busy {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label(RichText::new("Waiting for the browser…").color(p.secondary));
                            });
                        } else if let Some((msg, true)) = &self.status {
                            ui.label(RichText::new(msg).color(RED));
                        }
                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            if ui.button("Back").clicked() {
                                next = Some(AddStep::Provider { google: true });
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let ready = !busy && !g.client_id.trim().is_empty();
                                let btn = egui::Button::new(RichText::new("Sign in with Google").color(Color32::WHITE))
                                    .fill(ACCENT);
                                if ui.add_enabled(ready, btn).clicked() {
                                    connect = true;
                                }
                                if ui.button("Cancel").clicked() {
                                    next = None;
                                }
                            });
                        });
                    }
                }
            });
        self.settings_ui.adding = next;
        if connect {
            self.status = None;
            self.store.save();
            let g = &self.store.google;
            self.worker.send(Cmd::ConnectGoogle { client_id: g.client_id.clone(), client_secret: g.client_secret.clone() });
        }
    }

    /// Confirms removing the Google account (signing out).
    fn remove_account_sheet(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.settings_ui.confirm_remove {
            return;
        }
        let mut remove = false;
        let mut cancel = false;
        let email = self.store.google.account_email.clone().unwrap_or_else(|| "Google".into());
        let unsynced = self.store.unsynced_count();
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
                ui.label(RichText::new(format!("Remove the account “{email}”?")).font(theme::bold_font(14.0)));
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Its calendars are removed from this computer. They stay on Google, and come back if you add the account again.")
                        .color(p.secondary),
                );
                if unsynced > 0 {
                    let s = if unsynced == 1 { "" } else { "s" };
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("{unsynced} change{s} not yet sent to Google will be lost.")).color(RED));
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
            self.settings_ui.confirm_remove = false;
        }
        if remove {
            let on_google = |s: &Store, ev: &Event| s.remote_of(ev).is_some_and(|c| c.account == GOOGLE);
            if self.editor.as_ref().is_some_and(|ed| on_google(&self.store, &ed.draft)) {
                self.editor = None;
            }
            self.engine.disconnect(&mut self.store, GOOGLE);
            // Account settings start over for the next account.
            let s = &mut self.store.settings;
            s.google_enabled = true;
            s.google_description.clear();
            self.selected = None;
            self.flush();
        }
    }
}

// ---- Locations --------------------------------------------------------------

/// "7.3 MB" for a size in bytes.
fn fmt_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n => format!("{} KB", n.div_ceil(1024)),
    }
}

/// "546,328" for a count.
fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A bar filled to `frac` with `text` on it.
fn progress(ui: &mut Ui, frac: Option<f32>, text: String) {
    let bar = match frac {
        Some(f) => egui::ProgressBar::new(f.clamp(0.0, 1.0)),
        None => egui::ProgressBar::new(0.0).animate(true),
    };
    ui.add(bar.desired_width(220.0).desired_height(16.0).text(text));
}

impl App {
    /// The countries whose location data is downloaded, downloads under
    /// way, and a searchable list of countries to add.
    fn locations_tab(&mut self, ui: &mut Ui, p: &Palette) {
        let lib = places::library();
        lib.set_context(ui.ctx());
        ui.label(
            RichText::new(
                "Suggest places while you type an event's location. Each country's map data is downloaded \
                 once and searched on this computer: what you type is never sent anywhere.",
            )
            .color(p.secondary)
            .size(12.5),
        );
        ui.add_space(6.0);

        let installed = lib.installed();
        let jobs = lib.jobs();
        egui::Frame::none()
            .fill(p.sidebar)
            .rounding(6.0)
            .stroke(Stroke::new(1.0_f32, p.grid))
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.set_min_height(110.0);
                ui.spacing_mut().item_spacing.y = 6.0;
                if installed.is_empty() && jobs.is_empty() {
                    ui.add_space(36.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new("No countries yet").color(p.placeholder)));
                }
                for inst in installed.iter().filter(|i| !jobs.iter().any(|j| j.region.id == i.meta.id)) {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            ui.label(RichText::new(&inst.meta.name).font(theme::bold_font(13.5)).color(p.text));
                            let info = format!(
                                "{} places · {} · updated {}",
                                fmt_count(inst.meta.entries),
                                fmt_bytes(inst.bytes),
                                inst.meta.updated
                            );
                            ui.label(RichText::new(info).color(p.secondary).size(11.5));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Remove").clicked() {
                                if let Err(e) = lib.remove(inst) {
                                    self.status = Some((format!("{e:#}"), true));
                                }
                            }
                            // Offered once the app knows of a newer build.
                            let newer = lib
                                .catalog()
                                .iter()
                                .find(|r| r.id == inst.meta.id && r.updated > inst.meta.updated)
                                .cloned();
                            let hover = match &newer {
                                Some(r) => format!("Download the map data of {}", r.updated),
                                None => "This is the latest map data".to_owned(),
                            };
                            if ui.add_enabled(newer.is_some(), egui::Button::new("Update")).on_hover_text(hover).clicked() {
                                lib.add(newer.unwrap());
                            }
                        });
                    });
                }
                for job in &jobs {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&job.region.name).font(theme::bold_font(13.5)).color(p.text));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let failed = matches!(job.phase, Phase::Failed(_));
                            if ui.button(if failed { "Dismiss" } else { "Cancel" }).clicked() {
                                lib.cancel(&job.region.id);
                            }
                            if failed && ui.button("Retry").clicked() {
                                lib.add(job.region.clone());
                            }
                            match &job.phase {
                                Phase::Downloading { done, total } => progress(
                                    ui,
                                    total.map(|t| *done as f32 / t as f32),
                                    match total {
                                        Some(t) => format!("Downloading {} of {}", fmt_bytes(*done), fmt_bytes(*t)),
                                        None => format!("Downloading {}", fmt_bytes(*done)),
                                    },
                                ),
                                Phase::Failed(_) => {}
                            }
                        });
                    });
                    if let Phase::Failed(e) = &job.phase {
                        ui.label(RichText::new(e).color(RED).size(11.5));
                    }
                }
            });

        ui.add_space(8.0);
        let s = &mut self.settings_ui;
        ui.horizontal(|ui| {
            ui.label("Add a country:");
            ui.add(
                egui::TextEdit::singleline(&mut s.region_filter)
                    .hint_text("Search for a country")
                    .desired_width(ui.available_width()),
            );
        });
        let words: Vec<String> = places::tokens(&s.region_filter).collect();
        if !words.is_empty() {
            let all_in = |text: &str| {
                let have: Vec<String> = places::tokens(text).collect();
                words.iter().all(|w| have.iter().any(|h| h.starts_with(w.as_str())))
            };
            let matches: Vec<&places::Region> = lib.catalog().iter().filter(|r| all_in(&r.name)).take(40).collect();
            egui::ScrollArea::vertical().max_height(150.0).auto_shrink([false, true]).show(ui, |ui| {
                if matches.is_empty() {
                    ui.label(RichText::new("No country by that name").color(p.secondary));
                }
                for r in matches {
                    let row = Vec2::new(ui.available_width(), 30.0);
                    ui.allocate_ui_with_layout(row, Layout::left_to_right(Align::Center), |ui| {
                        ui.label(&r.name);
                        ui.label(RichText::new(fmt_bytes(r.bytes)).color(p.secondary).size(11.5));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            let busy = jobs.iter().any(|j| j.region.id == r.id && !matches!(j.phase, Phase::Failed(_)));
                            let have = installed.iter().any(|i| i.meta.id == r.id);
                            let label = if have { "Installed" } else { "Download" };
                            if ui.add_enabled(!busy && !have, egui::Button::new(label)).clicked() {
                                lib.add(r.clone());
                            }
                        });
                    });
                }
            });
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Map data © OpenStreetMap contributors (Open Database License). Each country is a \
                 small download from CleanCalendar's releases on GitHub, which learns only which \
                 countries you pick.",
            )
            .color(p.secondary)
            .size(11.0),
        );
    }
}

// ---- Advanced ---------------------------------------------------------------

fn advanced_tab(ui: &mut Ui, s: &mut Settings) {
    ui.add_space(2.0);
    ui.checkbox(&mut s.year_heatmap, "Show events in year view")
        .on_hover_text("Shade each day in the year view by how busy it is");
    ui.checkbox(&mut s.week_numbers, "Show week numbers")
        .on_hover_text("Show the week number in the week and month views");
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

/// Cancel and a default button (enabled when `ready`) at the bottom right
/// of a sheet; the default button moves on to `then`.
fn sheet_buttons(ui: &mut Ui, label: &str, ready: bool, next: &mut Option<AddStep>, then: AddStep) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let btn = egui::Button::new(RichText::new(label).color(Color32::WHITE)).fill(ACCENT);
        if ui.add_enabled(ready, btn).clicked() {
            *next = Some(then);
        }
        if ui.button("Cancel").clicked() {
            *next = None;
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
        Tab::Locations => {
            // A map pin.
            let c = center - Vec2::new(0.0, 3.0);
            painter.circle_filled(c, 7.5, color);
            painter.add(egui::Shape::convex_polygon(
                vec![Pos2::new(c.x - 6.4, c.y + 3.6), Pos2::new(c.x + 6.4, c.y + 3.6), Pos2::new(c.x, c.y + 13.0)],
                color,
                Stroke::NONE,
            ));
            painter.circle_filled(c, 2.8, Color32::WHITE);
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

/// "Google" in its letter colors.
fn google_wordmark(size: f32) -> LayoutJob {
    const RED_G: Color32 = Color32::from_rgb(0xEA, 0x43, 0x35);
    const YELLOW: Color32 = Color32::from_rgb(0xFB, 0xBC, 0x05);
    const GREEN: Color32 = Color32::from_rgb(0x34, 0xA8, 0x53);
    let mut job = LayoutJob::default();
    for (ch, color) in [('G', GOOGLE_BLUE), ('o', RED_G), ('o', YELLOW), ('g', GOOGLE_BLUE), ('l', GREEN), ('e', RED_G)] {
        job.append(&ch.to_string(), 0.0, TextFormat::simple(theme::bold_font(size), color));
    }
    job
}

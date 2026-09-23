//! SynthHires Bridge — desktop console UI (eframe/egui).
//!
//! Design language: minimalist "cloud console" (Cloudflare One inspired).
//! - Fixed left navigation, content header with live connection pill.
//! - Design tokens (palette) with dark/light; all custom widgets paint
//!   themselves (real hover/active states, not egui's default buttons).
//! - Status feedback: breathing connection dot, status chips, tasteful
//!   modals over a dimmed scrim.

use daemon_core::chat_store::{ChatStore, StoredConversation, StoredMessage};
use daemon_core::consent::{ConsentAnswer, ConsentBroker};
use daemon_core::task_registry::{TaskKind, TaskState, TaskStatus};
use eframe::egui;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

use crate::UiCmd;

// ─── Design tokens ───────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
#[allow(dead_code)] // palette completeness beats lint noise
struct Palette {
    bg: egui::Color32,
    surface: egui::Color32,
    surface2: egui::Color32,
    surface3: egui::Color32,
    border: egui::Color32,
    border_strong: egui::Color32,
    text: egui::Color32,
    text_dim: egui::Color32,
    text_faint: egui::Color32,
    accent: egui::Color32,
    accent_hover: egui::Color32,
    accent_active: egui::Color32,
    accent_soft: egui::Color32,
    success: egui::Color32,
    success_soft: egui::Color32,
    warning: egui::Color32,
    warning_soft: egui::Color32,
    danger: egui::Color32,
    danger_hover: egui::Color32,
    danger_soft: egui::Color32,
    brand_orange: egui::Color32,
}

fn dark_palette() -> Palette {
    Palette {
        bg: egui::Color32::from_rgb(0x0c, 0x0d, 0x10),
        surface: egui::Color32::from_rgb(0x15, 0x17, 0x1c),
        surface2: egui::Color32::from_rgb(0x1d, 0x20, 0x27),
        surface3: egui::Color32::from_rgb(0x24, 0x28, 0x31),
        border: egui::Color32::from_rgb(0x26, 0x2a, 0x33),
        border_strong: egui::Color32::from_rgb(0x33, 0x38, 0x44),
        text: egui::Color32::from_rgb(0xe6, 0xe8, 0xeb),
        text_dim: egui::Color32::from_rgb(0x9b, 0xa1, 0xab),
        text_faint: egui::Color32::from_rgb(0x6b, 0x70, 0x78),
        accent: egui::Color32::from_rgb(0x33, 0x82, 0xff),
        accent_hover: egui::Color32::from_rgb(0x4a, 0x90, 0xff),
        accent_active: egui::Color32::from_rgb(0x2a, 0x6e, 0xd9),
        accent_soft: egui::Color32::from_rgba_unmultiplied(0x33, 0x82, 0xff, 26),
        success: egui::Color32::from_rgb(0x3d, 0xd6, 0x8c),
        success_soft: egui::Color32::from_rgba_unmultiplied(0x3d, 0xd6, 0x8c, 24),
        warning: egui::Color32::from_rgb(0xf5, 0xa6, 0x23),
        warning_soft: egui::Color32::from_rgba_unmultiplied(0xf5, 0xa6, 0x23, 24),
        danger: egui::Color32::from_rgb(0xe5, 0x48, 0x4d),
        danger_hover: egui::Color32::from_rgb(0xf2, 0x55, 0x5a),
        danger_soft: egui::Color32::from_rgba_unmultiplied(0xe5, 0x48, 0x4d, 26),
        brand_orange: egui::Color32::from_rgb(0xf6, 0x82, 0x1f),
    }
}

fn light_palette() -> Palette {
    Palette {
        bg: egui::Color32::from_rgb(0xf6, 0xf7, 0xf9),
        surface: egui::Color32::from_rgb(0xff, 0xff, 0xff),
        surface2: egui::Color32::from_rgb(0xf0, 0xf1, 0xf4),
        surface3: egui::Color32::from_rgb(0xe6, 0xe8, 0xed),
        border: egui::Color32::from_rgb(0xe3, 0xe5, 0xea),
        border_strong: egui::Color32::from_rgb(0xc9, 0xcd, 0xd6),
        text: egui::Color32::from_rgb(0x19, 0x1b, 0x1f),
        text_dim: egui::Color32::from_rgb(0x5c, 0x62, 0x6c),
        text_faint: egui::Color32::from_rgb(0x9a, 0xa0, 0xa9),
        accent: egui::Color32::from_rgb(0x00, 0x51, 0xc3),
        accent_hover: egui::Color32::from_rgb(0x1a, 0x63, 0xd1),
        accent_active: egui::Color32::from_rgb(0x00, 0x43, 0xa0),
        accent_soft: egui::Color32::from_rgba_unmultiplied(0x00, 0x51, 0xc3, 18),
        success: egui::Color32::from_rgb(0x18, 0x8a, 0x54),
        success_soft: egui::Color32::from_rgba_unmultiplied(0x18, 0x8a, 0x54, 20),
        warning: egui::Color32::from_rgb(0xc7, 0x78, 0x00),
        warning_soft: egui::Color32::from_rgba_unmultiplied(0xc7, 0x78, 0x00, 20),
        danger: egui::Color32::from_rgb(0xd3, 0x2f, 0x2f),
        danger_hover: egui::Color32::from_rgb(0xe1, 0x3d, 0x3d),
        danger_soft: egui::Color32::from_rgba_unmultiplied(0xd3, 0x2f, 0x2f, 18),
        brand_orange: egui::Color32::from_rgb(0xf6, 0x82, 0x1f),
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Activity,
    Conversations,
    Logs,
}

// ─── App state ───────────────────────────────────────────────────────────────

pub struct BridgeApp {
    status_rx: watch::Receiver<String>,
    tasks_rx: watch::Receiver<Vec<TaskState>>,
    kill_tx: mpsc::Sender<Uuid>,
    ui_cmd_tx: mpsc::Sender<UiCmd>,
    log_rx: std::sync::mpsc::Receiver<String>,
    has_seen_bg_notice: bool,
    logs: VecDeque<String>,
    current_tab: Tab,
    show_unpair_confirm: bool,
    show_quit_confirm: bool,
    dark_mode: bool,
    chat_store: std::sync::Arc<ChatStore>,
    convs: Vec<StoredConversation>,
    convs_loaded: bool,
    selected_conv: Option<String>,
    selected_msgs: Vec<StoredMessage>,
    msgs_loaded_for: Option<String>,
    conv_search: String,
    conv_error: Option<String>,
    consent: std::sync::Arc<ConsentBroker>,
    // Real WS connection state (connected/RTT/reconnects/last_error). The
    // status_rx string channel is NOT authoritative for connection state.
    ws_health: std::sync::Arc<daemon_core::WsHealth>,
}

impl BridgeApp {
    pub fn new(
        _cc: &eframe::CreationContext<'_>,
        status_rx: watch::Receiver<String>,
        tasks_rx: watch::Receiver<Vec<TaskState>>,
        kill_tx: mpsc::Sender<Uuid>,
        ui_cmd_tx: mpsc::Sender<UiCmd>,
        log_rx: std::sync::mpsc::Receiver<String>,
        chat_store: std::sync::Arc<ChatStore>,
        consent: std::sync::Arc<ConsentBroker>,
        ws_health: std::sync::Arc<daemon_core::WsHealth>,
    ) -> Self {
        Self {
            status_rx,
            tasks_rx,
            kill_tx,
            ui_cmd_tx,
            log_rx,
            has_seen_bg_notice: false,
            logs: VecDeque::with_capacity(500),
            current_tab: Tab::Overview,
            show_unpair_confirm: false,
            show_quit_confirm: false,
            dark_mode: true,
            chat_store,
            convs: Vec::new(),
            convs_loaded: false,
            selected_conv: None,
            selected_msgs: Vec::new(),
            msgs_loaded_for: None,
            conv_search: String::new(),
            conv_error: None,
            consent,
            ws_health,
        }
    }

    // ── data plumbing (unchanged behavior) ───────────────────────────────

    fn refresh_conversations(&mut self) {
        let query = self.conv_search.trim();
        let result = if query.is_empty() {
            self.chat_store.list_conversations(200)
        } else {
            self.chat_store.search_conversations(query, 200)
        };
        match result {
            Ok(convs) => {
                self.convs = convs;
                self.convs_loaded = true;
                self.conv_error = None;
                if let Some(sel) = &self.selected_conv {
                    if !self.convs.iter().any(|c| &c.id == sel) {
                        self.selected_conv = self.convs.first().map(|c| c.id.clone());
                    }
                } else {
                    self.selected_conv = self.convs.first().map(|c| c.id.clone());
                }
                self.msgs_loaded_for = None;
                self.selected_msgs = Vec::new();
            }
            Err(e) => {
                self.conv_error = Some(format!("No se pudo leer el archivo local: {e}"));
            }
        }
    }

    fn refresh_messages(&mut self) {
        let sel = match &self.selected_conv {
            Some(s) => s.clone(),
            None => {
                self.selected_msgs = Vec::new();
                self.msgs_loaded_for = None;
                return;
            }
        };
        if self.msgs_loaded_for.as_deref() == Some(sel.as_str()) {
            return;
        }
        match self.chat_store.get_messages(&sel) {
            Ok(msgs) => {
                self.selected_msgs = msgs;
                self.msgs_loaded_for = Some(sel);
            }
            Err(e) => {
                self.conv_error = Some(format!("No se pudieron leer los mensajes: {e}"));
            }
        }
    }

    fn export_conversation(&self, conv_id: &str) -> Result<(), String> {
        let conv = self.convs.iter().find(|c| c.id == conv_id);
        let msgs = self.chat_store.get_messages(conv_id).unwrap_or_default();
        let title = conv
            .and_then(|c| c.title.clone())
            .unwrap_or_else(|| "conversacion".into());
        let safe_title: String = title
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .take(60)
            .collect();
        let json = serde_json::json!({
            "exportedAt": chrono::Utc::now().to_rfc3339(),
            "conversation": {
                "id": conv.map(|c| c.id.clone()).unwrap_or_default(),
                "title": title,
                "model": conv.and_then(|c| c.model.clone()),
                "provider": conv.and_then(|c| c.provider.clone()),
                "messages": msgs.iter().map(|m| serde_json::json!({
                    "id": m.id,
                    "role": m.role,
                    "content": m.content,
                    "createdAt": m.created_at,
                })).collect::<Vec<_>>(),
            }
        });
        let pretty = serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".into());
        let path = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .map(|home| std::path::PathBuf::from(home).join("Downloads"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(format!("synthhires-{safe_title}.json"));
        std::fs::write(&path, pretty).map_err(|e| format!("No se pudo exportar: {e}"))?;
        let _ = open::that(&path);
        Ok(())
    }

    // ── theme ────────────────────────────────────────────────────────────

    fn apply_theme(&self, ctx: &egui::Context) {
        let p = self.palette();
        let mut visuals = if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };

        visuals.panel_fill = p.bg;
        visuals.window_fill = p.surface;
        visuals.extreme_bg_color = p.surface2; // TextEdit backgrounds
        visuals.faint_bg_color = p.surface2;
        visuals.override_text_color = Some(p.text);
        visuals.window_stroke = egui::Stroke::new(1.0f32, p.border);
        visuals.selection.bg_fill = p.accent_soft;
        visuals.selection.stroke = egui::Stroke::new(1.0f32, p.accent);
        visuals.hyperlink_color = p.accent;

        for w in [
            &mut visuals.widgets.noninteractive,
            &mut visuals.widgets.inactive,
            &mut visuals.widgets.hovered,
            &mut visuals.widgets.active,
        ] {
            w.rounding = egui::Rounding::same(8.0);
        }
        visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0f32, p.border);
        visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0f32, p.text_dim);
        visuals.widgets.inactive.bg_fill = p.surface2;
        visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0f32, p.text);
        visuals.widgets.hovered.bg_fill = p.surface3;
        visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0f32, p.border_strong);
        visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0f32, p.text);
        visuals.widgets.active.bg_fill = p.accent_soft;
        visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0f32, p.accent);

        ctx.set_visuals(visuals);

        ctx.style_mut(|style| {
            use egui::{FontId, TextStyle};
            style.text_styles = [
                (TextStyle::Heading, FontId::proportional(19.0)),
                (TextStyle::Body, FontId::proportional(14.0)),
                (TextStyle::Button, FontId::proportional(14.0)),
                (TextStyle::Small, FontId::proportional(11.5)),
                (TextStyle::Monospace, FontId::monospace(12.5)),
            ]
            .into();
            style.spacing.item_spacing = egui::vec2(10.0, 10.0);
            style.spacing.button_padding = egui::vec2(12.0, 7.0);
        });
    }

    fn palette(&self) -> Palette {
        if self.dark_mode {
            dark_palette()
        } else {
            light_palette()
        }
    }
}

// ─── Custom widgets (real hover/active painting) ─────────────────────────────

struct BtnStyle<'a> {
    label: &'a str,
    base: egui::Color32,
    hover: egui::Color32,
    active: egui::Color32,
    text: egui::Color32,
    border: egui::Color32,
    border_hover: egui::Color32,
    rounding: f32,
    padding: egui::Vec2,
    size: f32,
}

impl<'a> BtnStyle<'a> {
    fn ghost(label: &'a str, p: &Palette) -> Self {
        Self {
            label,
            base: egui::Color32::TRANSPARENT,
            hover: p.surface2,
            active: p.surface3,
            text: p.text_dim,
            border: p.border,
            border_hover: p.border_strong,
            rounding: 8.0,
            padding: egui::vec2(12.0, 7.0),
            size: 13.5,
        }
    }
    fn primary(label: &'a str, p: &Palette) -> Self {
        Self {
            label,
            base: p.accent,
            hover: p.accent_hover,
            active: p.accent_active,
            text: egui::Color32::WHITE,
            border: egui::Color32::TRANSPARENT,
            border_hover: egui::Color32::TRANSPARENT,
            rounding: 8.0,
            padding: egui::vec2(14.0, 7.0),
            size: 13.5,
        }
    }
    fn danger(label: &'a str, p: &Palette) -> Self {
        Self {
            label,
            base: p.danger,
            hover: p.danger_hover,
            active: p.danger,
            text: egui::Color32::WHITE,
            border: egui::Color32::TRANSPARENT,
            border_hover: egui::Color32::TRANSPARENT,
            rounding: 8.0,
            padding: egui::vec2(14.0, 7.0),
            size: 13.5,
        }
    }
    fn danger_ghost(label: &'a str, p: &Palette) -> Self {
        Self {
            label,
            base: egui::Color32::TRANSPARENT,
            hover: p.danger_soft,
            active: p.danger_soft,
            text: p.danger,
            border: p.border,
            border_hover: p.danger,
            rounding: 8.0,
            padding: egui::vec2(12.0, 7.0),
            size: 13.5,
        }
    }
    fn quiet(label: &'a str, p: &Palette) -> Self {
        // borderless text button for tertiary actions
        Self {
            label,
            base: egui::Color32::TRANSPARENT,
            hover: p.surface2,
            active: p.surface3,
            text: p.text_dim,
            border: egui::Color32::TRANSPARENT,
            border_hover: egui::Color32::TRANSPARENT,
            rounding: 8.0,
            padding: egui::vec2(10.0, 6.0),
            size: 13.0,
        }
    }
}

/// Painted button with genuine hover/active states.
fn button(ui: &mut egui::Ui, s: &BtnStyle<'_>) -> egui::Response {
    let font = egui::FontId::proportional(s.size);
    let galley = ui
        .painter()
        .layout_no_wrap(s.label.to_string(), font, s.text);
    let desired = galley.size() + s.padding * 2.0;
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());

    let (fill, border) = if resp.is_pointer_button_down_on() {
        (s.active, s.border)
    } else if resp.hovered() {
        (s.hover, s.border_hover)
    } else {
        (s.base, s.border)
    };
    if fill != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, s.rounding, fill);
    }
    if border != egui::Color32::TRANSPARENT {
        ui.painter()
            .rect_stroke(rect, s.rounding, egui::Stroke::new(1.0f32, border));
    }
    let text_pos = rect.center() - galley.size() * 0.5;
    ui.painter().galley(text_pos, galley, s.text);
    resp
}

/// Small rounded status chip.
fn chip(ui: &mut egui::Ui, text: &str, fg: egui::Color32, bg: egui::Color32) {
    let font = egui::FontId::proportional(11.0);
    let galley = ui.painter().layout_no_wrap(text.to_string(), font, fg);
    let pad = egui::vec2(8.0, 4.0);
    let desired = galley.size() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    ui.painter().rect_filled(rect, 6.0, bg);
    ui.painter()
        .galley(rect.center() - galley.size() * 0.5, galley, fg);
}

/// Uppercase section label.
fn section_label(ui: &mut egui::Ui, text: &str, p: &Palette) {
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .size(10.5)
            .strong()
            .color(p.text_faint),
    );
}

/// Card container.
fn card(ui: &mut egui::Ui, p: &Palette, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(p.surface)
        .rounding(10.0)
        .stroke(egui::Stroke::new(1.0f32, p.border))
        .inner_margin(egui::Margin::same(16.0))
        .show(ui, |ui| {
            add(ui);
        });
}

/// Breathing status dot (glow + core).
fn status_dot(ui: &mut egui::Ui, _p: &Palette, color: egui::Color32, breathing: bool) {
    let t = ui.input(|i| i.time) as f32;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let c = rect.center();
    let glow_alpha = if breathing {
        (70.0 + 50.0 * (t * 2.6).sin()).max(30.0)
    } else {
        0.0
    };
    if glow_alpha > 0.0 {
        let glow = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), glow_alpha as u8);
        ui.painter().circle_filled(c, 7.0, glow);
    }
    let core_alpha = if breathing {
        (200.0 + 55.0 * (t * 2.6).sin()).min(255.0)
    } else {
        255.0
    };
    let core = egui::Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), core_alpha as u8);
    ui.painter().circle_filled(c, 3.6, core);
}

/// Connection state derived from real WsHealth.
#[allow(clippy::type_complexity)]
fn connection_state(
    status_rx: &watch::Receiver<String>,
    h: &daemon_core::WsHealthSnapshot,
) -> (String, egui::Color32, bool) {
    let pairing_status = status_rx.borrow().clone();
    if h.connected {
        ("Conectado".to_string(), egui::Color32::from_rgb(0x3d, 0xd6, 0x8c), true)
    } else if pairing_status.contains("Esperando emparejamiento") {
        ("Esperando emparejamiento".to_string(), egui::Color32::from_rgb(0xf5, 0xa6, 0x23), true)
    } else if h.reconnects > 0 || !h.last_error.is_empty() {
        ("Reconectando".to_string(), egui::Color32::from_rgb(0xf5, 0xa6, 0x23), true)
    } else {
        ("Conectando".to_string(), egui::Color32::from_rgb(0x9b, 0xa1, 0xab), true)
    }
}

fn fmt_duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

fn fmt_relative(epoch_ms: u64, now_ms: u64) -> String {
    if epoch_ms == 0 {
        return "—".into();
    }
    let diff = now_ms.saturating_sub(epoch_ms) / 1000;
    format!("hace {}", fmt_duration(diff))
}

// ─── Modal scaffolding (scrim + centered card) ───────────────────────────────

fn show_modal(ctx: &egui::Context, id: &str, width: f32, body: impl FnOnce(&mut egui::Ui, &Palette)) {
    // Scrim
    egui::Area::new(egui::Id::new(format!("{id}-scrim")))
        .order(egui::Order::Middle)
        .fixed_pos(ctx.screen_rect().left_top())
        .show(ctx, |ui| {
            let size = ctx.screen_rect().size();
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter()
                .rect_filled(rect, 0.0, egui::Color32::from_black_alpha(140));
        });
    // Card
    egui::Area::new(egui::Id::new(id))
        .order(egui::Order::Foreground)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let p = if ui.ctx().style().visuals.dark_mode {
                dark_palette()
            } else {
                light_palette()
            };
            egui::Frame::none()
                .fill(p.surface)
                .rounding(14.0)
                .stroke(egui::Stroke::new(1.0f32, p.border_strong))
                .inner_margin(egui::Margin::same(22.0))
                .show(ui, |ui| {
                    ui.set_width(width);
                    body(ui, &p);
                });
        });
}

// ─── App impl ────────────────────────────────────────────────────────────────

impl BridgeApp {
    fn nav_item(
        &mut self,
        ui: &mut egui::Ui,
        label: &str,
        tab: Tab,
        p: &Palette,
        badge: Option<usize>,
    ) {
        let selected = self.current_tab == tab;
        let full = ui.available_size_before_wrap().x;
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(full, 34.0),
            egui::Sense::click(),
        );
        let (fill, text_color) = if selected {
            (p.accent_soft, p.text)
        } else if resp.hovered() {
            (p.surface2, p.text)
        } else {
            (egui::Color32::TRANSPARENT, p.text_dim)
        };
        if fill != egui::Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, 8.0, fill);
        }
        if selected {
            // Left accent bar
            let bar = egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.center().y - 9.0),
                egui::vec2(3.0, 18.0),
            );
            ui.painter().rect_filled(bar, 2.0, p.accent);
        }
        ui.painter().text(
            egui::pos2(rect.left() + 14.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(13.5),
            text_color,
        );
        if let Some(n) = badge {
            if n > 0 {
                ui.painter().text(
                    egui::pos2(rect.right() - 14.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    format!("{n}"),
                    egui::FontId::proportional(11.5),
                    if selected { p.accent } else { p.text_faint },
                );
            }
        }
        if resp.clicked() {
            self.current_tab = tab;
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::SidePanel::left("nav")
            .exact_width(216.0)
            .frame(egui::Frame::none().fill(p.surface).inner_margin(egui::Margin::same(12.0)))
            .show(ctx, |ui| {
                // Brand
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.painter().circle_filled(
                        ui.cursor().left_center() + egui::vec2(9.0, 0.0),
                        5.0,
                        p.brand_orange,
                    );
                    ui.add_space(22.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("SynthHires Bridge")
                                .size(15.0)
                                .strong()
                                .color(p.text),
                        );
                        ui.label(
                            egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .size(10.5)
                                .color(p.text_faint),
                        );
                    });
                });

                ui.add_space(18.0);
                section_label(ui, "Consola", p);
                ui.add_space(4.0);

                let tasks = self.tasks_rx.borrow().clone();
                let running = tasks
                    .iter()
                    .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Cancelling))
                    .count();

                self.nav_item(ui, "Resumen", Tab::Overview, p, None);
                self.nav_item(ui, "Actividad", Tab::Activity, p, Some(running));
                self.nav_item(ui, "Conversaciones", Tab::Conversations, p, None);
                self.nav_item(ui, "Registros", Tab::Logs, p, None);

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.add_space(12.0);
                    // Danger zone
                    let mut quit_style = BtnStyle::danger_ghost("Salir del Bridge", p);
                    quit_style.size = 13.0;
                    if button(ui, &quit_style).clicked() {
                        self.show_quit_confirm = true;
                    }
                    let mut unpair_style = BtnStyle::danger_ghost("Desvincular dispositivo", p);
                    unpair_style.padding = egui::vec2(12.0, 7.0);
                    if button(ui, &unpair_style).clicked() {
                        self.show_unpair_confirm = true;
                    }
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add_space(6.0);
                    let dash = button(ui, &BtnStyle { padding: egui::vec2(12.0, 7.0), ..BtnStyle::quiet("Abrir dashboard", p) });
                    if dash.clicked() {
                        let _ = self.ui_cmd_tx.try_send(UiCmd::OpenDashboard);
                    }
                    let min = button(ui, &BtnStyle { padding: egui::vec2(12.0, 7.0), ..BtnStyle::quiet("Minimizar a la bandeja", p) });
                    if min.clicked() {
                        if !self.has_seen_bg_notice {
                            crate::tray::show_background_notice();
                            self.has_seen_bg_notice = true;
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                    }
                });
            });
    }

    fn header(&mut self, ctx: &egui::Context, p: &Palette) {
        egui::TopBottomPanel::top("header")
            .exact_height(58.0)
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(20.0, 0.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Left: current view name
                    let view = match self.current_tab {
                        Tab::Overview => "Resumen",
                        Tab::Activity => "Actividad",
                        Tab::Conversations => "Conversaciones",
                        Tab::Logs => "Registros",
                    };
                    ui.label(
                        egui::RichText::new(view)
                            .size(15.0)
                            .strong()
                            .color(p.text),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // Theme toggle
                        let theme_label = if self.dark_mode { "☀" } else { "☾" };
                        let t = button(
                            ui,
                            &BtnStyle {
                                padding: egui::vec2(9.0, 5.0),
                                ..BtnStyle::quiet(theme_label, p)
                            },
                        );
                        if t.clicked() {
                            self.dark_mode = !self.dark_mode;
                        }

                        // Connection pill
                        let h = self.ws_health.snapshot();
                        let (label, color, _breathing) = connection_state(&self.status_rx, &h);
                        let detail = if h.connected && h.last_rtt_ms > 0 {
                            format!(" · {} ms", h.last_rtt_ms)
                        } else if !h.connected && h.reconnects > 0 {
                            format!(" · {}", h.reconnects)
                        } else {
                            String::new()
                        };
                        let pill_text = format!("{label}{detail}");
                        let font = egui::FontId::proportional(12.5);
                        let galley = ui
                            .painter()
                            .layout_no_wrap(pill_text.clone(), font.clone(), color);
                        let pad_x = 12.0;
                        let width = galley.size().x + pad_x * 2.0 + 16.0;
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(width, 28.0),
                            egui::Sense::click(),
                        );
                        let bg = if self.dark_mode {
                            egui::Color32::from_black_alpha(60)
                        } else {
                            egui::Color32::from_black_alpha(8)
                        };
                        let hover_bg = p.surface2;
                        let fill = if resp.hovered() { hover_bg } else { bg };
                        ui.painter().rect_filled(rect, 14.0, fill);
                        ui.painter()
                            .rect_stroke(rect, 14.0, egui::Stroke::new(1.0f32, p.border));
                        // dot
                        let dot_c = egui::pos2(rect.left() + 12.0, rect.center().y);
                        let t_now = ui.input(|i| i.time) as f32;
                        let glow_alpha = (70.0 + 50.0 * (t_now * 2.6).sin()).max(30.0);
                        let glow = egui::Color32::from_rgba_unmultiplied(
                            color.r(),
                            color.g(),
                            color.b(),
                            glow_alpha as u8,
                        );
                        ui.painter().circle_filled(dot_c, 6.0, glow);
                        ui.painter().circle_filled(dot_c, 3.2, color);
                        ui.painter().galley(
                            egui::pos2(rect.left() + 22.0, rect.center().y - galley.size().y / 2.0),
                            galley,
                            color,
                        );
                        if resp.clicked() {
                            self.current_tab = Tab::Overview;
                        }
                    });
                });
            });
    }

    fn overview_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(6.0);
        let h = self.ws_health.snapshot();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let (label, color, breathing) = connection_state(&self.status_rx, &h);

        // Hero connection card
        card(ui, p, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                status_dot(ui, p, color, breathing);
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(&label)
                        .size(17.0)
                        .strong()
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    chip(ui, "WEBSOCKET", p.text_faint, p.surface2);
                });
            });
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            // Meta grid (2 columns)
            egui::Grid::new("conn_meta")
                .num_columns(4)
                .spacing([24.0, 8.0])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Latencia").size(12.0).color(p.text_dim));
                    ui.label(
                        egui::RichText::new(if h.last_rtt_ms > 0 { format!("{} ms", h.last_rtt_ms) } else { "—".into() })
                            .size(13.0)
                            .color(p.text),
                    );
                    ui.label(egui::RichText::new("Reconexiones").size(12.0).color(p.text_dim));
                    ui.label(
                        egui::RichText::new(format!("{}", h.reconnects))
                            .size(13.0)
                            .color(if h.reconnects > 0 { p.warning } else { p.text }),
                    );
                    ui.end_row();

                    ui.label(egui::RichText::new("Última conexión").size(12.0).color(p.text_dim));
                    ui.label(
                        egui::RichText::new(if h.last_connected_at_ms > 0 {
                            fmt_relative(h.last_connected_at_ms, now_ms)
                        } else {
                            "—".into()
                        })
                        .size(13.0)
                        .color(p.text),
                    );
                    ui.label(egui::RichText::new("Heartbeat").size(12.0).color(p.text_dim));
                    ui.label(
                        egui::RichText::new(if h.last_heartbeat_ack_at_ms > 0 {
                            fmt_relative(h.last_heartbeat_ack_at_ms, now_ms)
                        } else {
                            "—".into()
                        })
                        .size(13.0)
                        .color(p.text),
                    );
                    ui.end_row();
                });

            if !h.connected && !h.last_error.is_empty() {
                ui.add_space(8.0);
                let err: String = h.last_error.chars().take(120).collect();
                egui::Frame::none()
                    .fill(p.danger_soft)
                    .rounding(8.0)
                    .inner_margin(egui::Margin::symmetric(10.0, 7.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(err)
                                .size(12.0)
                                .color(p.danger)
                                .family(egui::FontFamily::Monospace),
                        );
                    });
            }
        });

        ui.add_space(12.0);

        // Quick actions
        ui.horizontal(|ui| {
            if button(ui, &BtnStyle::primary("Abrir dashboard", p)).clicked() {
                let _ = self.ui_cmd_tx.try_send(UiCmd::OpenDashboard);
            }
            if button(ui, &BtnStyle::ghost("Minimizar a la bandeja", p)).clicked() {
                if !self.has_seen_bg_notice {
                    crate::tray::show_background_notice();
                    self.has_seen_bg_notice = true;
                }
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
        });

        ui.add_space(16.0);

        // Running snapshot
        section_label(ui, "Actividad reciente", p);
        ui.add_space(6.0);
        let tasks = self.tasks_rx.borrow().clone();
        let running: Vec<&TaskState> = tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Running | TaskStatus::Cancelling))
            .collect();
        if running.is_empty() {
            card(ui, p, |ui| {
                ui.set_min_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.add_space(14.0);
                    ui.label(
                        egui::RichText::new("Sin tareas en ejecución")
                            .size(13.5)
                            .color(p.text_dim),
                    );
                    ui.label(
                        egui::RichText::new("El agente está inactivo en este PC.")
                            .size(12.0)
                            .color(p.text_faint),
                    );
                    ui.add_space(14.0);
                });
            });
        } else {
            for task in running {
                self.task_card(ui, task, p);
                ui.add_space(8.0);
            }
        }
    }

    fn task_card(&mut self, ui: &mut egui::Ui, task: &TaskState, p: &Palette) {
        let (kind_letter, kind_color) = match task.kind {
            TaskKind::ShellExec => ("S", p.accent),
            TaskKind::FileRead => ("R", p.success),
            TaskKind::FileWrite => ("W", p.warning),
            TaskKind::DbProxy => ("D", egui::Color32::from_rgb(0xc0, 0x6b, 0xf6)),
            TaskKind::Other(_) => ("·", p.text_faint),
        };

        egui::Frame::none()
            .fill(p.surface)
            .rounding(10.0)
            .stroke(egui::Stroke::new(1.0f32, p.border))
            .inner_margin(egui::Margin::same(14.0))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Kind glyph square
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::hover());
                    let sq_bg = egui::Color32::from_rgba_unmultiplied(
                        kind_color.r(),
                        kind_color.g(),
                        kind_color.b(),
                        30,
                    );
                    ui.painter().rect_filled(rect, 8.0, sq_bg);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        kind_letter,
                        egui::FontId::proportional(14.0),
                        kind_color,
                    );
                    ui.add_space(4.0);

                    ui.vertical(|ui| {
                        let desc: String = task.description.chars().take(70).collect();
                        ui.label(
                            egui::RichText::new(desc)
                                .size(13.5)
                                .strong()
                                .color(p.text),
                        );
                        let elapsed = task
                            .finished_at
                            .unwrap_or_else(std::time::Instant::now)
                            .duration_since(task.started_at_instant);
                        let started_ms = task.started_at_utc.timestamp_millis();
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · iniciado {}",
                                fmt_duration(elapsed.as_secs()),
                                fmt_date(started_ms)
                            ))
                            .size(11.5)
                            .color(p.text_faint),
                        );
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match &task.status {
                            TaskStatus::Running => {
                                if matches!(&task.kind, TaskKind::ShellExec) {
                                    if button(
                                        ui,
                                        &BtnStyle {
                                            padding: egui::vec2(10.0, 5.0),
                                            ..BtnStyle::danger_ghost("Detener", p)
                                        },
                                    )
                                    .clicked()
                                    {
                                        let _ = self.kill_tx.try_send(task.id);
                                    }
                                }
                                chip(ui, "EN CURSO", p.accent, p.accent_soft);
                            }
                            TaskStatus::Cancelling => chip(ui, "DETENIENDO", p.warning, p.warning_soft),
                            TaskStatus::Completed(_) => chip(ui, "OK", p.success, p.success_soft),
                            TaskStatus::Killed => chip(ui, "INTERRUMPIDO", p.warning, p.warning_soft),
                            TaskStatus::Failed(_) => chip(ui, "ERROR", p.danger, p.danger_soft),
                        }
                    });
                });
                if let TaskStatus::Failed(err) = &task.status {
                    ui.add_space(6.0);
                    let err: String = err.chars().take(160).collect();
                    ui.label(
                        egui::RichText::new(err)
                            .size(12.0)
                            .color(p.danger)
                            .family(egui::FontFamily::Monospace),
                    );
                }
            });
    }

    fn activity_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(6.0);
        let tasks = self.tasks_rx.borrow().clone();
        if tasks.is_empty() {
            card(ui, p, |ui| {
                ui.set_min_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.add_space(40.0);
                    ui.label(
                        egui::RichText::new("No hay actividad todavía")
                            .size(15.0)
                            .strong()
                            .color(p.text_dim),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("Cuando el agente ejecute acciones en este PC,\naparecerán aquí en tiempo real.")
                            .size(12.5)
                            .color(p.text_faint),
                    );
                    ui.add_space(40.0);
                });
            });
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false; 2])
            .show(ui, |ui| {
                for task in &tasks {
                    self.task_card(ui, task, p);
                    ui.add_space(8.0);
                }
            });
    }

    fn logs_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("Transparencia total del daemon")
                    .size(13.0)
                    .color(p.text_dim),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, &BtnStyle::ghost("Limpiar", p)).clicked() {
                    self.logs.clear();
                }
            });
        });
        ui.add_space(8.0);

        egui::Frame::none()
            .fill(p.surface)
            .rounding(10.0)
            .stroke(egui::Stroke::new(1.0f32, p.border))
            .inner_margin(egui::Margin::same(12.0))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        if self.logs.is_empty() {
                            ui.label(
                                egui::RichText::new("Sin registros aún.")
                                    .size(12.5)
                                    .color(p.text_faint),
                            );
                        }
                        for log in &self.logs {
                            let color = if log.contains("ERROR") {
                                p.danger
                            } else if log.contains("WARN") {
                                p.warning
                            } else if log.contains("DEBUG") {
                                p.text_faint
                            } else {
                                p.text_dim
                            };
                            ui.label(
                                egui::RichText::new(log)
                                    .family(egui::FontFamily::Monospace)
                                    .size(12.0)
                                    .color(color),
                            );
                        }
                    });
            });
    }

    fn conversations_tab(&mut self, ui: &mut egui::Ui, p: &Palette) {
        if !self.convs_loaded {
            self.refresh_conversations();
        }
        self.refresh_messages();

        egui::SidePanel::left("conv_list")
            .resizable(false)
            .exact_width(300.0)
            .frame(egui::Frame::none().inner_margin(egui::Margin::same(0.0)))
            .show_inside(ui, |ui| {
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.conv_search)
                            .hint_text("Buscar título o contenido…")
                            .desired_width(ui.available_width() - 52.0),
                    );
                    if button(ui, &BtnStyle { padding: egui::vec2(9.0, 6.0), ..BtnStyle::ghost("↻", p) })
                        .clicked()
                    {
                        self.refresh_conversations();
                    }
                });
                if let Some(err) = &self.conv_error {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(err).size(12.0).color(p.danger));
                }
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        if self.convs.is_empty() {
                            ui.add_space(20.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("Sin conversaciones archivadas")
                                        .size(13.0)
                                        .color(p.text_dim),
                                );
                                ui.label(
                                    egui::RichText::new("Se sincronizan automáticamente.")
                                        .size(11.5)
                                        .color(p.text_faint),
                                );
                            });
                        }
                        // Snapshot the fields needed for painting so the
                        // mutable self (selection refresh) is free afterwards.
                        let conv_rows: Vec<(String, String, bool, i64)> = self
                            .convs
                            .iter()
                            .map(|c| {
                                (
                                    c.id.clone(),
                                    c.title.clone().unwrap_or_else(|| c.id.clone()),
                                    c.is_pinned,
                                    c.updated_at,
                                )
                            })
                            .collect();
                        let selected_id = self.selected_conv.clone();
                        for (conv_id, title, pinned, updated) in conv_rows {
                            let selected = selected_id.as_deref() == Some(conv_id.as_str());
                            let full = ui.available_width();
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(full - 16.0, 52.0),
                                egui::Sense::click(),
                            );
                            let (fill, border) = if selected {
                                (p.accent_soft, p.accent)
                            } else if resp.hovered() {
                                (p.surface2, p.border)
                            } else {
                                (egui::Color32::TRANSPARENT, egui::Color32::TRANSPARENT)
                            };
                            if fill != egui::Color32::TRANSPARENT {
                                ui.painter().rect_filled(rect, 8.0, fill);
                            }
                            if border != egui::Color32::TRANSPARENT {
                                ui.painter()
                                    .rect_stroke(rect, 8.0, egui::Stroke::new(1.0f32, border));
                            }
                            ui.painter().text(
                                egui::pos2(rect.left() + 12.0, rect.top() + 14.0),
                                egui::Align2::LEFT_CENTER,
                                title.chars().take(28).collect::<String>(),
                                egui::FontId::proportional(13.0),
                                if selected { p.text } else { p.text_dim },
                            );
                            ui.painter().text(
                                egui::pos2(rect.left() + 12.0, rect.bottom() - 14.0),
                                egui::Align2::LEFT_CENTER,
                                fmt_date(updated),
                                egui::FontId::proportional(10.5),
                                p.text_faint,
                            );
                            if pinned {
                                ui.painter().text(
                                    egui::pos2(rect.right() - 12.0, rect.center().y),
                                    egui::Align2::RIGHT_CENTER,
                                    "★",
                                    egui::FontId::proportional(12.0),
                                    p.brand_orange,
                                );
                            }
                            if resp.clicked() {
                                self.selected_conv = Some(conv_id);
                                self.msgs_loaded_for = None;
                                self.refresh_messages();
                            }
                            ui.add_space(2.0);
                        }
                    });
            });

        // Detail
        ui.add_space(10.0);
        if let Some(sel) = self.selected_conv.clone() {
            let conv_meta: Option<(String, String, String)> = self
                .convs
                .iter()
                .find(|c| c.id == sel)
                .map(|c| {
                    (
                        c.title.clone().unwrap_or_else(|| c.id.clone()),
                        c.model.clone().unwrap_or_default(),
                        c.provider.clone().unwrap_or_default(),
                    )
                });
            let title = conv_meta
                .as_ref()
                .map(|m| m.0.clone())
                .unwrap_or_else(|| sel.clone());

            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(p.text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if button(ui, &BtnStyle::danger_ghost("Eliminar", p)).clicked() {
                        match self.chat_store.delete_conversation(&sel) {
                            Ok(_) => {
                                self.selected_conv = None;
                                self.msgs_loaded_for = None;
                                self.selected_msgs = Vec::new();
                                self.refresh_conversations();
                            }
                            Err(e) => self.conv_error = Some(format!("No se pudo eliminar: {e}")),
                        }
                    }
                    if button(ui, &BtnStyle::ghost("Exportar JSON", p)).clicked() {
                        if let Err(e) = self.export_conversation(&sel) {
                            self.conv_error = Some(e);
                        }
                    }
                });
            });
            if let Some((_, model, provider)) = &conv_meta {
                if !model.is_empty() && !provider.is_empty() {
                    ui.label(
                        egui::RichText::new(format!(
                            "{provider} · {model} · {} mensajes",
                            self.selected_msgs.len()
                        ))
                        .size(11.5)
                        .color(p.text_faint),
                    );
                }
            }
            ui.add_space(8.0);

            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if self.selected_msgs.is_empty() {
                        ui.label(
                            egui::RichText::new("(sin mensajes)")
                                .size(12.5)
                                .color(p.text_faint),
                        );
                    }
                    for m in &self.selected_msgs {
                        let (role_label, fg, bg) = match m.role.as_str() {
                            "user" => ("TÚ", p.accent, p.accent_soft),
                            "assistant" => ("AGENTE", p.success, p.success_soft),
                            "system" => ("SISTEMA", p.warning, p.warning_soft),
                            _ => ("—", p.text_dim, p.surface2),
                        };
                        egui::Frame::none()
                            .fill(bg)
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(10.0))
                            .show(ui, |ui| {
                                ui.set_min_width(ui.available_width());
                                chip(ui, role_label, fg, egui::Color32::TRANSPARENT);
                                ui.add_space(2.0);
                                let content: String = m.content.chars().take(4000).collect();
                                ui.label(
                                    egui::RichText::new(content)
                                        .size(12.5)
                                        .color(p.text),
                                );
                            });
                        ui.add_space(6.0);
                    }
                });
        } else {
            ui.centered_and_justified(|ui| {
                ui.label(
                    egui::RichText::new("Selecciona una conversación")
                        .size(13.5)
                        .color(p.text_faint),
                );
            });
        }
    }
}

// ─── eframe::App ─────────────────────────────────────────────────────────────

impl eframe::App for BridgeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain the log channel
        while let Ok(msg) = self.log_rx.try_recv() {
            if self.logs.len() >= 500 {
                self.logs.pop_front();
            }
            self.logs.push_back(msg);
        }
        ctx.request_repaint_after(Duration::from_millis(150));

        // Intercept native close to minimize instead
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            if !self.has_seen_bg_notice {
                crate::tray::show_background_notice();
                self.has_seen_bg_notice = true;
            }
        }

        self.apply_theme(ctx);
        let p = self.palette();

        // ── Modals (above everything) ──
        if self.show_unpair_confirm {
            show_modal(ctx, "unpair-modal", 420.0, |ui, p| {
                ui.label(
                    egui::RichText::new("Desvincular dispositivo")
                        .size(16.0)
                        .strong()
                        .color(p.text),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "Se eliminará la credencial de emparejamiento de forma segura y se cerrará la conexión actual. El agente perderá acceso a este PC.",
                    )
                    .size(13.0)
                    .color(p.text_dim),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if button(ui, &BtnStyle::ghost("Cancelar", p)).clicked() {
                        self.show_unpair_confirm = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, &BtnStyle::danger("Desvincular", p)).clicked() {
                            let _ = self.ui_cmd_tx.try_send(UiCmd::Unpair);
                            self.show_unpair_confirm = false;
                        }
                    });
                });
            });
        }

        if self.show_quit_confirm {
            show_modal(ctx, "quit-modal", 420.0, |ui, p| {
                ui.label(
                    egui::RichText::new("Cerrar Bridge")
                        .size(16.0)
                        .strong()
                        .color(p.text),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "El daemon se detendrá por completo: el agente perderá acceso al sistema y las tareas en curso podrían fallar.",
                    )
                    .size(13.0)
                    .color(p.text_dim),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if button(ui, &BtnStyle::ghost("Cancelar", p)).clicked() {
                        self.show_quit_confirm = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, &BtnStyle::danger("Cerrar", p)).clicked() {
                            std::process::exit(0);
                        }
                    });
                });
            });
        }

        // Consent prompt overlay
        let pending_consents = self.consent.pending();
        if !pending_consents.is_empty() {
            let prompt = pending_consents[0].clone();
            show_modal(ctx, "consent-modal", 480.0, |ui, p| {
                ui.horizontal(|ui| {
                    status_dot(ui, p, p.warning, false);
                    ui.label(
                        egui::RichText::new("Consentimiento requerido")
                            .size(16.0)
                            .strong()
                            .color(p.text),
                    );
                });
                ui.add_space(10.0);
                egui::Frame::none()
                    .fill(p.surface2)
                    .rounding(8.0)
                    .inner_margin(egui::Margin::same(10.0))
                    .show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(&prompt.capability)
                                .size(12.0)
                                .strong()
                                .color(p.accent)
                                .family(egui::FontFamily::Monospace),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&prompt.summary)
                                .size(13.0)
                                .color(p.text_dim),
                        );
                        if let Some(path) = &prompt.path {
                            ui.add_space(4.0);
                            ui.label(
                                egui::RichText::new(path)
                                    .size(11.5)
                                    .color(p.text_faint)
                                    .family(egui::FontFamily::Monospace),
                            );
                        }
                    });
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if button(ui, &BtnStyle::ghost("Denegar", p)).clicked() {
                        self.consent.answer(
                            &prompt.action_id,
                            ConsentAnswer { approved: false, remember: false },
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button(ui, &BtnStyle::primary("Permitir", p)).clicked() {
                            self.consent.answer(
                                &prompt.action_id,
                                ConsentAnswer { approved: true, remember: false },
                            );
                        }
                        if prompt.path.is_some() {
                            if button(ui, &BtnStyle::quiet("Permitir siempre en esta carpeta", p))
                                .clicked()
                            {
                                self.consent.answer(
                                    &prompt.action_id,
                                    ConsentAnswer { approved: true, remember: true },
                                );
                            }
                        }
                    });
                });
            });
        }

        // ── Layout: sidebar | header / content ──
        self.sidebar(ctx, &p);
        self.header(ctx, &p);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(p.bg)
                    .inner_margin(egui::Margin::symmetric(20.0, 0.0)),
            )
            .show(ctx, |ui| match self.current_tab {
                Tab::Overview => self.overview_tab(ui, &p),
                Tab::Activity => self.activity_tab(ui, &p),
                Tab::Conversations => self.conversations_tab(ui, &p),
                Tab::Logs => self.logs_tab(ui, &p),
            });
    }
}

fn fmt_date(epoch_ms: i64) -> String {
    if epoch_ms <= 0 {
        return "—".into();
    }
    let secs = epoch_ms / 1000;
    match chrono::DateTime::from_timestamp(secs, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%d/%m/%Y %H:%M")
            .to_string(),
        None => "—".into(),
    }
}

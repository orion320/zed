//! Type-Y-to-confirm modal shown when the user quits Zed while one or more
//! items report `should_confirm_close` (notably terminals with a non-idle
//! foreground process such as Claude Code, ssh, vim, builds, REPLs).
//!
//! The modal communicates its outcome back to the caller via a oneshot
//! channel: it sends `true` when the user explicitly types Y, and `false`
//! on any other dismissal (escape, mouse-out, modal swap, drop).

use futures::channel::oneshot;
use gpui::{DismissEvent, EventEmitter, FocusHandle, Focusable, KeyDownEvent, SharedString};
use ui::{Headline, HeadlineSize, ListBulletItem, prelude::*};

use crate::ModalView;

pub struct QuitConfirmModal {
    running: Vec<SharedString>,
    confirm_tx: Option<oneshot::Sender<bool>>,
    focus_handle: FocusHandle,
}

impl QuitConfirmModal {
    pub fn new(
        running: Vec<SharedString>,
        confirm_tx: oneshot::Sender<bool>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            running,
            confirm_tx: Some(confirm_tx),
            focus_handle: cx.focus_handle(),
        }
    }

    fn confirm(&mut self, cx: &mut Context<Self>) {
        if let Some(tx) = self.confirm_tx.take() {
            // Receiver may have been dropped if the caller already gave up;
            // a send failure there is benign.
            let _ = tx.send(true);
        }
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(tx) = self.confirm_tx.take() {
            let _ = tx.send(false);
        }
        cx.emit(DismissEvent);
    }

    fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Only a Y keystroke (no modifiers) confirms; everything else cancels.
        let m = &event.keystroke.modifiers;
        let plain = !(m.control || m.alt || m.platform || m.shift || m.function);
        if plain && event.keystroke.key.eq_ignore_ascii_case("y") {
            self.confirm(cx);
        } else if event.keystroke.key == "escape" {
            self.cancel(cx);
        }
    }
}

impl Drop for QuitConfirmModal {
    fn drop(&mut self) {
        // If the modal is torn down without an explicit answer (e.g. another
        // modal replaced it), treat that as cancel so the quit aborts safely.
        if let Some(tx) = self.confirm_tx.take() {
            let _ = tx.send(false);
        }
    }
}

impl Focusable for QuitConfirmModal {
    fn focus_handle(&self, _: &ui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for QuitConfirmModal {}

impl ModalView for QuitConfirmModal {
    fn fade_out_background(&self) -> bool {
        true
    }
}

impl Render for QuitConfirmModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.running.len();
        let title: SharedString = if count == 1 {
            "A terminal is running:".into()
        } else {
            format!("{} terminals are running:", count).into()
        };

        v_flex()
            .key_context("QuitConfirmModal")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .elevation_3(cx)
            .w(rems(32.))
            .bg(cx.theme().colors().elevated_surface_background)
            .overflow_hidden()
            .child(
                v_flex()
                    .p_3()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(IconName::Warning).color(Color::Warning))
                            .child(Headline::new(title).size(HeadlineSize::Small)),
                    )
                    .child(
                        v_flex().children(
                            self.running
                                .iter()
                                .cloned()
                                .map(|name| ListBulletItem::new(name)),
                        ),
                    ),
            )
            .child(
                v_flex().p_3().gap_1().child(
                    Label::new("Press Y to close anyway, or any other key to cancel.")
                        .color(Color::Muted),
                ),
            )
    }
}

/// Collects display labels for items in a single MultiWorkspace whose
/// `should_confirm_close` is true. Walks both the main editor panes and
/// the panes inside docked panels (e.g. the bottom terminal panel), since
/// they are independent pane groups.
pub fn labels_for_multi_workspace(
    multi_workspace: &crate::MultiWorkspace,
    cx: &gpui::App,
) -> Vec<SharedString> {
    let mut out: Vec<SharedString> = Vec::new();
    let mut push_pane = |pane: &gpui::Entity<crate::Pane>, cx: &gpui::App| {
        for item in pane.read(cx).items() {
            if item.should_confirm_close(cx) {
                out.push(
                    item.close_confirm_label(cx)
                        .unwrap_or_else(|| "Running process".into()),
                );
            }
        }
    };

    for workspace in multi_workspace.workspaces() {
        let workspace = workspace.read(cx);
        for pane in workspace.panes() {
            push_pane(pane, cx);
        }
        for dock in [workspace.left_dock(), workspace.bottom_dock(), workspace.right_dock()] {
            for panel in dock.read(cx).panels() {
                if let Some(pane) = panel.pane(cx) {
                    push_pane(&pane, cx);
                }
            }
        }
    }
    out
}

/// App-wide variant: walks every MultiWorkspace window in the running app.
/// Returns an empty `Vec` when nothing needs confirmation (caller should
/// skip the modal).
pub fn collect_close_confirm_labels(
    workspaces: &[gpui::WindowHandle<crate::MultiWorkspace>],
    cx: &mut gpui::AsyncApp,
) -> Vec<SharedString> {
    let mut labels = Vec::new();
    for window in workspaces {
        let Ok(window_labels) = window.update(cx, |multi_workspace, _window, cx| {
            labels_for_multi_workspace(multi_workspace, cx)
        }) else {
            continue;
        };
        labels.extend(window_labels);
    }
    labels
}

/// Shows the modal in the active window and waits for the user's answer.
/// Returns `true` only when the user explicitly types Y; any other outcome
/// (escape, drop, modal swap, errored update) is treated as cancel.
pub async fn prompt_quit_confirmation(
    active_window: gpui::WindowHandle<crate::MultiWorkspace>,
    running: Vec<SharedString>,
    cx: &mut gpui::AsyncApp,
) -> bool {
    let (tx, rx) = oneshot::channel();
    let push_result = active_window.update(cx, |multi_workspace, window, cx| {
        multi_workspace.toggle_modal(window, cx, |_window, cx| {
            QuitConfirmModal::new(running, tx, cx)
        });
    });
    if push_result.is_err() {
        return false;
    }
    rx.await.unwrap_or(false)
}


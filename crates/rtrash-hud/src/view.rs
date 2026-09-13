//! HUD face on icedtea 0.17 constructors. Drawing only.

use iced::widget::{column, container, row, Space};
use iced::{Alignment, Element, Fill};
use icedtea::a11y::{A11y, Role};
use icedtea::collection::Tabs;
use icedtea::icon::Icons;
use icedtea::theme::Tokens;
use icedtea::toast::ToastKind;
use icedtea::variant::Variant;
use icedtea::widget;

use crate::app::{ConfirmKind, HudApp, Message};
use crate::catalog::Pane;
use crate::keys::Mode;
use crate::theme;

/// Board face: tabs + pane body + footer.
pub fn view(app: &HudApp) -> Element<'_, Message> {
    let tea = theme::tokens();
    if app.mode == Mode::Help {
        return help_overlay(app, tea);
    }

    let mut pane = column![header(app, tea), tab_row(app, tea)]
        .spacing(10)
        .padding([14, 16]);

    pane = pane.push(pane_body(app, tea));

    if let Some(kind) = app.confirm {
        let label = match kind {
            ConfirmKind::Restore => "confirm restore? y/n",
            ConfirmKind::Empty => "confirm empty? y/n",
        };
        pane = pane.push(widget::banner(
            label.to_string(),
            None,
            Some(ToastKind::Warning),
            tea,
            A11y::new("confirm", Role::Status),
        ));
    }
    if !app.status.is_empty() {
        pane = pane.push(widget::banner(
            app.status.clone(),
            None,
            Some(ToastKind::Info),
            tea,
            A11y::new("status", Role::Status),
        ));
    }

    container(pane.spacing(10).width(Fill).height(Fill))
        .width(Fill)
        .height(Fill)
        .style(move |_| icedtea::style::shell(tea))
        .into()
}

fn header(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    let live = if app.catalog.dirs.is_empty() {
        ("no-root", Variant::Warning)
    } else {
        ("offline", Variant::Quiet)
    };
    row![
        widget::label(
            "rtrash",
            widget::LabelFace::Body,
            tea,
            A11y::new("rtrash", Role::Header),
        ),
        widget::badge(
            live.0,
            None,
            tea,
            live.1,
            widget::BadgeSize::Small,
            A11y::new("serve", Role::Status),
        ),
        widget::chip(
            format!("force={}", app.force),
            Some(Message::Action(rtrash::tui_binds::Action::ToggleForce)),
            None,
            tea,
            if app.force {
                Variant::Primary
            } else {
                Variant::Chip
            },
            widget::ChipKind::Filter,
            Icons::NONE,
            A11y::button("force"),
        ),
        widget::chip(
            format!("dry-run={}", app.dry_run),
            Some(Message::Action(rtrash::tui_binds::Action::ToggleDryRun)),
            None,
            tea,
            if app.dry_run {
                Variant::Primary
            } else {
                Variant::Chip
            },
            widget::ChipKind::Filter,
            Icons::NONE,
            A11y::button("dry-run"),
        ),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn tab_row(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    let titles: Vec<String> = Pane::ALL.iter().map(|p| p.label().to_string()).collect();
    let active = Pane::ALL
        .iter()
        .position(|p| *p == app.pane)
        .unwrap_or(0);
    iced::widget::responsive(move |size| {
        let mut tabs = Tabs::new(titles.clone());
        tabs.select(active);
        tabs.closable = false;
        widget::tab_bar(
            &tabs,
            |i| Message::SetPane(Pane::ALL[i.min(Pane::ALL.len() - 1)]),
            |_| Message::Noop,
            size.width,
            false,
            tea,
            A11y::new("panes", Role::Tab),
        )
    })
    .width(Fill)
    .into()
}

fn pane_body(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    match app.pane {
        Pane::List | Pane::Restore | Pane::Empty => catalog_list(app, tea),
        Pane::Put => put_pane(app, tea),
        Pane::Status => status_pane(app, tea),
    }
}

fn catalog_list(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    let vis = app.visible();
    let find = widget::search_input(
        if app.mode == Mode::Filter {
            app.filter_draft.as_str()
        } else {
            app.filter.as_str()
        },
        Message::FilterChanged,
        Some(Message::FilterChanged(String::new())),
        Some(Message::Action(rtrash::tui_binds::Action::FilterCommit)),
        tea,
        A11y::new("filter catalog", Role::TextBox),
        None,
        &[],
    );

    if vis.is_empty() {
        return column![
            find,
            icedtea::pattern::status_page(
                "Empty catalog",
                "put a file, or pin --trash-dir",
                None,
                tea,
                A11y::new("empty", Role::Status),
            )
        ]
        .spacing(8)
        .into();
    }

    let selected = app.selected;
    let marked = app.marked.clone();
    let entries: Vec<(usize, String, String)> = vis
        .iter()
        .map(|&i| {
            let e = &app.catalog.entries[i];
            let date = e.date.clone().unwrap_or_else(|| "????-??-??T??:??:??".into());
            (i, date, e.path_label())
        })
        .collect();

    let list = widget::virtual_column(
        &app.list_heights,
        app.catalog_window,
        2,
        None,
        Message::CatalogScroll,
        |_| Message::Noop,
        None,
        tea,
        move |i| match entries.get(i) {
            Some((idx, date, path)) => catalog_row(*idx, date, path, i == selected, marked.contains(idx), tea),
            None => Space::new().height(0).into(),
        },
        A11y::new("catalog", Role::List),
    );

    column![find, container(list).width(Fill).height(Fill)]
        .spacing(8)
        .height(Fill)
        .into()
}

fn catalog_row(
    index: usize,
    date: &str,
    path: &str,
    selected: bool,
    marked: bool,
    tea: Tokens,
) -> Element<'static, Message> {
    let mark = if marked { "●" } else { "○" };
    let title = format!("{mark}  {date}  {path}");
    let row = widget::label(
        title,
        widget::LabelFace::Body,
        tea,
        A11y::new(format!("row-{index}"), Role::ListItem),
    );
    iced::widget::mouse_area(
        container(row)
            .width(Fill)
            .padding([6, 8])
            .style(move |_| icedtea::style::list_row(tea, selected)),
    )
    .on_press(Message::Select(index))
    .on_right_press(Message::ToggleMark(index))
    .into()
}

fn put_pane(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    column![
        widget::label(
            "Put a path into the pinned trash",
            widget::LabelFace::Meta,
            tea,
            A11y::new("put-help", Role::Status),
        ),
        widget::text_input(
            "path to put",
            &app.put_draft,
            Message::PutChanged,
            Some(Message::Action(rtrash::tui_binds::Action::Action)),
            widget::FieldOpts::NONE,
            tea,
            A11y::new("put", Role::TextBox),
            None,
        ),
        widget::button(
            "Put",
            Some(Message::Action(rtrash::tui_binds::Action::Action)),
            tea,
            Variant::Primary,
            Icons::NONE,
            widget::ButtonOpts::SHRINK,
            A11y::button("Put"),
        ),
    ]
    .spacing(8)
    .into()
}

fn status_pane(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    let (roots, items, bytes) = app.catalog.status_rows();
    if roots.is_empty() {
        return icedtea::pattern::status_page(
            "No trash directories",
            "pass --trash-dir or use the home trash",
            None,
            tea,
            A11y::new("no-roots", Role::Status),
        );
    }
    let mut col = column![widget::label(
        format!(
            "Total: {items} items ({})",
            rtrash::fastdelete::format_bytes(bytes)
        ),
        widget::LabelFace::Body,
        tea,
        A11y::new("total", Role::Status),
    )]
    .spacing(6);
    for r in roots {
        col = col.push(widget::label(
            format!(
                "{}\t{} items\t{}",
                r.path,
                r.items,
                rtrash::fastdelete::format_bytes(r.bytes)
            ),
            widget::LabelFace::Meta,
            tea,
            A11y::new(r.path.clone(), Role::ListItem),
        ));
    }
    col.into()
}

fn help_overlay(app: &HudApp, tea: Tokens) -> Element<'_, Message> {
    let mut lines = vec!["rtrash-hud keys (tui_binds)".to_string()];
    for &a in rtrash::tui_binds::Action::BROWSE {
        lines.push(format!(
            "{}  {}",
            app.keys.display_chords(a),
            a.doc()
        ));
    }
    lines.push("1-5  switch pane".into());
    widget::banner(
        lines.join("\n"),
        None,
        Some(ToastKind::Info),
        tea,
        A11y::new("help", Role::Status),
    )
}

#[cfg(test)]
mod tests {
    /// The board is painted out of icedtea's widgets.
    #[test]
    fn the_hud_paints_through_icedtea_widgets() {
        let src = include_str!("view.rs");
        const WIDGETS: &[&str] = &[
            "widget::chip",
            "widget::search_input",
            "widget::virtual_column",
            "widget::tab_bar",
            "widget::label",
            "widget::text_input",
            "widget::button",
            "widget::banner",
            "icedtea::style::list_row",
            "icedtea::pattern::status_page",
        ];
        let missing: Vec<&str> = WIDGETS
            .iter()
            .copied()
            .filter(|w| !src.contains(w))
            .collect();
        assert!(
            missing.is_empty(),
            "the view no longer paints with: {missing:?}"
        );
    }
}

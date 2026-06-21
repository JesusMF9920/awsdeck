//! Picker de ambientes (`ctrl-e`): overlay con la lista de profiles de
//! `~/.aws/config`. El `App` posee el estado; aquí solo se dibuja.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};

use crate::aws::context::ProfileEntry;

pub fn render(frame: &mut Frame, area: Rect, profiles: &[ProfileEntry], state: &mut ListState) {
    let height = (profiles.len() as u16 + 3).min(area.height.max(3));
    let popup = super::popup_area(area, 50, height);
    frame.render_widget(Clear, popup);

    let items: Vec<ListItem> = profiles
        .iter()
        .map(|p| {
            let region = p
                .region
                .clone()
                .unwrap_or_else(|| "(región actual)".to_string());
            ListItem::new(Line::from(vec![
                Span::raw(p.name.clone()),
                Span::raw("  "),
                Span::styled(region, Style::new().dark_gray()),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(Block::bordered().title(" cambiar ambiente (enter · esc) "))
        .highlight_style(Style::new().reversed())
        .highlight_symbol("› ");
    frame.render_stateful_widget(list, popup, state);
}

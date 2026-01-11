pub mod log_capture;

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table},
    style::{Color, Modifier, Style},
    layout::{Constraint, Layout, Rect},
    Frame,
};
use crate::models::Portfolio;

pub use log_capture::UiLogLayer;

// Количество уровней стакана для отображения
pub const ORDER_BOOK_DEPTH: usize = 5;
// Максимум логов в буфере
const MAX_LOG_LINES: usize = 100;

/// Один уровень стакана: цена и размер
#[derive(Debug, Clone, Copy, Default)]
pub struct OrderLevel {
    pub price: f64,
    pub size: f64,
}

impl OrderLevel {
    pub fn total(&self) -> f64 {
        self.price * self.size
    }
}

/// Полный стакан для одной стороны (UP или DOWN)
#[derive(Debug, Clone, Default)]
pub struct SideOrderBook {
    pub bids: [OrderLevel; ORDER_BOOK_DEPTH], // Лучшие bid'ы (отсортированы по убыванию цены)
    pub asks: [OrderLevel; ORDER_BOOK_DEPTH], // Лучшие ask'и (отсортированы по возрастанию цены)
}

/// Информация о событии
#[derive(Debug, Clone, Default)]
pub struct EventInfo {
    pub title: String,
    pub end_date: DateTime<Utc>,
    pub total_seconds: i64,
}

impl EventInfo {
    pub fn remaining_seconds(&self) -> i64 {
        (self.end_date - Utc::now()).num_seconds().max(0)
    }

    pub fn progress_ratio(&self) -> f64 {
        if self.total_seconds <= 0 {
            return 1.0;
        }
        let remaining = self.remaining_seconds();
        let elapsed = self.total_seconds - remaining;
        (elapsed as f64 / self.total_seconds as f64).clamp(0.0, 1.0)
    }
}

/// Состояние UI - безопасно для многопоточного доступа
#[derive(Debug, Default)]
pub struct UiStateInner {
    pub event_info: EventInfo,
    pub portfolio: Portfolio,
    pub up_book: SideOrderBook,
    pub down_book: SideOrderBook,
    pub logs: VecDeque<String>,
    pub is_running: bool,
    pub dry_run: bool,
    pub price_to_beat: Option<f64>,
    pub current_price: Option<f64>,
}

pub type UiState = Arc<Mutex<UiStateInner>>;

pub fn new_ui_state() -> UiState {
    Arc::new(Mutex::new(UiStateInner::default()))
}

/// Добавить лог-сообщение в буфер
pub fn add_log(state: &UiState, msg: String) {
    if let Ok(mut s) = state.lock() {
        s.logs.push_back(msg);
        while s.logs.len() > MAX_LOG_LINES {
            s.logs.pop_front();
        }
    }
}

/// Обновить информацию о событии
pub fn set_event_info(state: &UiState, title: String, end_date: DateTime<Utc>, total_seconds: i64) {
    if let Ok(mut s) = state.lock() {
        s.event_info = EventInfo { title, end_date, total_seconds };
        s.is_running = true;
    }
}

/// Установить режим dry run
pub fn set_dry_run(state: &UiState, dry_run: bool) {
    if let Ok(mut s) = state.lock() {
        s.dry_run = dry_run;
    }
}

/// Обновить портфолио
pub fn update_portfolio(state: &UiState, portfolio: Portfolio) {
    if let Ok(mut s) = state.lock() {
        s.portfolio = portfolio;
    }
}

/// Обновить стакан UP
pub fn update_up_book(state: &UiState, bids: [OrderLevel; ORDER_BOOK_DEPTH], asks: [OrderLevel; ORDER_BOOK_DEPTH]) {
    if let Ok(mut s) = state.lock() {
        s.up_book.bids = bids;
        s.up_book.asks = asks;
    }
}

/// Обновить стакан DOWN
pub fn update_down_book(state: &UiState, bids: [OrderLevel; ORDER_BOOK_DEPTH], asks: [OrderLevel; ORDER_BOOK_DEPTH]) {
    if let Ok(mut s) = state.lock() {
        s.down_book.bids = bids;
        s.down_book.asks = asks;
    }
}

/// Остановить UI
pub fn stop_ui(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.is_running = false;
    }
}

/// Установить price to beat
pub fn set_price_to_beat(state: &UiState, price: Option<f64>) {
    if let Ok(mut s) = state.lock() {
        s.price_to_beat = price;
    }
}

/// Установить текущую цену
pub fn set_current_price(state: &UiState, price: f64) {
    if let Ok(mut s) = state.lock() {
        s.current_price = Some(price);
    }
}

/// Терминал для TUI
pub type Terminal = ratatui::Terminal<CrosstermBackend<Stdout>>;

/// Инициализация терминала
pub fn init_terminal() -> io::Result<Terminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = ratatui::Terminal::new(backend)?;
    Ok(terminal)
}

/// Восстановление терминала
pub fn restore_terminal(terminal: &mut Terminal) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

/// Главный рендер UI
pub fn render(frame: &mut Frame, state: &UiState) {
    let state = match state.lock() {
        Ok(s) => s,
        Err(_) => return,
    };

    let area = frame.area();

    // Основной layout: левая часть и правая (стакан)
    let [left_area, right_area] = Layout::horizontal([
        Constraint::Percentage(60),
        Constraint::Percentage(40),
    ]).areas(area);

    // Левая часть: event info, portfolio, logs
    let [event_area, portfolio_area, logs_area] = Layout::vertical([
        Constraint::Length(8),   // Event info (увеличено для price info с заголовками)
        Constraint::Length(9),   // Portfolio
        Constraint::Fill(1),     // Logs
    ]).areas(left_area);

    render_event_info(frame, event_area, &state.event_info, state.dry_run, state.price_to_beat, state.current_price);
    render_portfolio(frame, portfolio_area, &state.portfolio);
    render_logs(frame, logs_area, &state.logs);

    // Правая часть: стаканы UP и DOWN
    let [up_book_area, down_book_area] = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ]).areas(right_area);

    render_order_book(frame, up_book_area, "UP", &state.up_book, Color::Green);
    render_order_book(frame, down_book_area, "DOWN", &state.down_book, Color::Red);
}

/// Рендер информации о событии
fn render_event_info(frame: &mut Frame, area: Rect, info: &EventInfo, dry_run: bool, price_to_beat: Option<f64>, current_price: Option<f64>) {
    // Заголовок с индикатором режима
    let title = if dry_run {
        " MARKET [DRY RUN] "
    } else {
        " MARKET "
    };

    let title_color = if dry_run { Color::Yellow } else { Color::Cyan };

    let block = Block::default()
        .title(title)
        .title_style(Style::default().fg(title_color).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let remaining = info.remaining_seconds();
    let total = info.total_seconds;

    // Title, time, progress, price labels, prices
    let [title_area, time_area, progress_area, price_labels_area, prices_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ]).areas(inner);

    // Название события
    let event_title = Paragraph::new(info.title.as_str())
        .style(Style::default().fg(Color::White));
    frame.render_widget(event_title, title_area);

    // Время в формате MM:SS
    let remaining_mins = remaining / 60;
    let remaining_secs = remaining % 60;
    let total_mins = total / 60;
    let total_secs = total % 60;
    let time_text = format!("Time: {:02}:{:02} / {:02}:{:02}", remaining_mins, remaining_secs, total_mins, total_secs);
    let time = Paragraph::new(time_text)
        .style(Style::default().fg(Color::Yellow));
    frame.render_widget(time, time_area);

    // Progress bar
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Magenta).bg(Color::DarkGray))
        .ratio(info.progress_ratio())
        .label(format!("{:.0}%", info.progress_ratio() * 100.0));
    frame.render_widget(gauge, progress_area);

    // Price labels - выровненные
    let price_labels = Line::from(vec![
        Span::styled(format!("{:<13}", "Price to beat"), Style::default().fg(Color::Gray)),
        Span::raw(" | "),
        Span::styled("Current price", Style::default().fg(Color::Cyan)),
    ]);
    let labels_paragraph = Paragraph::new(price_labels);
    frame.render_widget(labels_paragraph, price_labels_area);

    // Price values - выровненные с индикаторами изменения
    let price_to_beat_str = price_to_beat
        .map(|p| format!("${:.2}", p))
        .unwrap_or_else(|| "N/A".to_string());

    // Формируем строку current price с индикатором
    let mut price_spans = vec![
        Span::styled(format!("{:<13}", price_to_beat_str), Style::default().fg(Color::Gray)),
        Span::raw(" | "),
    ];

    if let Some(curr) = current_price {
        let current_price_str = format!("${:.2}", curr);
        price_spans.push(Span::styled(current_price_str, Style::default().fg(Color::Cyan)));

        // Добавляем треугольник и разницу, если есть price_to_beat
        if let Some(ptb) = price_to_beat {
            let diff = curr - ptb;
            if diff > 0.0 {
                // Цена выше - зеленый треугольник вверх
                price_spans.push(Span::raw(" "));
                price_spans.push(Span::styled("▲", Style::default().fg(Color::Green)));
                price_spans.push(Span::raw(" "));
                price_spans.push(Span::styled(format!("+${:.2}", diff), Style::default().fg(Color::Green)));
            } else if diff < 0.0 {
                // Цена ниже - красный треугольник вниз
                price_spans.push(Span::raw(" "));
                price_spans.push(Span::styled("▼", Style::default().fg(Color::Red)));
                price_spans.push(Span::raw(" "));
                price_spans.push(Span::styled(format!("-${:.2}", diff.abs()), Style::default().fg(Color::Red)));
            }
        }
    } else {
        price_spans.push(Span::styled("N/A", Style::default().fg(Color::Cyan)));
    }

    let price_values = Line::from(price_spans);
    let values_paragraph = Paragraph::new(price_values);
    frame.render_widget(values_paragraph, prices_area);
}

/// Рендер портфолио
fn render_portfolio(frame: &mut Frame, area: Rect, portfolio: &Portfolio) {
    let block = Block::default()
        .title(" PORTFOLIO ")
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let up_avg = portfolio.up_avg();
    let down_avg = portfolio.down_avg();
    let total_avg = up_avg + down_avg;
    let total_spent = portfolio.up_spent + portfolio.down_spent;

    // Создаем текст с информацией о позициях
    let text = vec![
        Line::from(vec![
            Span::styled("  UP: ", Style::default().fg(Color::Green)),
            Span::raw(format!("{:.1} shares @ avg {:.3}", portfolio.up_shares, up_avg)),
            Span::styled(format!("  ${:.2}", portfolio.up_spent), Style::default().fg(Color::Gray)),
        ]),
        Line::from(vec![
            Span::styled("DOWN: ", Style::default().fg(Color::Red)),
            Span::raw(format!("{:.1} shares @ avg {:.3}", portfolio.down_shares, down_avg)),
            Span::styled(format!("  ${:.2}", portfolio.down_spent), Style::default().fg(Color::Gray)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled("Total Avg: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{:.3}", total_avg),
                Style::default().fg(if total_avg < 0.97 { Color::Green } else if total_avg < 1.0 { Color::Yellow } else { Color::Red }),
            ),
            Span::raw("  |  "),
            Span::styled("Spent: ", Style::default().fg(Color::White)),
            Span::styled(format!("${:.2}", total_spent), Style::default().fg(Color::Cyan)),
        ]),
        Line::from(vec![
            Span::styled("Maker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.maker_trades)),
            Span::raw("  |  "),
            Span::styled("Taker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.taker_trades)),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Рендер стакана
fn render_order_book(frame: &mut Frame, area: Rect, title: &str, book: &SideOrderBook, color: Color) {
    let block = Block::default()
        .title(format!(" {} ORDER BOOK ", title))
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Разделяем на asks и bids
    let [asks_area, divider_area, bids_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ]).areas(inner);

    // Заголовок asks
    let header_style = Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD);

    // Asks (красные) - вычисляем кумулятивный total от лучшего ask'а вверх
    // asks[0] = лучший (самый низкий) ask
    let mut cumulative_ask = 0.0;
    let ask_data: Vec<(f64, f64, f64)> = book.asks.iter()
        .filter(|l| l.size > 0.0)
        .map(|level| {
            cumulative_ask += level.price * level.size;
            (level.price, level.size, cumulative_ask)
        })
        .collect();

    // Показываем от высокой к низкой цене (перевернутый порядок для визуала)
    let ask_rows: Vec<Row> = ask_data.iter()
        .rev()
        .map(|(price, size, cum_total)| {
            Row::new(vec![
                Cell::from(format!("{:.0}¢", price * 100.0))
                    .style(Style::default().fg(Color::Red)),
                Cell::from(format!("{:.2}", size)),
                Cell::from(format!("${:.2}", cum_total)),
            ])
        })
        .collect();

    let ask_table = Table::new(
        ask_rows,
        [Constraint::Percentage(30), Constraint::Percentage(35), Constraint::Percentage(35)],
    )
    .header(
        Row::new(vec!["Asks", "Shares", "Total"])
            .style(header_style)
    );
    frame.render_widget(ask_table, asks_area);

    // Разделитель со spread
    let best_bid = book.bids.first().map(|l| l.price).unwrap_or(0.0);
    let best_ask = book.asks.first().map(|l| l.price).unwrap_or(0.0);
    let spread = ((best_ask - best_bid) * 100.0).max(0.0);

    let divider = Paragraph::new(format!("───── Spread: {:.0}¢ ─────", spread))
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(divider, divider_area);

    // Bids (зеленые) - вычисляем кумулятивный total от лучшего bid'а вниз
    // bids[0] = лучший (самый высокий) bid
    let mut cumulative_bid = 0.0;
    let bid_rows: Vec<Row> = book.bids.iter()
        .filter(|l| l.size > 0.0)
        .map(|level| {
            cumulative_bid += level.price * level.size;
            Row::new(vec![
                Cell::from(format!("{:.0}¢", level.price * 100.0))
                    .style(Style::default().fg(Color::Green)),
                Cell::from(format!("{:.2}", level.size)),
                Cell::from(format!("${:.2}", cumulative_bid)),
            ])
        })
        .collect();

    let bid_table = Table::new(
        bid_rows,
        [Constraint::Percentage(30), Constraint::Percentage(35), Constraint::Percentage(35)],
    )
    .header(
        Row::new(vec!["Bids", "Shares", "Total"])
            .style(header_style)
    );
    frame.render_widget(bid_table, bids_area);
}

/// Рендер логов
fn render_logs(frame: &mut Frame, area: Rect, logs: &VecDeque<String>) {
    let block = Block::default()
        .title(" Activity Log ")
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Берём последние логи, которые поместятся
    let height = inner.height as usize;
    let width = inner.width as usize;

    let visible_logs: Vec<Line> = logs.iter()
        .rev()
        .take(height)
        .rev()
        .map(|s| {
            // Обрезаем длинные строки чтобы избежать проблем с wrap
            let truncated = if s.chars().count() > width {
                let truncate_at = s.char_indices()
                    .nth(width.saturating_sub(1))
                    .map(|(idx, _)| idx)
                    .unwrap_or(s.len());
                format!("{}…", &s[..truncate_at])
            } else {
                s.clone()
            };

            // Подсветка по типу сообщения
            let style = if truncated.contains("ERROR") || truncated.contains("❌") {
                Style::default().fg(Color::Red)
            } else if truncated.contains("WARN") || truncated.contains("⚠️") {
                Style::default().fg(Color::Yellow)
            } else if truncated.contains("✅") || truncated.contains("FOUND") || truncated.contains("НАЙДЕНО") {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::Gray)
            };
            Line::styled(truncated, style)
        })
        .collect();

    let paragraph = Paragraph::new(visible_logs);
    frame.render_widget(paragraph, inner);
}

/// Проверка нажатия 'q' для выхода
pub fn check_exit_key() -> bool {
    if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind == KeyEventKind::Press && key.code == KeyCode::Char('q') {
                return true;
            }
        }
    }
    false
}

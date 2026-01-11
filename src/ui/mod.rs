pub mod log_capture;

use std::collections::{VecDeque, HashSet};
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
pub const ORDER_BOOK_DEPTH: usize = 11;
// Максимум записей в истории
const MAX_HISTORY_ENTRIES: usize = 100;

/// Тип торговли
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TradeType {
    Maker,
    Taker,
}

/// Запись в истории торговли
#[derive(Debug, Clone)]
pub struct TradeHistoryEntry {
    pub is_up: bool,           // true = Up, false = Down
    pub shares: f64,           // количество акций
    pub price: f64,            // цена в центах (0.36 = 36¢)
    pub cost: f64,             // стоимость в долларах
    pub trade_type: TradeType, // Maker или Taker
    pub timestamp: DateTime<Utc>, // время покупки
}

/// Открытый ордер
#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub order_id: String,
    pub is_up: bool,           // true = Up, false = Down
    pub price: f64,            // цена
    pub filled: f64,           // заполнено
    pub total: f64,            // всего
}

impl OpenOrder {
    pub fn total_cost(&self) -> f64 {
        self.price * self.total
    }
}

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
    pub is_running: bool,
    pub trading_enabled: bool,  // false = dryrun, true = real trading
    pub price_to_beat: Option<f64>,
    pub current_price: Option<f64>,
    pub our_up_bid_prices: HashSet<u32>,   // Цены в центах, где размещены наши UP ордера
    pub our_down_bid_prices: HashSet<u32>, // Цены в центах, где размещены наши DOWN ордера
    // История торговли
    pub trade_history: VecDeque<TradeHistoryEntry>,
    pub history_scroll_offset: usize,  // Для скролла истории
    // Открытые ордера
    pub open_orders: Vec<OpenOrder>,
}

pub type UiState = Arc<Mutex<UiStateInner>>;

pub fn new_ui_state() -> UiState {
    Arc::new(Mutex::new(UiStateInner::default()))
}

/// Добавить запись в историю торговли
pub fn add_trade_history(state: &UiState, entry: TradeHistoryEntry) {
    if let Ok(mut s) = state.lock() {
        s.trade_history.push_front(entry); // Новые записи в начало
        while s.trade_history.len() > MAX_HISTORY_ENTRIES {
            s.trade_history.pop_back();
        }
    }
}

/// Добавить открытый ордер
pub fn add_open_order(state: &UiState, order: OpenOrder) {
    if let Ok(mut s) = state.lock() {
        s.open_orders.push(order);
    }
}

/// Обновить заполнение открытого ордера
pub fn update_open_order_filled(state: &UiState, order_id: &str, filled: f64) {
    if let Ok(mut s) = state.lock() {
        if let Some(order) = s.open_orders.iter_mut().find(|o| o.order_id == order_id) {
            order.filled = filled;
        }
    }
}

/// Удалить открытый ордер
pub fn remove_open_order(state: &UiState, order_id: &str) {
    if let Ok(mut s) = state.lock() {
        s.open_orders.retain(|o| o.order_id != order_id);
    }
}

/// Очистить все открытые ордера (при старте нового события)
pub fn clear_open_orders(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.open_orders.clear();
    }
}

/// Прокрутка истории вверх
pub fn scroll_history_up(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        if s.history_scroll_offset < s.trade_history.len().saturating_sub(1) {
            s.history_scroll_offset += 1;
        }
    }
}

/// Прокрутка истории вниз
pub fn scroll_history_down(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.history_scroll_offset = s.history_scroll_offset.saturating_sub(1);
    }
}

/// Обновить информацию о событии
pub fn set_event_info(state: &UiState, title: String, end_date: DateTime<Utc>, total_seconds: i64) {
    if let Ok(mut s) = state.lock() {
        s.event_info = EventInfo { title, end_date, total_seconds };
        s.is_running = true;
    }
}

/// Установить режим торговли (вызывается при инициализации)
pub fn set_trading_enabled(state: &UiState, enabled: bool) {
    if let Ok(mut s) = state.lock() {
        s.trading_enabled = enabled;
    }
}

/// Переключить режим торговли (вызывается при нажатии 'r')
pub fn toggle_trading(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.trading_enabled = !s.trading_enabled;
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

/// Добавить цену нашего bid ордера
pub fn add_our_bid_price(state: &UiState, is_up: bool, price: f64) {
    if let Ok(mut s) = state.lock() {
        let price_cents = (price * 100.0).round() as u32;
        if is_up {
            s.our_up_bid_prices.insert(price_cents);
        } else {
            s.our_down_bid_prices.insert(price_cents);
        }
    }
}

/// Удалить цену нашего bid ордера
pub fn remove_our_bid_price(state: &UiState, is_up: bool, price: f64) {
    if let Ok(mut s) = state.lock() {
        let price_cents = (price * 100.0).round() as u32;
        if is_up {
            s.our_up_bid_prices.remove(&price_cents);
        } else {
            s.our_down_bid_prices.remove(&price_cents);
        }
    }
}

/// Очистить все наши цены (при старте нового события)
pub fn clear_our_bid_prices(state: &UiState) {
    if let Ok(mut s) = state.lock() {
        s.our_up_bid_prices.clear();
        s.our_down_bid_prices.clear();
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

    // Левая часть: event info, portfolio, open orders, history
    let [event_area, portfolio_area, open_orders_area, history_area] = Layout::vertical([
        Constraint::Length(8),    // Event info
        Constraint::Length(9),    // Portfolio
        Constraint::Length(14),   // Open Orders (увеличено для отображения большего числа ордеров)
        Constraint::Fill(1),      // History
    ]).areas(left_area);

    render_event_info(frame, event_area, &state.event_info, state.trading_enabled, state.price_to_beat, state.current_price);

    // Получаем best_bid для расчета PnL
    let up_best_bid = state.up_book.bids.first().map(|l| l.price).unwrap_or(0.0);
    let down_best_bid = state.down_book.bids.first().map(|l| l.price).unwrap_or(0.0);
    render_portfolio(frame, portfolio_area, &state.portfolio, state.trading_enabled, up_best_bid, down_best_bid);
    render_open_orders(frame, open_orders_area, &state.open_orders);
    render_history(frame, history_area, &state.trade_history, state.history_scroll_offset);

    // Правая часть: стаканы UP и DOWN
    let [up_book_area, down_book_area] = Layout::vertical([
        Constraint::Percentage(50),
        Constraint::Percentage(50),
    ]).areas(right_area);

    render_order_book(frame, up_book_area, "UP", &state.up_book, Color::Green, &state.our_up_bid_prices);
    render_order_book(frame, down_book_area, "DOWN", &state.down_book, Color::Red, &state.our_down_bid_prices);
}

/// Рендер информации о событии
fn render_event_info(frame: &mut Frame, area: Rect, info: &EventInfo, trading_enabled: bool, price_to_beat: Option<f64>, current_price: Option<f64>) {
    // Заголовок с индикатором режима
    let title = if !trading_enabled {
        " MARKET [DRY RUN] "
    } else {
        " MARKET "
    };

    let title_color = if !trading_enabled { Color::Yellow } else { Color::Cyan };

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
fn render_portfolio(frame: &mut Frame, area: Rect, portfolio: &Portfolio, trading_enabled: bool, up_best_bid: f64, down_best_bid: f64) {
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

    // Расчет PnL: (текущий best_bid * количество акций) - потраченная сумма
    let up_pnl = (up_best_bid * portfolio.up_shares) - portfolio.up_spent;
    let down_pnl = (down_best_bid * portfolio.down_shares) - portfolio.down_spent;
    let total_pnl = up_pnl + down_pnl;

    // Хелпер для создания PnL спанов
    fn pnl_spans(pnl: f64) -> Vec<Span<'static>> {
        if pnl > 0.0 {
            vec![
                Span::styled(" ▲", Style::default().fg(Color::Green)),
                Span::styled(format!(" +${:.2}", pnl), Style::default().fg(Color::Green)),
            ]
        } else if pnl < 0.0 {
            vec![
                Span::styled(" ▼", Style::default().fg(Color::Red)),
                Span::styled(format!(" -${:.2}", pnl.abs()), Style::default().fg(Color::Red)),
            ]
        } else {
            vec![
                Span::styled(" $0.00", Style::default().fg(Color::Gray)),
            ]
        }
    }

    // Создаем текст с информацией о позициях
    // Формат: filled/placed shares @ avg  $spent  PnL
    let mut up_line = vec![
        Span::styled("  UP: ", Style::default().fg(Color::Green)),
        Span::raw(format!("{:.1}/{:.1} shares @ avg {:.3}", portfolio.up_shares, portfolio.up_total_placed, up_avg)),
        Span::styled(format!("  ${:.2}", portfolio.up_spent), Style::default().fg(Color::Gray)),
    ];
    if portfolio.up_shares > 0.0 {
        up_line.extend(pnl_spans(up_pnl));
    }

    let mut down_line = vec![
        Span::styled("DOWN: ", Style::default().fg(Color::Red)),
        Span::raw(format!("{:.1}/{:.1} shares @ avg {:.3}", portfolio.down_shares, portfolio.down_total_placed, down_avg)),
        Span::styled(format!("  ${:.2}", portfolio.down_spent), Style::default().fg(Color::Gray)),
    ];
    if portfolio.down_shares > 0.0 {
        down_line.extend(pnl_spans(down_pnl));
    }

    // Total PnL строка
    let mut total_pnl_line = vec![
        Span::styled("Total PnL:", Style::default().fg(Color::White)),
    ];
    if portfolio.up_shares > 0.0 || portfolio.down_shares > 0.0 {
        total_pnl_line.extend(pnl_spans(total_pnl));
    } else {
        total_pnl_line.push(Span::styled(" $0.00", Style::default().fg(Color::Gray)));
    }

    let text = vec![
        Line::from(up_line),
        Line::from(down_line),
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
            Span::raw("  |  "),
        ].into_iter().chain(total_pnl_line).collect::<Vec<_>>()),
        Line::from(vec![
            Span::styled("Maker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.maker_trades)),
            Span::raw("  |  "),
            Span::styled("Taker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.taker_trades)),
        ]),
        Line::from(""),
        // Добавляем строку с индикатором Trading: ON/OFF
        Line::from(vec![
            Span::styled("Trading: ", Style::default().fg(Color::White)),
            Span::styled(
                if trading_enabled { "ON" } else { "OFF" },
                Style::default()
                    .fg(if trading_enabled { Color::Green } else { Color::Red })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Рендер стакана
fn render_order_book(frame: &mut Frame, area: Rect, title: &str, book: &SideOrderBook, color: Color, our_bid_prices: &HashSet<u32>) {
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
            let price_cents = (price * 100.0).round() as u32;
            let has_our_order = our_bid_prices.contains(&price_cents);
            let price_text = if has_our_order {
                format!("{:.0}¢⏱", price * 100.0)
            } else {
                format!("{:.0}¢ ", price * 100.0)  // Пробел для выравнивания
            };
            Row::new(vec![
                Cell::from(price_text)
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
            let price_cents = (level.price * 100.0).round() as u32;
            let has_our_order = our_bid_prices.contains(&price_cents);
            let price_text = if has_our_order {
                format!("{:.0}¢⏱", level.price * 100.0)
            } else {
                format!("{:.0}¢ ", level.price * 100.0)  // Пробел для выравнивания
            };
            Row::new(vec![
                Cell::from(price_text)
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

/// Рендер открытых ордеров
fn render_open_orders(frame: &mut Frame, area: Rect, orders: &[OpenOrder]) {
    let block = Block::default()
        .title(" OPEN ORDERS ")
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header_style = Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD);

    let rows: Vec<Row> = orders.iter()
        .map(|order| {
            let outcome_color = if order.is_up { Color::Green } else { Color::Red };
            let outcome_text = if order.is_up { "Up" } else { "Down" };

            Row::new(vec![
                Cell::from("Buy").style(Style::default().fg(Color::White)),
                Cell::from(outcome_text).style(Style::default().fg(outcome_color)),
                Cell::from(format!("{:.0}¢", order.price * 100.0)).style(Style::default().fg(Color::White)),
                Cell::from(format!("{:.0} / {:.0}", order.filled, order.total)).style(Style::default().fg(Color::White)),
                Cell::from(format!("${:.2}", order.total_cost())).style(Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),   // Side
            Constraint::Length(8),   // Outcome
            Constraint::Length(8),   // Price
            Constraint::Length(10),  // Filled
            Constraint::Length(8),   // Total
        ],
    )
    .header(
        Row::new(vec!["Side", "Outcome", "Price", "Filled", "Total"])
            .style(header_style)
    );
    frame.render_widget(table, inner);
}

/// Форматирование времени с момента покупки
fn format_time_ago(timestamp: DateTime<Utc>) -> String {
    let now = Utc::now();
    let duration = now.signed_duration_since(timestamp);
    let total_secs = duration.num_seconds().max(0);

    let mins = total_secs / 60;
    let secs = total_secs % 60;

    if mins > 0 {
        format!("{}m {:02}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Рендер истории торговли
fn render_history(frame: &mut Frame, area: Rect, history: &VecDeque<TradeHistoryEntry>, scroll_offset: usize) {
    let block = Block::default()
        .title(" HISTORY ")
        .title_style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;

    if history.is_empty() {
        let empty_msg = Paragraph::new("No trades yet...")
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty_msg, inner);
        return;
    }

    // Применяем скролл и берем записи для отображения
    let visible_entries: Vec<Line> = history.iter()
        .skip(scroll_offset)
        .take(height)
        .enumerate()
        .map(|(idx, entry)| {
            let number = scroll_offset + idx + 1;
            let outcome_color = if entry.is_up { Color::Green } else { Color::Red };
            let outcome_text = if entry.is_up { "Up" } else { "Down" };
            let trade_type_str = match entry.trade_type {
                TradeType::Maker => "Maker",
                TradeType::Taker => "Taker",
            };
            let time_ago = format_time_ago(entry.timestamp);

            // Формат: "1. Bought 5.00 Up at 36¢($1.8)      Maker      14m 00s"
            Line::from(vec![
                Span::styled(format!("{:>2}. ", number), Style::default().fg(Color::White)),
                Span::styled("Bought ", Style::default().fg(Color::White)),
                Span::styled(format!("{:.2} ", entry.shares), Style::default().fg(outcome_color)),
                Span::styled(format!("{} ", outcome_text), Style::default().fg(outcome_color)),
                Span::styled("at ", Style::default().fg(Color::White)),
                Span::styled(format!("{:.0}¢", entry.price * 100.0), Style::default().fg(Color::White)),
                Span::styled(format!("(${:.2})", entry.cost), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("      {:<6}", trade_type_str), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("      {}", time_ago), Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(visible_entries);
    frame.render_widget(paragraph, inner);
}

/// Результат проверки нажатых клавиш
pub enum KeyAction {
    None,
    Exit,
    ToggleTrading,
    ScrollHistoryUp,
    ScrollHistoryDown,
}

/// Проверка нажатия клавиш: 'q' для выхода, 'r' для переключения торговли, 'c'/'x' для скролла истории
pub fn check_key_action() -> KeyAction {
    if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') => return KeyAction::Exit,
                    KeyCode::Char('r') => return KeyAction::ToggleTrading,
                    KeyCode::Char('c') => return KeyAction::ScrollHistoryUp,
                    KeyCode::Char('x') => return KeyAction::ScrollHistoryDown,
                    _ => {}
                }
            }
        }
    }
    KeyAction::None
}

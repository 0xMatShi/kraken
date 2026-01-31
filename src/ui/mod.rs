pub mod log_capture;

use crate::models::Portfolio;
use chrono::{DateTime, Utc};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    prelude::*,
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table},
};
use std::collections::{HashSet, VecDeque};
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};

pub use log_capture::UiLogLayer;

// Количество уровней стакана для отображения
pub const ORDER_BOOK_DEPTH: usize = 19;
// Максимум записей в истории
const MAX_HISTORY_ENTRIES: usize = 50;

/// Тип торговли
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TradeType {
    Maker,
    Taker,
}

/// Запись в истории торговли
#[derive(Debug, Clone)]
pub struct TradeHistoryEntry {
    pub is_up: bool,              // true = Up, false = Down
    pub shares: f64,              // количество акций
    pub price: f64,               // цена в центах (0.36 = 36¢)
    pub cost: f64,                // стоимость в долларах
    pub trade_type: TradeType,    // Maker или Taker
    pub timestamp: DateTime<Utc>, // время покупки
}

/// Открытый ордер
#[derive(Debug, Clone)]
pub struct OpenOrder {
    pub order_id: String,
    pub is_up: bool, // true = Up, false = Down
    pub price: f64,  // цена
    pub filled: f64, // заполнено
    pub total: f64,  // всего
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
    pub slug: String,
    pub end_date: DateTime<Utc>,
    pub total_seconds: i64,
}

/// Конфигурация для отображения в UI
#[derive(Debug, Clone, Default)]
pub struct UiConfig {
    pub max_balance: f64,
    pub size: f64,
    pub max_size_side: f64,
    pub seconds_before_start: i64,
    pub legs_strategy: String,
}

impl EventInfo {
    pub fn remaining_seconds(&self) -> i64 {
        (self.end_date - Utc::now()).num_seconds().max(0)
    }

    /// Сколько секунд прошло с начала события
    pub fn elapsed_seconds(&self) -> i64 {
        (self.total_seconds - self.remaining_seconds()).max(0)
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

/// Режим работы бота
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TradingMode {
    Stop,       // Софт стоит афк
    RealRun,    // Реальная торговля за деньги
    Cancelling, // Режим отмены ордеров (нажать x для отмены всех)
    Hedge,      // Режим ручного хеджирования (покупка по рынку)
}

impl Default for TradingMode {
    fn default() -> Self {
        TradingMode::Stop // По умолчанию Stop
    }
}

/// Состояние ввода hedge
#[derive(Debug, Clone, PartialEq)]
pub enum HedgeInputState {
    None,                   // Не в режиме ввода
    RequestingUp(String),   // Вводим количество для UP
    RequestingDown(String), // Вводим количество для DOWN
}

impl Default for HedgeInputState {
    fn default() -> Self {
        HedgeInputState::None
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
    pub trading_mode: TradingMode,         // Текущий режим работы
    pub our_up_bid_prices: HashSet<u32>,   // Цены в центах, где размещены наши UP ордера
    pub our_down_bid_prices: HashSet<u32>, // Цены в центах, где размещены наши DOWN ордера
    // История торговли
    pub trade_history: VecDeque<TradeHistoryEntry>,
    // Открытые ордера
    pub open_orders: Vec<OpenOrder>,
    // Конфигурация для отображения
    pub config: UiConfig,
    // Состояние ввода hedge
    pub hedge_input_state: HedgeInputState,
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

/// Обновить информацию о событии
pub fn set_event_info(
    state: &UiState,
    title: String,
    slug: String,
    end_date: DateTime<Utc>,
    total_seconds: i64,
) {
    if let Ok(mut s) = state.lock() {
        s.event_info = EventInfo {
            title,
            slug,
            end_date,
            total_seconds,
        };
        s.is_running = true;
    }
}

/// Установить режим торговли (вызывается при инициализации)
pub fn set_trading_mode(state: &UiState, mode: TradingMode) {
    if let Ok(mut s) = state.lock() {
        s.trading_mode = mode;
    }
}

/// Получить текущий режим торговли
pub fn get_trading_mode(state: &UiState) -> TradingMode {
    state.lock().map(|s| s.trading_mode).unwrap_or_default()
}

/// Переключить режим торговли
/// Возвращает true если нужно отменить все ордера (CancelAllOrders в режиме Cancelling)
pub fn switch_trading_mode(state: &UiState, action: KeyAction) -> bool {
    if let Ok(mut s) = state.lock() {
        match action {
            KeyAction::ToggleTrading => {
                match s.trading_mode {
                    TradingMode::Stop => {
                        s.trading_mode = TradingMode::RealRun;
                        tracing::info!("🟢 Режим: REAL RUN (реальная торговля)");
                    }
                    TradingMode::RealRun => {
                        s.trading_mode = TradingMode::Stop;
                        tracing::info!("⏸️ Режим: STOP (софт стоит афк)");
                    }
                    TradingMode::Cancelling | TradingMode::Hedge => {
                        // Из Cancelling/Hedge нельзя переключить Space-ом
                    }
                }
            }
            KeyAction::ActivateCancelling => {
                s.trading_mode = TradingMode::Cancelling;
                tracing::info!("🟡 Режим: CANCELLING (нажмите x для отмены ордеров)");
            }
            KeyAction::ToggleHedge => match s.trading_mode {
                TradingMode::Hedge => {
                    s.trading_mode = TradingMode::Stop;
                    tracing::info!("⏸️ Режим: STOP (выход из Hedge)");
                }
                _ => {
                    s.trading_mode = TradingMode::Hedge;
                    s.hedge_input_state = HedgeInputState::None;
                    tracing::info!("🔵 Режим: HEDGE (нажмите u/d для покупки)");
                }
            },
            KeyAction::CancelAllOrders => {
                // Отменить ордера и вернуться в Stop
                s.trading_mode = TradingMode::Stop;
                tracing::info!("⏸️ Режим: STOP (ордера отменяются...)");
                return true; // Сигнал для отмены ордеров
            }
            _ => return false,
        }
    }
    false
}

/// Обновить портфолио
pub fn update_portfolio(state: &UiState, portfolio: Portfolio) {
    if let Ok(mut s) = state.lock() {
        s.portfolio = portfolio;
    }
}

/// Обновить стакан UP
pub fn update_up_book(
    state: &UiState,
    bids: [OrderLevel; ORDER_BOOK_DEPTH],
    asks: [OrderLevel; ORDER_BOOK_DEPTH],
) {
    if let Ok(mut s) = state.lock() {
        s.up_book.bids = bids;
        s.up_book.asks = asks;
    }
}

/// Обновить стакан DOWN
pub fn update_down_book(
    state: &UiState,
    bids: [OrderLevel; ORDER_BOOK_DEPTH],
    asks: [OrderLevel; ORDER_BOOK_DEPTH],
) {
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

/// Установить конфигурацию для отображения
pub fn set_config(
    state: &UiState,
    max_balance: f64,
    size: f64,
    max_size_side: f64,
    seconds_before_start: i64,
    legs_strategy: String,
) {
    if let Ok(mut s) = state.lock() {
        s.config = UiConfig {
            max_balance,
            size,
            max_size_side,
            seconds_before_start,
            legs_strategy,
        };
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

    // Основной layout: левая часть и правая
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(area);

    // Левая часть: event info, portfolio, open orders, history
    let [event_area, portfolio_area, open_orders_area, history_area] = Layout::vertical([
        Constraint::Length(6),  // Event info
        Constraint::Length(9),  // Portfolio
        Constraint::Length(14), // Open Orders
        Constraint::Fill(1),    // History
    ])
    .areas(left_area);

    render_event_info(frame, event_area, &state.event_info, state.trading_mode);
    render_portfolio(frame, portfolio_area, &state.portfolio, state.trading_mode);
    render_open_orders(frame, open_orders_area, &state.open_orders);
    render_history(frame, history_area, &state.trade_history);

    // Правая часть: Configuration (6 строк) или Hedge (12 строк) + Order Book (остаток)
    let config_height = if state.trading_mode == TradingMode::Hedge {
        12
    } else {
        7
    };
    let [config_area, order_book_area] =
        Layout::vertical([Constraint::Length(config_height), Constraint::Fill(1)])
            .areas(right_area);

    // Показываем окно Hedge вместо Configuration в режиме Hedge
    if state.trading_mode == TradingMode::Hedge {
        render_hedge(frame, config_area, &state.hedge_input_state);
    } else {
        render_configuration(frame, config_area, &state.config);
    }
    render_combined_order_book(
        frame,
        order_book_area,
        &state.up_book,
        &state.down_book,
        &state.our_up_bid_prices,
        &state.our_down_bid_prices,
    );
}

/// Рендер информации о событии
fn render_event_info(frame: &mut Frame, area: Rect, info: &EventInfo, trading_mode: TradingMode) {
    // Заголовок с индикатором режима
    let title = match trading_mode {
        TradingMode::Stop => " MARKET [STOP] ",
        TradingMode::RealRun => " MARKET [REAL RUN] ",
        TradingMode::Cancelling => " MARKET [CANCELLING] ",
        TradingMode::Hedge => " MARKET [HEDGE] ",
    };

    let title_color = match trading_mode {
        TradingMode::Stop => Color::DarkGray,
        TradingMode::RealRun => Color::Green,
        TradingMode::Cancelling => Color::Yellow,
        TradingMode::Hedge => Color::Blue,
    };

    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let remaining = info.remaining_seconds();
    let total = info.total_seconds;

    // Title, URL, time, progress
    let [title_area, url_area, time_area, progress_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

    // Название события
    let event_title = Paragraph::new(info.title.as_str()).style(Style::default().fg(Color::White));
    frame.render_widget(event_title, title_area);

    // URL (Termius сделает его кликабельным)
    if !info.slug.is_empty() {
        let url = format!("https://polymarket.com/event/{}", info.slug);
        let url_paragraph = Paragraph::new(url).style(Style::default().fg(Color::DarkGray));
        frame.render_widget(url_paragraph, url_area);
    }

    // Время в формате MM:SS
    let remaining_mins = remaining / 60;
    let remaining_secs = remaining % 60;
    let total_mins = total / 60;
    let total_secs = total % 60;
    let time_text = format!(
        "Time: {:02}:{:02} / {:02}:{:02}",
        remaining_mins, remaining_secs, total_mins, total_secs
    );
    let time = Paragraph::new(time_text).style(Style::default().fg(Color::Yellow));
    frame.render_widget(time, time_area);

    // Progress bar
    let gauge = Gauge::default()
        .gauge_style(Style::default().fg(Color::Magenta).bg(Color::DarkGray))
        .ratio(info.progress_ratio())
        .label(format!("{:.0}%", info.progress_ratio() * 100.0));
    frame.render_widget(gauge, progress_area);
}

/// Рендер портфолио
fn render_portfolio(
    frame: &mut Frame,
    area: Rect,
    portfolio: &Portfolio,
    trading_mode: TradingMode,
) {
    let block = Block::default()
        .title(" PORTFOLIO ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let up_avg = portfolio.up_avg();
    let down_avg = portfolio.down_avg();
    let total_avg = up_avg + down_avg;
    let total_spent = portfolio.up_spent + portfolio.down_spent;

    // Создаем текст с информацией о позициях
    let up_line = vec![
        Span::styled("  UP: ", Style::default().fg(Color::Green)),
        Span::raw(format!(
            "{:.1} shares @ avg {:.3}",
            portfolio.up_shares, up_avg
        )),
        Span::styled(
            format!("  ${:.2}", portfolio.up_spent),
            Style::default().fg(Color::Gray),
        ),
    ];

    let down_line = vec![
        Span::styled("DOWN: ", Style::default().fg(Color::Red)),
        Span::raw(format!(
            "{:.1} shares @ avg {:.3}",
            portfolio.down_shares, down_avg
        )),
        Span::styled(
            format!("  ${:.2}", portfolio.down_spent),
            Style::default().fg(Color::Gray),
        ),
    ];

    let text = vec![
        Line::from(up_line),
        Line::from(down_line),
        Line::from(""),
        Line::from(vec![
            Span::styled("Total Avg: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("{:.3}", total_avg),
                Style::default().fg(if total_avg < 0.97 {
                    Color::Green
                } else if total_avg < 1.0 {
                    Color::Yellow
                } else {
                    Color::Red
                }),
            ),
            Span::raw("  |  "),
            Span::styled("Spent: ", Style::default().fg(Color::White)),
            Span::styled(
                format!("${:.2}", total_spent),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("Maker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.maker_trades)),
            Span::raw("  |  "),
            Span::styled("Taker: ", Style::default().fg(Color::Gray)),
            Span::raw(format!("{}", portfolio.taker_trades)),
        ]),
        Line::from(""),
        // Добавляем строку с индикатором режима
        Line::from(vec![
            Span::styled("Mode: ", Style::default().fg(Color::White)),
            Span::styled(
                match trading_mode {
                    TradingMode::Stop => "STOP",
                    TradingMode::RealRun => "REAL RUN",
                    TradingMode::Cancelling => "CANCELLING (x to cancel)",
                    TradingMode::Hedge => "HEDGE (u/d to buy, b to exit)",
                },
                Style::default()
                    .fg(match trading_mode {
                        TradingMode::Stop => Color::DarkGray,
                        TradingMode::RealRun => Color::Green,
                        TradingMode::Cancelling => Color::Yellow,
                        TradingMode::Hedge => Color::Blue,
                    })
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Рендер окна Hedge (вместо Configuration в режиме Hedge)
fn render_hedge(frame: &mut Frame, area: Rect, hedge_input: &HedgeInputState) {
    let block = Block::default()
        .title(" HEDGE ")
        .title_style(
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = match hedge_input {
        HedgeInputState::RequestingUp(input) => {
            vec![
                Line::from(vec![
                    Span::styled("Mode: ", Style::default().fg(Color::White)),
                    Span::styled(
                        "Manual Hedging",
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "Enter UP shares to buy:",
                    Style::default().fg(Color::Yellow),
                )]),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::White)),
                    Span::styled(
                        if input.is_empty() { "_" } else { input },
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Price: ", Style::default().fg(Color::Gray)),
                    Span::styled("0.99 ", Style::default().fg(Color::Green)),
                    Span::styled("(instant fill)", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'y'",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to confirm or ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'n'",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to cancel", Style::default().fg(Color::Gray)),
                ]),
            ]
        }
        HedgeInputState::RequestingDown(input) => {
            vec![
                Line::from(vec![
                    Span::styled("Mode: ", Style::default().fg(Color::White)),
                    Span::styled(
                        "Manual Hedging",
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "Enter DOWN shares to buy:",
                    Style::default().fg(Color::Yellow),
                )]),
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::White)),
                    Span::styled(
                        if input.is_empty() { "_" } else { input },
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Price: ", Style::default().fg(Color::Gray)),
                    Span::styled("0.99 ", Style::default().fg(Color::Green)),
                    Span::styled("(instant fill)", Style::default().fg(Color::DarkGray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'y'",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to confirm or ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'n'",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to cancel", Style::default().fg(Color::Gray)),
                ]),
            ]
        }
        HedgeInputState::None => {
            vec![
                Line::from(vec![
                    Span::styled("Mode: ", Style::default().fg(Color::White)),
                    Span::styled(
                        "Manual Hedging",
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Line::from(""),
                Line::from(vec![Span::styled(
                    "Instructions:",
                    Style::default().fg(Color::Yellow),
                )]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'u'",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to buy UP shares", Style::default().fg(Color::Gray)),
                ]),
                Line::from(vec![
                    Span::styled("Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'d'",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to buy DOWN shares", Style::default().fg(Color::Gray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Press ", Style::default().fg(Color::Gray)),
                    Span::styled(
                        "'b'",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" to exit Hedge mode", Style::default().fg(Color::Gray)),
                ]),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Note: ", Style::default().fg(Color::Yellow)),
                    Span::styled(
                        "Orders placed @ 0.99 for instant fill",
                        Style::default().fg(Color::DarkGray),
                    ),
                ]),
            ]
        }
    };

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Рендер открытых ордеров
fn render_open_orders(frame: &mut Frame, area: Rect, orders: &[OpenOrder]) {
    let block = Block::default()
        .title(" OPEN ORDERS ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let header_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);

    let rows: Vec<Row> = orders
        .iter()
        .map(|order| {
            let outcome_color = if order.is_up {
                Color::Green
            } else {
                Color::Red
            };
            let outcome_text = if order.is_up { "Up" } else { "Down" };

            Row::new(vec![
                Cell::from("Buy").style(Style::default().fg(Color::White)),
                Cell::from(outcome_text).style(Style::default().fg(outcome_color)),
                Cell::from(format!("{:.0}¢", order.price * 100.0))
                    .style(Style::default().fg(Color::White)),
                Cell::from(format!("{:.0} / {:.0}", order.filled, order.total))
                    .style(Style::default().fg(Color::White)),
                Cell::from(format!("${:.2}", order.total_cost()))
                    .style(Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(6),  // Side
            Constraint::Length(8),  // Outcome
            Constraint::Length(8),  // Price
            Constraint::Length(10), // Filled
            Constraint::Length(8),  // Total
        ],
    )
    .header(Row::new(vec!["Side", "Outcome", "Price", "Filled", "Total"]).style(header_style));
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
fn render_history(frame: &mut Frame, area: Rect, history: &VecDeque<TradeHistoryEntry>) {
    let block = Block::default()
        .title(" HISTORY ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let height = inner.height as usize;

    if history.is_empty() {
        let empty_msg =
            Paragraph::new("No trades yet...").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(empty_msg, inner);
        return;
    }

    // Показываем записи с начала (новые сверху)
    let visible_entries: Vec<Line> = history
        .iter()
        .take(height)
        .enumerate()
        .map(|(idx, entry)| {
            let number = idx + 1;
            let outcome_color = if entry.is_up {
                Color::Green
            } else {
                Color::Red
            };
            let outcome_text = if entry.is_up { "Up" } else { "Down" };
            let trade_type_str = match entry.trade_type {
                TradeType::Maker => "Maker",
                TradeType::Taker => "Taker",
            };
            let time_ago = format_time_ago(entry.timestamp);

            // Формат: "1. Bought 5.00 Up at 36¢($1.8)      Maker      14m 00s"
            Line::from(vec![
                Span::styled(
                    format!("{:>2}. ", number),
                    Style::default().fg(Color::White),
                ),
                Span::styled("Bought ", Style::default().fg(Color::White)),
                Span::styled(
                    format!("{:.2} ", entry.shares),
                    Style::default().fg(outcome_color),
                ),
                Span::styled(
                    format!("{} ", outcome_text),
                    Style::default().fg(outcome_color),
                ),
                Span::styled("at ", Style::default().fg(Color::White)),
                Span::styled(
                    format!("{:.0}¢", entry.price * 100.0),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("(${:.2})", entry.cost),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("      {:<6}", trade_type_str),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("      {}", time_ago),
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        })
        .collect();

    let paragraph = Paragraph::new(visible_entries);
    frame.render_widget(paragraph, inner);
}

/// Рендер конфигурации
fn render_configuration(frame: &mut Frame, area: Rect, config: &UiConfig) {
    let block = Block::default()
        .title(" CONFIGURATION ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let text = vec![
        Line::from(vec![
            Span::styled("max_balance = ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}", config.max_balance),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("size = ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}", config.size),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("max_size_side = ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{:.1}", config.max_size_side),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("seconds_before_start = ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}", config.seconds_before_start),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("legs_strategy = ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("\"{}\"", config.legs_strategy),
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, inner);
}

/// Рендер объединенного стакана (только bids для UP и DOWN)
fn render_combined_order_book(
    frame: &mut Frame,
    area: Rect,
    up_book: &SideOrderBook,
    down_book: &SideOrderBook,
    our_up_bid_prices: &HashSet<u32>,
    our_down_bid_prices: &HashSet<u32>,
) {
    let block = Block::default()
        .title(" ORDER BOOK ")
        .title_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Разделяем на UP bids, spread, DOWN bids
    let [up_bids_area, spread_area, down_bids_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);

    let header_style = Style::default()
        .fg(Color::Gray)
        .add_modifier(Modifier::BOLD);

    // UP Bids (зеленые) - перевернутые, чтобы лучшая цена была внизу у spread
    // Сначала собираем данные с кумулятивным total
    let up_bid_data: Vec<(f64, f64, f64, bool)> = {
        let mut cumulative_up = 0.0;
        up_book
            .bids
            .iter()
            .filter(|l| l.size > 0.0)
            .map(|level| {
                cumulative_up += level.price * level.size;
                let price_cents = (level.price * 100.0).round() as u32;
                let has_our_order = our_up_bid_prices.contains(&price_cents);
                (level.price, level.size, cumulative_up, has_our_order)
            })
            .collect()
    };

    // Переворачиваем: худшие цены сверху, лучшие внизу
    let up_bid_rows: Vec<Row> = up_bid_data
        .iter()
        .rev()
        .map(|(price, size, cum_total, has_our_order)| {
            let price_text = if *has_our_order {
                format!("{:.0}¢⏱", price * 100.0)
            } else {
                format!("{:.0}¢", price * 100.0)
            };
            Row::new(vec![
                Cell::from(price_text).style(Style::default().fg(Color::Green)),
                Cell::from(format!("{:.2}", size)),
                Cell::from(format!("${:.2}", cum_total)),
            ])
        })
        .collect();

    let up_table = Table::new(
        up_bid_rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(35),
            Constraint::Percentage(35),
        ],
    )
    .header(Row::new(vec![
        Cell::from("Up Bids").style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("Shares").style(header_style),
        Cell::from("Total").style(header_style),
    ]));
    frame.render_widget(up_table, up_bids_area);

    // Spread
    let up_best_bid = up_book.bids.first().map(|l| l.price).unwrap_or(0.0);
    let down_best_bid = down_book.bids.first().map(|l| l.price).unwrap_or(0.0);
    let spread = ((1.0 - up_best_bid - down_best_bid) * 100.0).abs();

    let spread_text = format!("───── Spread: {:.0}¢ ─────", spread);
    let spread_widget = Paragraph::new(spread_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Center);
    frame.render_widget(spread_widget, spread_area);

    // DOWN Bids (красные)
    let mut cumulative_down = 0.0;
    let down_bid_rows: Vec<Row> = down_book
        .bids
        .iter()
        .filter(|l| l.size > 0.0)
        .map(|level| {
            cumulative_down += level.price * level.size;
            let price_cents = (level.price * 100.0).round() as u32;
            let has_our_order = our_down_bid_prices.contains(&price_cents);
            let price_text = if has_our_order {
                format!("{:.0}¢⏱", level.price * 100.0)
            } else {
                format!("{:.0}¢", level.price * 100.0)
            };
            Row::new(vec![
                Cell::from(price_text).style(Style::default().fg(Color::Red)),
                Cell::from(format!("{:.2}", level.size)),
                Cell::from(format!("${:.2}", cumulative_down)),
            ])
        })
        .collect();

    let down_table = Table::new(
        down_bid_rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(35),
            Constraint::Percentage(35),
        ],
    )
    .header(Row::new(vec![
        Cell::from("Down Bids").style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Cell::from("Shares").style(header_style),
        Cell::from("Total").style(header_style),
    ]));
    frame.render_widget(down_table, down_bids_area);
}

/// Результат проверки нажатых клавиш
#[derive(Clone, Copy)]
pub enum KeyAction {
    None,
    Exit,               // 'q' - выход
    ToggleTrading,      // Space - переключить Stop/RealRun
    ActivateCancelling, // 'c' - войти в режим Cancelling
    CancelAllOrders,    // 'x' - отменить все ордера (только в Cancelling)
    ToggleHedge,        // 'b' - переключить режим Hedge
    RequestUpHedge,     // 'u' - запросить количество UP для покупки (только в Hedge)
    RequestDownHedge,   // 'd' - запросить количество DOWN для покупки (только в Hedge)
    ConfirmHedge,       // 'y' - подтвердить покупку
    CancelHedgeInput,   // 'n' - отменить ввод
}

/// Проверка нажатия клавиш и обработка ввода hedge
/// Возвращает (KeyAction, Option<HedgeInputState>)
/// - KeyAction - действие для обработки
/// - Option<HedgeInputState> - новое состояние ввода hedge если изменилось
pub fn check_key_action(
    current_mode: TradingMode,
    hedge_input: &HedgeInputState,
) -> (KeyAction, Option<HedgeInputState>) {
    if event::poll(std::time::Duration::from_millis(50)).unwrap_or(false) {
        if let Ok(Event::Key(key)) = event::read() {
            if key.kind == KeyEventKind::Press {
                // Сначала обрабатываем ввод текста в режиме hedge (цифры, backspace)
                match hedge_input {
                    HedgeInputState::RequestingUp(input)
                    | HedgeInputState::RequestingDown(input) => {
                        match key.code {
                            KeyCode::Char(c) if c.is_ascii_digit() || c == '.' => {
                                let mut new_input = input.clone();
                                new_input.push(c);
                                let new_state = match hedge_input {
                                    HedgeInputState::RequestingUp(_) => {
                                        HedgeInputState::RequestingUp(new_input)
                                    }
                                    HedgeInputState::RequestingDown(_) => {
                                        HedgeInputState::RequestingDown(new_input)
                                    }
                                    _ => unreachable!(),
                                };
                                return (KeyAction::None, Some(new_state));
                            }
                            KeyCode::Backspace => {
                                let mut new_input = input.clone();
                                new_input.pop();
                                let new_state = match hedge_input {
                                    HedgeInputState::RequestingUp(_) => {
                                        HedgeInputState::RequestingUp(new_input)
                                    }
                                    HedgeInputState::RequestingDown(_) => {
                                        HedgeInputState::RequestingDown(new_input)
                                    }
                                    _ => unreachable!(),
                                };
                                return (KeyAction::None, Some(new_state));
                            }
                            _ => {
                                // Для других клавиш продолжаем обработку ниже
                            }
                        }
                    }
                    _ => {}
                }

                // Обрабатываем остальные клавиши
                match key.code {
                    KeyCode::Char('q') => return (KeyAction::Exit, None),
                    KeyCode::Char(' ') => return (KeyAction::ToggleTrading, None),
                    KeyCode::Char('c') => return (KeyAction::ActivateCancelling, None),
                    KeyCode::Char('b') => {
                        // 'b' переключает Hedge режим (только когда не в режиме ввода)
                        if *hedge_input == HedgeInputState::None {
                            return (KeyAction::ToggleHedge, None);
                        }
                    }
                    KeyCode::Char('n') => {
                        // 'n' отменяет ввод hedge (остаемся в режиме Hedge)
                        if !matches!(hedge_input, HedgeInputState::None) {
                            return (KeyAction::CancelHedgeInput, None);
                        }
                    }
                    KeyCode::Char('x') => {
                        // x работает только в режиме Cancelling
                        if current_mode == TradingMode::Cancelling {
                            return (KeyAction::CancelAllOrders, None);
                        }
                    }
                    KeyCode::Char('u') => {
                        // u работает только в режиме Hedge
                        if current_mode == TradingMode::Hedge
                            && *hedge_input == HedgeInputState::None
                        {
                            return (KeyAction::RequestUpHedge, None);
                        }
                    }
                    KeyCode::Char('d') => {
                        // d работает только в режиме Hedge
                        if current_mode == TradingMode::Hedge
                            && *hedge_input == HedgeInputState::None
                        {
                            return (KeyAction::RequestDownHedge, None);
                        }
                    }
                    KeyCode::Char('y') => {
                        // y подтверждает ввод hedge
                        if !matches!(hedge_input, HedgeInputState::None) {
                            return (KeyAction::ConfirmHedge, None);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    (KeyAction::None, None)
}

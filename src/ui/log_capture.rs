use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;
use super::{UiState, add_log};

/// Visitor для извлечения сообщения из tracing Event
struct MessageVisitor {
    message: String,
}

impl MessageVisitor {
    fn new() -> Self {
        Self { message: String::new() }
    }
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
            // Убираем кавычки если есть
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len()-1].to_string();
            }
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        }
    }
}

/// Tracing Layer для захвата логов и отправки в UI
pub struct UiLogLayer {
    ui_state: UiState,
}

impl UiLogLayer {
    pub fn new(ui_state: UiState) -> Self {
        Self { ui_state }
    }
}

impl<S> Layer<S> for UiLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::new();
        event.record(&mut visitor);

        if !visitor.message.is_empty() {
            let level = event.metadata().level();
            let timestamp = chrono::Local::now().format("%H:%M:%S");

            let formatted = format!(
                "[{}] {} {}",
                timestamp,
                level,
                visitor.message
            );

            add_log(&self.ui_state, formatted);
        }
    }
}

use tracing::{Event, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Tracing Layer - теперь не отправляет логи в UI, только для файлового логирования
/// UI теперь использует отдельную систему истории торговли
pub struct UiLogLayer;

impl UiLogLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for UiLogLayer
where
    S: Subscriber,
{
    fn on_event(&self, _event: &Event<'_>, _ctx: Context<'_, S>) {
        // Логи больше не отправляются в UI
        // Файловое логирование происходит через tracing_appender в main.rs
    }
}

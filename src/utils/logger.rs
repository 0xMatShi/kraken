use time::macros::format_description;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Инициализирует систему логирования
/// Логи пишутся в файл ./logs/app.log и в UI
///
/// ВАЖНО: возвращает WorkerGuard, который ОБЯЗАТЕЛЬНО нужно сохранить
/// на протяжении всей работы программы, иначе логирование в файл прекратится!
pub fn init_logger() -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    // Настройка tracing логгера с кастомным форматом времени
    let timer = OffsetTime::new(
        time::UtcOffset::UTC,
        format_description!("[hour]:[minute]:[second].[subsecond digits:3]"),
    );

    // Фильтр логов: по умолчанию WARN для всех библиотек, INFO для нашего проекта
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,mmdnca=info"));

    // Создаем неблокирующий файловый appender (один файл без ротации)
    let file_appender = tracing_appender::rolling::never("./logs", "app.log");
    let (non_blocking_file, log_guard) = tracing_appender::non_blocking(file_appender);

    // Слой для записи в файл (без цветов)
    let file_layer = fmt::layer()
        .with_target(false)
        .with_thread_ids(false)
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_ansi(false)
        .with_span_events(FmtSpan::NONE)
        .with_timer(timer)
        .with_writer(non_blocking_file);

    // UI лог слой (логи записываются только в файл, не в UI)
    let ui_log_layer = crate::ui::UiLogLayer::new();

    // Объединяем слои и инициализируем
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(ui_log_layer)
        .init();

    Ok(log_guard)
}

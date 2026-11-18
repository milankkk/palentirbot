use std::fmt::Result as FmtResult;

use time::format_description::FormatItem;
use tracing::{Event, Subscriber};
use tracing_appender::{
    non_blocking::{NonBlocking, WorkerGuard},
    rolling,
};
use tracing_subscriber::{
    fmt::{
        format::Writer,
        time::{FormatTime, UtcTime},
        FmtContext, FormatEvent, FormatFields, Layer,
    },
    layer::SubscriberExt,
    registry::LookupSpan,
    util::SubscriberInitExt,
    EnvFilter, Layer as _,
};

pub fn init() -> Option<WorkerGuard> {
    // Displays ERROR, WARN, and INFO from palentirbot
    // and ERROR and WARN from dependencies
    let stdout_filter: EnvFilter = concat!(env!("CARGO_PKG_NAME"), "=info,warn")
        .parse()
        .unwrap();

    let stdout_layer = Layer::default()
        .event_format(StdoutEventFormat::default())
        .with_filter(stdout_filter);

    match rolling::Builder::new()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix("shishabot.log")
        .build("./logs")
    {
        Ok(file_appender) => {
            let (file_writer, guard) = NonBlocking::new(file_appender);

            // Check RUST_LOG in .env, if it's not found it'll
            // display ERROR, WARN, INFO, DEBUG, and TRACE from palentirbot
            // and ERROR, WARN, INFO from dependencies.
            let file_filter = match EnvFilter::try_from_default_env() {
                Ok(filter) => filter,
                Err(_) => concat!(env!("CARGO_PKG_NAME"), "=trace,info")
                    .parse()
                    .unwrap(),
            };

            let file_layer = Layer::default()
                .event_format(FileEventFormat::default())
                .with_writer(file_writer)
                .with_filter(file_filter);

            tracing_subscriber::registry()
                .with(stdout_layer)
                .with(file_layer)
                .init();

            Some(guard)
        }
        Err(err) => {
            eprintln!(
                "Warning: failed to initialize file logger in ./logs ({err}); continuing with stdout logging only"
            );
            tracing_subscriber::registry().with(stdout_layer).init();
            None
        }
    }
}

struct StdoutEventFormat {
    timer: UtcTime<&'static [FormatItem<'static>]>,
}

impl Default for StdoutEventFormat {
    fn default() -> Self {
        Self {
            timer: UtcTime::new(DATETIME_FORMAT),
        }
    }
}

impl<S, N> FormatEvent<S, N> for StdoutEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> FmtResult {
        self.timer.format_time(&mut writer)?;
        let metadata = event.metadata();

        write!(writer, " {:>5} ", metadata.level(),)?;

        ctx.field_format().format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

struct FileEventFormat {
    timer: UtcTime<&'static [FormatItem<'static>]>,
}

impl Default for FileEventFormat {
    fn default() -> Self {
        Self {
            timer: UtcTime::new(DATETIME_FORMAT),
        }
    }
}

impl<S, N> FormatEvent<S, N> for FileEventFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> FmtResult {
        self.timer.format_time(&mut writer)?;
        let metadata = event.metadata();

        write!(
            writer,
            " {:>5} [{}:{}] ",
            metadata.level(),
            metadata.file().unwrap_or_else(|| metadata.target()),
            metadata.line().unwrap_or(0),
        )?;

        ctx.field_format().format_fields(writer.by_ref(), event)?;

        writeln!(writer)
    }
}

pub const DATETIME_FORMAT: &[FormatItem<'_>] =
    time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

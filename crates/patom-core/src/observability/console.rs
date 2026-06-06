//! Console formatter: `<ts> <LEVEL> <fields…>`, no parent-span chain.
//!
//! Events that need a span handle (session id, agent id, …) carry it as an
//! explicit `patom.*` field. The OTel layer keeps the span tree for backend
//! queries; the console is for humans tailing stderr.

use std::fmt;
use std::io;

use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::fmt::{
    FmtContext, FormatEvent, FormatFields,
    format::Writer,
    time::{FormatTime, SystemTime},
};
use tracing_subscriber::registry::LookupSpan;

pub(super) fn layer<S>() -> impl Layer<S>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(io::stderr)
        .event_format(CompactNoSpan)
}

struct CompactNoSpan;

impl<S, N> FormatEvent<S, N> for CompactNoSpan
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        SystemTime.format_time(&mut writer)?;
        // Right-pad to 5 chars so columns align across levels.
        let level = match *event.metadata().level() {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN ",
            Level::INFO => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };
        write!(writer, " {level} ")?;
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

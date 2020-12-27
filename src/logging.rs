use crate::Result;
use flexi_logger::{
    Cleanup, Criterion, DeferredNow, Duplicate, Logger, Naming, ReconfigurationHandle,
};
use futures::future::BoxFuture;
use log::Record;
use tide::{Middleware, Next, Request, Response};

pub(crate) fn log_format(
    w: &mut dyn std::io::Write,
    now: &mut DeferredNow,
    record: &Record,
) -> std::result::Result<(), std::io::Error> {
    write!(
        w,
        "[{}] {} [{}] {}:{}: {}",
        now.now().format("%Y-%m-%d %H:%M:%S%.6f %:z"),
        record.level(),
        record.metadata().target(),
        //record.module_path().unwrap_or("<unnamed>"),
        record.file().unwrap_or("<unnamed>"),
        record.line().unwrap_or(0),
        &record.args()
    )
}

pub(crate) fn set_up_logging(log_to_file: bool) -> Result<ReconfigurationHandle> {
    let mut logger = Logger::with_env_or_str("debug").format(log_format);
    if log_to_file {
        logger = logger
            .log_to_file()
            .directory("logs")
            .duplicate_to_stderr(Duplicate::All)
            .rotate(
                Criterion::Size(100_000_000),
                Naming::Timestamps,
                Cleanup::KeepLogFiles(10),
            );
    }

    let handle = logger.start()?;

    Ok(handle)
}

/*
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestLogger;

impl RequestLogger {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    async fn log_basic<'a, State: Send + Sync + 'static>(
        &'a self,
        ctx: Request<State>,
        next: Next<'a, State>,
    ) -> Response {
        let path = ctx.url().path();
        let method = ctx.method().as_ref();
        trace!(target:"server","IN => {} {}", method, path);
        let start = std::time::Instant::now();
        let res = next.run(ctx).await;
        let status = res.status();
        info!(target:"server",
              "{} {} {} {}µs",
              method,
              path,
              status.as_str(),
              start.elapsed().as_micros()
        );
        res
    }
}

impl<State: Send + Sync + 'static> Middleware<State> for RequestLogger {
    fn handle<'a>(&'a self, ctx: Request<State>, next: Next<'a, State>) -> BoxFuture<'a, Response> {
        Box::pin(async move { self.log_basic(ctx, next).await })
    }
}
*/

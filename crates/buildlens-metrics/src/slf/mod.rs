mod lexer;
mod model;
mod parser;

pub use lexer::SlfError;
pub use model::{IdeActivityLog, IdeSection};
// Only unit tests build these directly.
#[allow(unused_imports)]
pub use model::{DvtLocation, IdeMessage};
pub use parser::{ParseOptions, parse};

/// CFAbsoluteTime epoch (2001-01-01) offset from the Unix epoch.
pub const CF_ABSOLUTE_TIME_OFFSET: f64 = 978_307_200.0;

pub fn cf_to_unix(cf_absolute_time: f64) -> f64 {
    cf_absolute_time + CF_ABSOLUTE_TIME_OFFSET
}

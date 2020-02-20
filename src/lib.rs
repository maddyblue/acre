pub mod acme;
pub mod conn;
pub mod dial;
pub mod fid;
pub mod fsys;

use std::error::Error;
use std::fmt;

type Result<T> = std::result::Result<T, Box<dyn Error + Sync + Send>>;

pub fn err_str(error: String) -> Box<ErrorStr> {
	Box::new(ErrorStr { error })
}

#[derive(Debug)]
pub struct ErrorStr {
	error: String,
}

impl Error for ErrorStr {
	fn description(&self) -> &str {
		self.error.as_str()
	}
}

impl fmt::Display for ErrorStr {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		f.write_str(&self.error.to_string())
	}
}

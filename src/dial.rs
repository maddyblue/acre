extern crate regex;

use crate::{
	conn::{Conn, RcConn},
	fid, fsys, Result,
};
use lazy_static::lazy_static;
use regex::Regex;
use std::cell::RefCell;
use std::env;
use std::os::unix::net::UnixStream;
use std::rc::Rc;

pub fn dial(addr: &str) -> Result<RcConn> {
	let stream = UnixStream::connect(addr)?;
	let conn = Conn::new(stream)?;
	Ok(RcConn {
		rc: Rc::new(RefCell::new(conn)),
	})
}

pub fn dial_service(service: &str) -> Result<RcConn> {
	let ns = namespace();
	dial((ns + "/" + service).as_str())
}

pub fn mount_service(service: &str) -> Result<fsys::Fsys> {
	let mut conn = dial_service(service)?;
	let fsys = conn.attach(fid::get_user(), "".to_string())?;
	Ok(fsys)
}

// namespace returns the path to the name space directory.
pub fn namespace() -> String {
	if let Ok(val) = env::var("NAMESPACE") {
		return val;
	}
	let mut disp = if let Ok(val) = env::var("DISPLAY") {
		val
	} else {
		// No $DISPLAY? Use :0.0 for non-X11 GUI (OS X).
		String::from(":0.0")
	};

	lazy_static! {
		static ref DOT_ZERO: Regex = Regex::new(r"\A(.*:\d+)\.0\z").unwrap();
	}
	// Canonicalize: xxx:0.0 => xxx:0.
	if let Some(m) = DOT_ZERO.captures(disp.as_str()) {
		disp = m.get(1).unwrap().as_str().to_string();
	}
	format!("/tmp/ns.{}.{}", env::var("USER").unwrap(), disp)
}

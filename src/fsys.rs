extern crate nine;

use crate::fid::Fid;
use nine::p2000::OpenMode;

pub struct Fsys {
	pub fid: Fid,
}

impl Fsys {
	pub fn open(&mut self, name: &str, mode: OpenMode) -> Result<Fid, String> {
		let mut fid = self.fid.walk(name)?;
		fid.open(mode)?;
		Ok(fid)
	}
}

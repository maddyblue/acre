extern crate nine;

use crate::{conn::RefConn, Result};
use nine::p2000::{OpenMode, Qid};
use std::cmp;
use std::env;
use std::io;
use std::rc::Rc;

pub fn get_user() -> String {
	env::var("USER").unwrap()
}

pub struct Fid {
	pub c: RefConn,

	pub qid: Qid,
	pub fid: u32,
	pub mode: OpenMode,
	offset: u64,
}

impl Fid {
	pub fn new(c: RefConn, fid: u32, qid: Qid) -> Fid {
		Fid {
			c,
			qid,
			fid,
			mode: OpenMode::READ,
			offset: 0,
		}
	}
	pub fn walk(&mut self, name: &str) -> Result<Fid> {
		let mut c = self.c.borrow_mut();
		let wfid = c.newfid();
		let mut fid = self.fid;

		let name = String::from(name);
		let mut elem: Vec<String> = name
			.split("/")
			.filter(|&x| x != "" && x != ".")
			.map(|x| x.to_string())
			.collect();
		let mut qid: Qid;

		const MAXWELEM: usize = 16;
		loop {
			let n = cmp::min(elem.len(), MAXWELEM);
			let wname = elem[0..n].to_vec();
			elem.drain(0..n);
			let qids = c.walk(fid, wfid, wname)?;
			qid = if n == 0 {
				self.qid.clone()
			} else {
				qids[n - 1].clone()
			};
			if elem.len() == 0 {
				break;
			}
			fid = wfid;
		}
		Ok(Fid::new(Rc::clone(&self.c), wfid, qid))
	}

	pub fn open(&mut self, mode: OpenMode) -> Result<()> {
		let mut c = self.c.borrow_mut();
		c.open(self.fid, mode)?;
		self.mode = mode;
		Ok(())
	}
}

const IOHDRSZ: u32 = 24;

impl io::Read for Fid {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let mut c = self.c.borrow_mut();
		let msize = c.msize - IOHDRSZ;
		let n: u32 = cmp::min(buf.len() as u32, msize);
		let data = match c.read(self.fid, self.offset, n) {
			Ok(r) => r,
			Err(e) => return Err(io::Error::new(io::ErrorKind::Other, format!("{}", e))),
		};
		for (i, x) in data.iter().enumerate() {
			buf[i] = *x
		}
		self.offset += data.len() as u64;
		Ok(data.len())
	}
}

impl Drop for Fid {
	fn drop(&mut self) {
		let mut c = self.c.borrow_mut();
		let _ = c.clunk(self.fid);
	}
}

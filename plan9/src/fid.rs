use crate::{conn::Conn, Result};
use nine::p2000::{OpenMode, Qid};
use std::cmp;
use std::env;
use std::io;

pub fn get_user() -> String {
	env::var("USER").unwrap()
}

pub struct Fid {
	pub c: Conn,

	pub qid: Qid,
	pub fid: u32,
	pub mode: OpenMode,
	offset: u64,
}

impl Fid {
	pub fn new(c: Conn, fid: u32, qid: Qid) -> Fid {
		Fid {
			c: c.clone(),
			qid,
			fid,
			mode: OpenMode::READ,
			offset: 0,
		}
	}
	pub fn walk(&mut self, name: &str) -> Result<Fid> {
		let wfid = self.c.newfid();
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
			let qids = self.c.walk(fid, wfid, wname)?;
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
		Ok(Fid::new(self.c.clone(), wfid, qid))
	}

	pub fn open(&mut self, mode: OpenMode) -> Result<()> {
		self.c.open(self.fid, mode)?;
		self.mode = mode;
		Ok(())
	}
}

const IOHDRSZ: u32 = 24;

impl io::Read for Fid {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let msize = self.c.msize - IOHDRSZ;
		let n: u32 = cmp::min(buf.len() as u32, msize);
		let data = match self.c.read(self.fid, self.offset, n) {
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

impl io::Seek for Fid {
	fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
		match pos {
			io::SeekFrom::Start(n) => self.offset = n,
			io::SeekFrom::Current(n) => {
				if n >= 0 {
					self.offset += n as u64;
				} else {
					self.offset -= n as u64;
				}
			}
			io::SeekFrom::End(_) => {
				return Err(io::Error::new(
					io::ErrorKind::Other,
					format!("seeking to end unsupported"),
				))
			}
		}
		Ok(self.offset)
	}
}

impl io::Write for Fid {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		let msize = (self.c.msize - IOHDRSZ) as usize;
		let mut tot: usize = 0;
		let n = buf.len();
		let mut first = true;
		while tot < n || first {
			let want: usize = cmp::min(n - tot, msize);
			let got = match self
				.c
				.write(self.fid, self.offset, buf[tot..tot + want].to_vec())
			{
				Ok(r) => r as usize,
				Err(e) => return Err(io::Error::new(io::ErrorKind::Other, format!("{}", e))),
			};
			tot += got;
			self.offset += got as u64;
			first = false;
		}
		Ok(tot)
	}
	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl Drop for Fid {
	fn drop(&mut self) {
		let _ = self.c.clunk(self.fid);
	}
}

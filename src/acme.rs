use crate::dial;
use crate::{err_str, fid::Fid, fsys::Fsys, Result};
use nine::p2000::OpenMode;
use std::io::{BufRead, BufReader, Read, Write};

fn mount() -> Result<Fsys> {
	dial::mount_service("acme")
}

#[derive(Debug)]
pub struct WinInfo {
	id: usize,
	name: String,
}

impl WinInfo {
	pub fn windows() -> Result<Vec<WinInfo>> {
		let mut fsys = mount()?;
		let index = fsys.open("index", OpenMode::READ)?;
		let r = BufReader::new(index);
		let mut ws = Vec::new();
		for line in r.lines() {
			if let Ok(line) = line {
				let sp: Vec<&str> = line.split_whitespace().collect();
				if sp.len() < 6 {
					continue;
				}
				ws.push(WinInfo {
					id: sp[0].parse()?,
					name: sp[5].to_string(),
				});
			}
		}
		Ok(ws)
	}
}

pub struct LogReader {
	f: Fid,
	buf: [u8; 8192],
}

#[derive(Debug)]
pub struct LogEvent {
	id: usize,
	op: String,
	name: String,
}

impl LogReader {
	pub fn new() -> Result<LogReader> {
		let mut fsys = mount()?;
		let log = fsys.open("log", OpenMode::READ)?;
		Ok(LogReader {
			f: log,
			buf: [0; 8192],
		})
	}
	pub fn read(&mut self) -> Result<LogEvent> {
		let sz = self.f.read(&mut self.buf)?;
		let data = String::from_utf8(self.buf[0..sz].to_vec())?;
		let sp: Vec<String> = data.splitn(3, " ").map(|x| x.to_string()).collect();
		if sp.len() != 3 {
			return Err(err_str("malformed log event".to_string()));
		}
		let id = sp[0].parse()?;
		let op = sp[1].to_string();
		let name = sp[2].trim().to_string();
		Ok(LogEvent { id, op, name })
	}
}

pub struct Win {
	id: usize,
	ctl: Fid,
	body: Fid,
}

impl Win {
	pub fn new() -> Result<Win> {
		let mut fsys = mount()?;
		let mut fid = fsys.open("new/ctl", OpenMode::RDWR)?;
		let mut buf = [0; 100];
		let sz = fid.read(&mut buf)?;
		let data = String::from_utf8(buf[0..sz].to_vec())?;
		let sp: Vec<&str> = data.split_whitespace().collect();
		if sp.len() == 0 {
			return Err(err_str("short read from acme/new/ctl".to_string()));
		}
		let id = sp[0].parse()?;
		Win::open(&mut fsys, id, fid)
	}
	// open connects to the existing window with the given id.
	pub fn open(fsys: &mut Fsys, id: usize, ctl: Fid) -> Result<Win> {
		let addr = format!("{}/body", id);
		let body = fsys.open(addr.as_str(), OpenMode::RDWR)?;
		Ok(Win { id, ctl, body })
	}

	pub fn id(&self) -> usize {
		self.id
	}
	pub fn write(&mut self, name: &str, data: String) -> Result<()> {
		let f = self.fid(name)?;
		f.write(data.as_bytes())?;
		Ok(())
	}
	fn fid(&mut self, name: &str) -> Result<&mut Fid> {
		match name {
			"ctl" => Ok(&mut self.ctl),
			"body" => Ok(&mut self.body),
			_ => Err(err_str(format!("unknown acme file: {}", name))),
		}
	}
	pub fn ctl(&mut self, data: String) -> Result<()> {
		self.write("ctl", format!("{}\n", data))
	}
	pub fn name(&mut self, name: &str) -> Result<()> {
		self.ctl(format!("name {}", name))
	}
	pub fn del(&mut self, sure: bool) -> Result<()> {
		let cmd = if sure { "delete" } else { "del" };
		self.ctl(cmd.to_string())
	}
}

#[cfg(test)]
mod tests {
	use crate::acme::*;

	#[test]
	fn windows() {
		let ws = WinInfo::windows().unwrap();
		assert_ne!(ws.len(), 0);
		println!("ws: {:?}", ws);
	}

	#[test]
	fn log() {
		let mut log = LogReader::new().unwrap();
		let ev = log.read().unwrap();
		println!("ev: {:?}", ev);
	}

	#[test]
	fn new() {
		let mut w = Win::new().unwrap();
		w.name("testing").unwrap();
		w.write("body", "blah hello".to_string()).unwrap();
		w.del(true).unwrap();
	}
}

use crate::dial;
use crate::{err_str, fid::Fid, fsys::Fsys, Result};
use nine::p2000::OpenMode;
use std::io::{BufRead, BufReader, Read};

fn mount() -> Result<Fsys> {
	dial::mount_service("acme")
}

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

#[derive(Debug)]
pub struct WinInfo {
	id: usize,
	name: String,
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

#[cfg(test)]
mod tests {
	use crate::acme;

	#[test]
	fn windows() {
		let ws = acme::windows().unwrap();
		assert_ne!(ws.len(), 0);
		println!("ws: {:?}", ws);
	}

	#[test]
	fn log() {
		let mut log = acme::LogReader::new().unwrap();
		let ev = log.read().unwrap();
		println!("ev: {:?}", ev);
	}
}

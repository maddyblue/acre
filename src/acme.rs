use crate::dial;
use crate::{err_str, fid::Fid, fsys::Fsys, Result};
use lazy_static::lazy_static;
use nine::p2000::OpenMode;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

lazy_static! {
	pub static ref FSYS: Mutex<Fsys> = Mutex::new(dial::mount_service("acme").unwrap());
}

#[derive(Debug)]
pub struct WinInfo {
	pub id: usize,
	pub name: String,
}

impl WinInfo {
	pub fn windows() -> Result<Vec<WinInfo>> {
		let index = FSYS.lock().unwrap().open("index", OpenMode::READ)?;
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
	pub id: usize,
	pub op: String,
	pub name: String,
}

impl LogReader {
	pub fn new() -> Result<LogReader> {
		let log = FSYS.lock().unwrap().open("log", OpenMode::READ)?;
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
	addr: Fid,
	data: Fid,
	tag: Fid,
}

pub enum File {
	Ctl,
	Body,
	Addr,
	Data,
	Tag,
}

pub struct WinEvents {
	event: Fid,
}

impl WinEvents {
	pub fn read_event(&mut self) -> Result<Event> {
		let mut e = self.get_event()?;

		// expansion
		if e.flag & 2 != 0 {
			let mut e2 = self.get_event()?;
			if e.q0 == e.q1 {
				e2.orig_q0 = e.q0;
				e2.orig_q1 = e.q1;
				e2.flag = e.flag;
				e = e2;
			}
		}

		// chorded argument
		if e.flag & 8 != 0 {
			let e3 = self.get_event()?;
			let e4 = self.get_event()?;
			e.arg = e3.text;
			e.loc = e4.text;
		}

		Ok(e)
	}
	fn get_ch(&mut self) -> Result<char> {
		let mut buf = [0; 1];
		// TODO: figure out how to use a BufReader here to avoid a bunch of single reads.
		self.event.read_exact(&mut buf)?;
		Ok(buf[0] as char)
	}
	fn get_en(&mut self) -> Result<u32> {
		let mut c: char;
		let mut n: u32 = 0;
		loop {
			c = self.get_ch()?;
			if c < '0' || c > '9' {
				break;
			}
			n = n * 10 + c.to_digit(10).unwrap();
		}
		if c != ' ' {
			return Err(err_str(format!("event number syntax")));
		}
		Ok(n)
	}
	fn get_event(&mut self) -> Result<Event> {
		let c1 = self.get_ch()?;
		let c2 = self.get_ch()?;
		let q0 = self.get_en()?;
		let q1 = self.get_en()?;
		let flag = self.get_en()?;
		let nr = self.get_en()? as usize;
		if nr > EVENT_SIZE {
			return Err(err_str(format!("event size too long")));
		}
		let mut text = vec![];
		while text.len() < nr {
			text.push(self.get_ch()?);
		}
		let text: String = text.into_iter().collect();
		if self.get_ch()? != '\n' {
			return Err(err_str(format!("phase error")));
		}

		Ok(Event {
			c1,
			c2,
			q0,
			q1,
			flag,
			nr: nr as u32,
			text,
			orig_q0: q0,
			orig_q1: q1,
			arg: "".to_string(),
			loc: "".to_string(),
		})
	}
	pub fn write_event(&mut self, ev: Event) -> Result<()> {
		let s = format!("{}{}{} {} \n", ev.c1, ev.c2, ev.q0, ev.q1);
		self.event.write(s.as_bytes())?;
		Ok(())
	}
}

impl Win {
	pub fn new() -> Result<Win> {
		let mut fsys = FSYS.lock().unwrap();
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
		let body = fsys.open(format!("{}/body", id).as_str(), OpenMode::RDWR)?;
		let addr = fsys.open(format!("{}/addr", id).as_str(), OpenMode::RDWR)?;
		let data = fsys.open(format!("{}/data", id).as_str(), OpenMode::RDWR)?;
		let tag = fsys.open(format!("{}/tag", id).as_str(), OpenMode::RDWR)?;
		Ok(Win {
			id,
			ctl,
			body,
			addr,
			data,
			tag,
		})
	}
	pub fn events(&mut self) -> Result<WinEvents> {
		let event = FSYS
			.lock()
			.unwrap()
			.open(format!("{}/event", self.id).as_str(), OpenMode::RDWR)?;
		Ok(WinEvents { event })
	}
	pub fn id(&self) -> usize {
		self.id
	}
	pub fn write(&mut self, file: File, data: &str) -> Result<()> {
		let f = self.fid(file);
		f.write(data.as_bytes())?;
		Ok(())
	}
	fn fid(&mut self, file: File) -> &mut Fid {
		match file {
			File::Ctl => &mut self.ctl,
			File::Body => &mut self.body,
			File::Addr => &mut self.addr,
			File::Data => &mut self.data,
			File::Tag => &mut self.tag,
		}
	}
	pub fn ctl(&mut self, data: &str) -> Result<()> {
		self.write(File::Ctl, &format!("{}\n", data))
	}
	pub fn addr(&mut self, data: &str) -> Result<()> {
		self.write(File::Addr, &format!("{}", data))
	}
	pub fn clear(&mut self) -> Result<()> {
		self.write(File::Addr, &format!(","))?;
		self.write(File::Data, &format!(""))?;
		Ok(())
	}
	pub fn name(&mut self, name: &str) -> Result<()> {
		self.ctl(&format!("name {}", name))
	}
	pub fn del(&mut self, sure: bool) -> Result<()> {
		let cmd = if sure { "delete" } else { "del" };
		self.ctl(cmd)
	}
	pub fn read_addr(&mut self) -> Result<(usize, usize)> {
		let mut buf: [u8; 40] = [0; 40];
		let f = self.fid(File::Addr);
		f.seek(SeekFrom::Start(0))?;
		let sz = f.read(&mut buf)?;
		let addr = std::str::from_utf8(&buf[0..sz])?;
		let a: Vec<&str> = addr.split_whitespace().collect();
		if a.len() < 2 {
			return Err(err_str(format!("short read from acme addr")));
		}
		Ok((a[0].parse()?, a[1].parse()?))
	}
	pub fn read(&mut self, file: File) -> Result<&mut Fid> {
		let f = self.fid(file);
		f.seek(SeekFrom::Start(0))?;
		Ok(f)
	}
	pub fn seek(&mut self, file: File, pos: SeekFrom) -> Result<u64> {
		let f = self.fid(file);
		match f.seek(pos) {
			Ok(v) => Ok(v),
			Err(err) => Err(Box::new(err)),
		}
	}
}

const EVENT_SIZE: usize = 256;

#[derive(Debug)]
pub struct Event {
	pub c1: char,
	pub c2: char,
	pub q0: u32,
	pub q1: u32,
	pub orig_q0: u32,
	pub orig_q1: u32,
	pub flag: u32,
	pub nr: u32,
	pub text: String,
	pub arg: String,
	pub loc: String,
}

impl Event {
	pub fn load_text(&mut self) {
		if self.text.len() == 0 && self.q0 < self.q1 {
			/*
			w.Addr("#%d,#%d", e.Q0, e.Q1)
			data, err := w.ReadAll("xdata")
			if err != nil {
				w.Err(err.Error())
			}
			e.Text = data
			*/
			panic!("unimplemented");
		}
	}
}

#[derive(Debug)]
pub struct NlOffsets {
	nl: Vec<u64>,
	leftover: u64,
}

impl NlOffsets {
	pub fn new<R: Read>(r: R) -> Result<NlOffsets> {
		let mut r = BufReader::new(r);
		let mut nl = vec![0];
		let mut o = 0;
		let mut line = vec![];
		let mut leftover = 0;
		loop {
			line.clear();
			let sz = r.read_until('\n' as u8, &mut line)?;
			if sz == 0 {
				break;
			}
			let n = std::str::from_utf8(&line)?.chars().count() as u64;
			let last: u8 = *line.last().unwrap();
			if last != '\n' as u8 {
				leftover = n;
				break;
			}
			o += n;
			nl.push(o);
		}
		Ok(NlOffsets { nl, leftover })
	}
	// returns line, col
	pub fn offset_to_line(&self, offset: u64) -> (u64, u64) {
		for (i, o) in self.nl.iter().enumerate() {
			if *o > offset {
				return (i as u64 - 1, offset - self.nl[i - 1]);
			}
		}
		let i = self.nl.len() - 1;
		if offset >= self.nl[i] {
			return (i as u64, offset - self.nl[i]);
		}
		panic!("unreachable");
	}
	pub fn line_to_offset(&self, line: u64, col: u64) -> u64 {
		let line = line as usize;
		let eof = self.nl[self.nl.len() - 1] + self.leftover;
		if line > self.nl.len() {
			// beyond EOF, so just return the highest offset.
			return eof;
		}
		let mut o = self.nl[line] + col;
		if o > eof {
			o = eof;
		}
		o
	}
	// returns the position of the last character in the file.
	pub fn last(&self) -> (u64, u64) {
		if self.nl.is_empty() {
			(0, self.leftover)
		} else {
			(self.nl.len() as u64 - 1, self.leftover)
		}
	}
}

#[cfg(test)]
mod tests {
	use crate::acme::*;

	#[test]
	fn nloffsets() {
		let s = "12345\n678\n90";
		let c = std::io::Cursor::new(s);
		let n = NlOffsets::new(c).unwrap();
		assert_eq!(n.offset_to_line(0), (0, 0));
		assert_eq!(n.offset_to_line(3), (0, 3));
		assert_eq!(n.offset_to_line(5), (0, 5));
		assert_eq!(n.offset_to_line(6), (1, 0));
		assert_eq!(n.offset_to_line(7), (1, 1));
		assert_eq!(n.offset_to_line(9), (1, 3));
		assert_eq!(n.offset_to_line(10), (2, 0));
		assert_eq!(n.offset_to_line(11), (2, 1));
		assert_eq!(n.last(), (2, 2));
	}

	#[test]
	fn windows() {
		let ws = WinInfo::windows().unwrap();
		assert_ne!(ws.len(), 0);
	}

	#[test]
	fn log() {
		let mut log = LogReader::new().unwrap();
		log.read().unwrap();
	}

	#[test]
	#[ignore]
	fn new() {
		let mut w = Win::new().unwrap();
		let mut wev = w.events().unwrap();
		w.name("testing").unwrap();
		w.write(File::Body, "blah hello done hello").unwrap();
		loop {
			let mut ev = wev.read_event().unwrap();
			println!("ev: {:?}", ev);
			match ev.c2 {
				'x' | 'X' => {
					let text = ev.text.trim();
					if text == "done" {
						break;
					}
					println!("cmd text: {}", ev.text);
					wev.write_event(ev).unwrap();
				}
				'l' | 'L' => {
					ev.load_text();
					println!("look: {}", ev.text);
					wev.write_event(ev).unwrap();
				}
				_ => {}
			}
		}
		w.del(true).unwrap();
	}
}

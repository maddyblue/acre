#[macro_use]
extern crate crossbeam_channel;

use crossbeam_channel::{bounded, Receiver};
use nine::p2000::OpenMode;
use plan9::acme::*;
use std::collections::HashMap;
use std::fmt::Write;
use std::thread;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

fn main() -> Result<()> {
	let mut s = Server::new()?;
	s.wait()
}

struct Server {
	w: Win,
	ws: HashMap<usize, ServerWin>,
	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	err_r: Receiver<Error>,
}

struct ServerWin {
	name: String,
	id: usize,
	w: Win,
}

impl Server {
	fn new() -> Result<Server> {
		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let mut w = Win::new()?;
		w.name("acre")?;
		let mut wev = w.events()?;
		let s = Server {
			w,
			ws: HashMap::new(),
			log_r,
			ev_r,
			err_r,
		};
		let err_s1 = err_s.clone();
		thread::Builder::new()
			.name("LogReader".to_string())
			.spawn(move || {
				let mut log = LogReader::new().unwrap();
				loop {
					match log.read() {
						Ok(ev) => match ev.op.as_str() {
							"new" | "del" => {
								println!("sending log event: {:?}", ev);
								log_s.send(ev).unwrap();
							}
							_ => {
								println!("log event: {:?}", ev);
							}
						},
						Err(err) => {
							err_s1.send(err).unwrap();
							return;
						}
					};
				}
			})
			.unwrap();
		thread::Builder::new()
			.name("WindowEvents".to_string())
			.spawn(move || loop {
				let mut ev = wev.read_event().unwrap();
				println!("window event: {:?}", ev);
				match ev.c2 {
					'x' | 'X' => match ev.text.as_str() {
						"Del" => {
							return;
						}
						"Get" => {
							ev_s.send(ev).unwrap();
						}
						_ => {
							wev.write_event(ev).unwrap();
						}
					},
					'L' => {
						ev.load_text();
						ev_s.send(ev).unwrap();
					}
					_ => {}
				}
			})
			.unwrap();
		Ok(s)
	}
	fn sync(&mut self) -> Result<()> {
		let mut body = String::new();
		for (_, win) in &self.ws {
			write!(&mut body, "{}\n\t[define] [describe]\n", win.name)?;
		}
		write!(&mut body, "-----\n\n")?;
		self.w.clear()?;
		self.w.write(File::Body, body)?;
		self.w.ctl("cleartag".to_string())?;
		self.w.write(File::Tag, " Get".to_string())?;
		Ok(())
	}
	fn sync_windows(&mut self) -> Result<()> {
		println!("sync windows");
		let mut ws = HashMap::new();
		for wi in WinInfo::windows()? {
			if !wi.name.ends_with(".go") {
				continue;
			}
			println!("found {}", wi.name);
			let w = match self.ws.remove(&wi.id) {
				Some(w) => w,
				None => {
					let mut fsys = FSYS.lock().unwrap();
					let ctl = fsys.open(format!("{}/ctl", wi.id).as_str(), OpenMode::RDWR)?;
					let w = Win::open(&mut fsys, wi.id, ctl)?;
					ServerWin {
						name: wi.name,
						id: wi.id,
						w,
					}
				}
			};
			ws.insert(wi.id, w);
		}
		self.ws = ws;
		Ok(())
	}
	fn run_cmd(&mut self, ev: Event) -> Result<()> {
		match ev.c2 {
			'x' | 'X' => match ev.text.as_str() {
				"Get" => {
					self.sync_windows()?;
				}
				_ => {
					panic!("unexpected");
				}
			},
			'L' => match ev.text.as_str() {
				"define" => {}
				_ => {}
			},
			_ => {}
		}
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		self.sync_windows()?;
		loop {
			self.sync()?;
			select! {
				recv(self.log_r) -> msg => {
					match msg {
						Ok(_) => { self.sync_windows()?;},
						Err(_) => { println!("log_r closed"); break;},
					};
				},
				recv(self.ev_r) -> msg => {
					match msg {
						Ok(ev) => { self.run_cmd(ev)?; },
						Err(_) => { println!("ev_r closed"); break;},
					};
				},
				recv(self.err_r) -> msg => {
					match msg {
						Ok(v) => { println!("err: {}", v); break;},
						Err(_) => { println!("err_r closed"); break;},
					};
				},
			}
		}
		Ok(())
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		let _ = self.w.del(true);
	}
}

#[macro_use]
extern crate crossbeam_channel;

use crossbeam_channel::{bounded, Receiver};
use plan9::acme::*;
use std::fmt::Write;
use std::thread::spawn;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

fn main() -> Result<()> {
	let mut s = Server::new()?;
	s.wait()
}

struct Server {
	w: Win,
	log_r: Receiver<LogEvent>,
	ev_r: Receiver<Event>,
	err_r: Receiver<Error>,
}

impl Server {
	fn new() -> Result<Server> {
		let (log_s, log_r) = bounded(0);
		let (ev_s, ev_r) = bounded(0);
		let (err_s, err_r) = bounded(0);
		let (w, mut wev) = Win::new()?;
		let s = Server {
			w,
			log_r,
			ev_r,
			err_r,
		};
		let err_s1 = err_s.clone();
		spawn(move || {
			let mut log = LogReader::new().unwrap();
			loop {
				match log.read() {
					Ok(ev) => match ev.op.as_str() {
						"new" | "del" => {
							log_s.send(ev).unwrap();
						}
						_ => {}
					},
					Err(err) => {
						err_s1.send(err).unwrap();
						return;
					}
				};
			}
		});
		spawn(move || loop {
			let mut ev = wev.read_event().unwrap();
			println!("window event: {:?}", ev);
			match ev.c2 {
				'x' | 'X' => {
					if ev.text == "Del" {
						return;
					}
					wev.write_event(ev).unwrap();
				}
				'l' | 'L' => {
					ev.load_text();
					println!("look: {}", ev.text);
					ev_s.send(ev).unwrap();
				}
				_ => {}
			}
		});
		Ok(s)
	}
	fn sync(&mut self) -> Result<()> {
		let mut body = String::new();
		let ws = WinInfo::windows()?;
		for win in &ws {
			if !win.name.ends_with(".go") {
				continue;
			}
			println!("{}", win.name);
			write!(&mut body, "{}\n\n", win.name)?;
		}
		println!("clearing");
		self.w.clear()?;
		println!("write to body: {}", body);
		self.w.write(File::Body, body)?;
		Ok(())
	}
	fn wait(&mut self) -> Result<()> {
		self.sync()?;
		loop {
			select! {
				recv(self.log_r) -> msg => {
					match msg {
						Ok(_) => { self.sync()?;},
						Err(_) => { println!("log_r closed"); break;},
					};
				},
				recv(self.ev_r) -> msg => {
					match msg {
						Ok(_) => {},
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
		self.w.del(true)?;

		Ok(())
	}
}

/*
Start a thread that listens for log reader changes. On window create or destroy, re-run sync.
Start a thread that listens for w events. On look, do the thing.
*/

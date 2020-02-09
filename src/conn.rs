extern crate byteorder;
extern crate nine;

use crate::{err_str, fid, fsys, Result};
use byteorder::{LittleEndian, WriteBytesExt};
use nine::{de::*, p2000::*, ser::*};
use std::cell::RefCell;
use std::fmt::Debug;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;

pub struct Conn<Stream>
where
	Stream: Write,
	for<'a> &'a mut Stream: Read,
{
	stream: Stream,
	msg_buf: Vec<u8>,
	pub msize: u32,
	nextfid: u32,
}

impl<Stream: Write + Read> Conn<Stream> {
	pub fn new(stream: Stream) -> Result<Self> {
		let mut c = Conn {
			stream,
			msg_buf: Vec::new(),
			msize: 131072,
			nextfid: 1,
		};

		let tx = Tversion {
			tag: 0,
			msize: c.msize,
			version: "9P2000".into(),
		};
		let rx = c.rpc::<Tversion, Rversion>(&tx)?;
		if rx.msize > c.msize {
			return Err(err_str(format!("invalid msize {}", rx.msize)));
		}
		c.msize = rx.msize;
		if rx.version != "9P2000" {
			return Err(err_str(format!("invalid version {}", rx.version)));
		}

		Ok(c)
	}

	fn rpc<
		'de,
		S: Serialize + MessageTypeId + Debug,
		D: Deserialize<'de> + MessageTypeId + Debug,
	>(
		&mut self,
		s: &S,
	) -> Result<D> {
		self.send_msg(s)?;
		self.read_msg::<D>()
	}

	fn send_msg<T: Serialize + MessageTypeId + Debug>(&mut self, t: &T) -> Result<()> {
		self.msg_buf.truncate(0);
		let amt = into_vec(&t, &mut self.msg_buf)?;

		assert!(self.msize >= amt);
		self.stream.write_u32::<LittleEndian>(amt + 5)?;
		self.stream.write_u8(<T as MessageTypeId>::MSG_TYPE_ID)?;
		Ok(self.stream.write_all(&self.msg_buf[0..amt as usize])?)
	}

	fn read_msg<'de, T: Deserialize<'de> + MessageTypeId + Debug>(&mut self) -> Result<T> {
		let _size: u32 = self.read_a()?;
		let mtype: u8 = self.read_a()?;
		let want = <T as MessageTypeId>::MSG_TYPE_ID;
		if mtype == want {
			return self.read_a();
		}
		if mtype == 107 {
			let rerror: Rerror = self.read_a()?;
			return Err(err_str(rerror.ename.to_string()));
		}
		Err(err_str(format!(
			"unknown type: {}, expected: {}",
			mtype, want
		)))
	}

	fn read_a<'de, T: Deserialize<'de> + Debug>(&mut self) -> Result<T> {
		match from_reader(&mut self.stream) {
			Ok(t) => Ok(t),
			Err(e) => Err(err_str(e.to_string())),
		}
	}

	pub fn newfid(&mut self) -> u32 {
		self.nextfid += 1;
		self.nextfid
	}
}

pub struct RcConn {
	pub rc: RefConn,
}

pub type RefConn = Rc<RefCell<Conn<UnixStream>>>;

impl RcConn {
	pub fn attach(&mut self, user: String, aname: String) -> Result<fsys::Fsys> {
		let mut c = self.rc.borrow_mut();
		let newfid = c.newfid();
		let attach = Tattach {
			tag: 0,
			fid: newfid,
			afid: NOFID,
			uname: user.into(),
			aname: aname.into(),
		};

		let r = c.rpc::<Tattach, Rattach>(&attach)?;

		Ok(fsys::Fsys {
			fid: fid::Fid::new(Rc::clone(&self.rc), newfid, r.qid),
		})
	}
}

const NOFID: u32 = !0;

impl<Stream: Write + Read> Conn<Stream> {
	pub fn walk(&mut self, fid: u32, newfid: u32, wname: Vec<String>) -> Result<Vec<Qid>> {
		let walk = Twalk {
			tag: 0,
			fid,
			newfid,
			wname,
		};
		let rwalk = self.rpc::<Twalk, Rwalk>(&walk)?;
		Ok(rwalk.wqid)
	}
	pub fn open(&mut self, fid: u32, mode: OpenMode) -> Result<()> {
		let open = Topen { tag: 0, fid, mode };
		self.rpc::<Topen, Ropen>(&open)?;
		Ok(())
	}
	pub fn read(&mut self, fid: u32, offset: u64, count: u32) -> Result<Vec<u8>> {
		let read = Tread {
			tag: 0,
			fid,
			offset,
			count,
		};
		let rread = self.rpc::<Tread, Rread>(&read)?;
		return Ok(rread.data);
	}
	pub fn clunk(&mut self, fid: u32) -> Result<()> {
		let clunk = Tclunk { tag: 0, fid };
		self.rpc::<Tclunk, Rclunk>(&clunk)?;
		Ok(())
	}
	/*
	fn write(&mut self, fid: u32, offset: u64, data: Vec<u8>) -> u32 {
		let twrite = Twrite {
			tag: 0,
			fid,
			offset,
			data,
		};
		self.send_msg(&twrite).unwrap();
		let rwrite: Rwrite = self.read_msg().unwrap();

		rwrite.count
	}
	*/
}

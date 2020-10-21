use std::io::Write;
use std::sync::Mutex;

use anyhow::Result;
use lazy_static::lazy_static;
use nine::p2000::OpenMode;

use crate::dial;
use crate::{fid::Fid, fsys::Fsys};

lazy_static! {
    pub static ref FSYS: Mutex<Fsys> = Mutex::new(dial::mount_service("plumb").unwrap());
}

pub fn open(name: &str, mode: OpenMode) -> Result<Fid> {
    FSYS.lock().unwrap().open(&name, mode)
}

pub struct Message {
    pub dst: String,
    pub typ: String,
    pub data: Vec<u8>,
}

impl Message {
    pub fn send(self, mut f: Fid) -> Result<()> {
        let mut s: Vec<u8> = vec![];
        write!(&mut s, "\n")?; // src
        write!(&mut s, "{}\n", self.dst)?;
        write!(&mut s, "\n")?; // dir
        write!(&mut s, "{}\n", self.typ)?;
        write!(&mut s, "\n")?; // attr
        write!(&mut s, "{}\n", self.data.len())?;
        s.extend(&self.data);
        f.write(&s)?;
        Ok(())
    }
}

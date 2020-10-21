use anyhow::Result;
use nine::p2000::OpenMode;

use crate::fid::Fid;

pub struct Fsys {
    pub fid: Fid,
}

impl Fsys {
    pub fn open(&mut self, name: &str, mode: OpenMode) -> Result<Fid> {
        let mut fid = self.fid.walk(name)?;
        fid.open(mode)?;
        Ok(fid)
    }
}

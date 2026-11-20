//! Tolerant reading of library zips written by older sanitizer builds.
//!
//! The zip crate (8.1) wrote a malformed zip64 central record for a raw-copied entry whose local
//! header lies beyond 4 GiB: the record carries the zip64 extra block TWICE, the first copy
//! `[uncompressed, compressed, offset]` correct and the second `[0, 0, offset]`. The zip crate
//! reader lets the last block win, so such entries read back with size 0 and fail the CRC check.
//! (Python's `zipfile` and 7-Zip are not affected the same way, so the data itself is intact.)
//!
//! [`Repairs`] re-parses the central directory itself, finds exactly these records and can open
//! each affected entry through a one-entry virtual zip (the original local header + data followed
//! by a corrected central directory) that the zip crate then decompresses and CRC-checks normally.
//! Nothing is written anywhere; whole-zip hardening still runs on the zip crate's view of the file.

use crate::error::NeedsReview;
use crate::harden::{normalize_name, scan_entries, EntryInfo};
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

const THR: u64 = 0xFFFF_FFFF;
const MAX_CD: u64 = 256 * 1024 * 1024;

/// What the zip crate got wrong for one entry, with the real values.
#[derive(Clone, Debug)]
struct Fix {
    /// Original central record without name/extra/comment (46 bytes).
    record: [u8; 46],
    name: Vec<u8>,
    usize_: u64,
    csize: u64,
    offset: u64,
}

/// Entries of one zip that need the repair view (empty for healthy zips).
#[derive(Clone, Debug, Default)]
pub(crate) struct Repairs {
    path: PathBuf,
    fixes: HashMap<String, Fix>,
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?))
}

/// (central directory offset, size) from the end-of-central-directory records.
fn find_cd(f: &mut File, len: u64) -> Option<(u64, u64)> {
    let tail_len = len.min(66 * 1024 + 64);
    f.seek(SeekFrom::Start(len - tail_len)).ok()?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).ok()?;
    let i = tail.windows(4).rposition(|w| w == b"PK\x05\x06")?;
    let n_total = u16_at(&tail, i + 10)? as u64;
    let mut cd_size = u32_at(&tail, i + 12)? as u64;
    let mut cd_off = u32_at(&tail, i + 16)? as u64;
    if cd_off == THR || cd_size == THR || n_total == 0xFFFF {
        let loc = i.checked_sub(20)?;
        if tail.get(loc..loc + 4)? != b"PK\x06\x07" {
            return None;
        }
        let e64 = u64_at(&tail, loc + 8)?;
        f.seek(SeekFrom::Start(e64)).ok()?;
        let mut b = [0u8; 56];
        f.read_exact(&mut b).ok()?;
        if &b[..4] != b"PK\x06\x06" {
            return None;
        }
        cd_size = u64_at(&b, 40)?;
        cd_off = u64_at(&b, 48)?;
    }
    (cd_off.checked_add(cd_size)? <= len && cd_size <= MAX_CD).then_some((cd_off, cd_size))
}

impl Repairs {
    /// Scan the central directory of `zip_path` for the duplicated-zip64 pattern. Never fails:
    /// anything unexpected just means "no repairs".
    pub(crate) fn load(zip_path: &Path) -> Repairs {
        let mut out = Repairs {
            path: zip_path.to_path_buf(),
            fixes: HashMap::new(),
        };
        let _ = out.scan();
        out
    }

    fn scan(&mut self) -> Option<()> {
        let mut f = File::open(&self.path).ok()?;
        let len = f.metadata().ok()?.len();
        let (cd_off, cd_size) = find_cd(&mut f, len)?;
        f.seek(SeekFrom::Start(cd_off)).ok()?;
        let mut cd = vec![0u8; cd_size as usize];
        f.read_exact(&mut cd).ok()?;
        let mut p = 0usize;
        while p + 46 <= cd.len() && &cd[p..p + 4] == b"PK\x01\x02" {
            let nl = u16_at(&cd, p + 28)? as usize;
            let el = u16_at(&cd, p + 30)? as usize;
            let cl = u16_at(&cd, p + 32)? as usize;
            let name = cd.get(p + 46..p + 46 + nl)?.to_vec();
            let extra = cd.get(p + 46 + nl..p + 46 + nl + el)?;
            // Walk the extra blocks: the pattern is two or more zip64 blocks.
            let mut blocks: Vec<&[u8]> = Vec::new();
            let mut q = 0usize;
            while q + 4 <= extra.len() {
                let id = u16_at(extra, q)?;
                let l = u16_at(extra, q + 2)? as usize;
                let body = extra.get(q + 4..q + 4 + l)?;
                if id == 1 {
                    blocks.push(body);
                }
                q += 4 + l;
            }
            if blocks.len() >= 2 && blocks[0].len() == 24 {
                let a = blocks[0];
                let rec_u = u32_at(&cd, p + 24)? as u64;
                let rec_c = u32_at(&cd, p + 20)? as u64;
                let rec_o = u32_at(&cd, p + 42)? as u64;
                let usize_ = if rec_u != THR { rec_u } else { u64_at(a, 0)? };
                let csize = if rec_c != THR { rec_c } else { u64_at(a, 8)? };
                let offset = if rec_o != THR { rec_o } else { u64_at(a, 16)? };
                let mut record = [0u8; 46];
                record.copy_from_slice(&cd[p..p + 46]);
                if let Ok(norm) = normalize_name(&String::from_utf8_lossy(&name)) {
                    self.fixes.insert(
                        norm,
                        Fix {
                            record,
                            name,
                            usize_,
                            csize,
                            offset,
                        },
                    );
                }
            }
            p += 46 + nl + el + cl;
        }
        Some(())
    }

    pub(crate) fn has(&self, path: &str) -> bool {
        self.fixes.contains_key(path)
    }

    /// Replace the sizes the zip crate reported with the real ones.
    pub(crate) fn apply(&self, infos: &mut [EntryInfo]) {
        if self.fixes.is_empty() {
            return;
        }
        for e in infos.iter_mut().filter(|e| !e.is_dir) {
            if let Some(f) = self.fixes.get(&e.path) {
                e.size = f.usize_;
            }
        }
    }

    /// Open one affected entry through its virtual one-entry zip and run `f` on the stream.
    pub(crate) fn with_entry<T>(
        &self,
        path: &str,
        f: impl FnOnce(&mut dyn Read) -> io::Result<T>,
    ) -> io::Result<T> {
        let fix = self
            .fixes
            .get(path)
            .ok_or_else(|| io::Error::other("entry needs no repair"))?;
        let virt = Virt::new(&self.path, fix)?;
        let mut archive = ZipArchive::new(virt).map_err(io::Error::other)?;
        let mut file = archive.by_index(0).map_err(io::Error::other)?;
        f(&mut file)
    }
}

/// Like `harden::scan_entries` plus the repair view: sizes of affected entries are corrected.
pub(crate) fn scan_with_repairs<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    zip_path: &Path,
) -> Result<(Vec<EntryInfo>, Repairs), NeedsReview> {
    let mut infos = scan_entries(zip)?;
    let rep = Repairs::load(zip_path);
    rep.apply(&mut infos);
    Ok((infos, rep))
}

/// A one-entry zip made of the original local header + data and a corrected central directory.
struct Virt {
    file: BufReader<File>,
    /// Where the next read on `file` happens (to skip redundant seeks).
    fpos: u64,
    base: u64,
    span: u64,
    tail: Vec<u8>,
    pos: u64,
}

impl Virt {
    fn new(path: &Path, fix: &Fix) -> io::Result<Virt> {
        let bad = |m: &str| io::Error::other(m.to_string());
        let mut f = File::open(path)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(fix.offset))?;
        let mut lh = [0u8; 30];
        f.read_exact(&mut lh)?;
        if &lh[..4] != b"PK\x03\x04" {
            return Err(bad("local header signature missing"));
        }
        let nl = u16::from_le_bytes([lh[26], lh[27]]) as u64;
        let el = u16::from_le_bytes([lh[28], lh[29]]) as u64;
        let span = 30 + nl + el + fix.csize;
        if fix.offset.checked_add(span).is_none_or(|e| e > len) || span >= THR {
            return Err(bad("entry runs past the end of the file or is too large"));
        }
        // Corrected central record.
        let big = fix.usize_ >= THR || fix.csize >= THR;
        let mut cdr: Vec<u8> = fix.record[..46].to_vec();
        let (c32, u32v) = if big {
            (THR as u32, THR as u32)
        } else {
            (fix.csize as u32, fix.usize_ as u32)
        };
        cdr[20..24].copy_from_slice(&c32.to_le_bytes());
        cdr[24..28].copy_from_slice(&u32v.to_le_bytes());
        let mut extra = Vec::new();
        if big {
            extra.extend_from_slice(&1u16.to_le_bytes());
            extra.extend_from_slice(&16u16.to_le_bytes());
            extra.extend_from_slice(&fix.usize_.to_le_bytes());
            extra.extend_from_slice(&fix.csize.to_le_bytes());
            let need = u16::from_le_bytes([cdr[6], cdr[7]]).max(45);
            cdr[6..8].copy_from_slice(&need.to_le_bytes());
        }
        cdr[28..30].copy_from_slice(&(fix.name.len() as u16).to_le_bytes());
        cdr[30..32].copy_from_slice(&(extra.len() as u16).to_le_bytes());
        cdr[32..34].copy_from_slice(&0u16.to_le_bytes()); // comment
        cdr[34..36].copy_from_slice(&0u16.to_le_bytes()); // disk
        cdr[42..46].copy_from_slice(&0u32.to_le_bytes()); // offset
        let mut tail = cdr;
        tail.extend_from_slice(&fix.name);
        tail.extend_from_slice(&extra);
        let cd_len = tail.len() as u32;
        tail.extend_from_slice(b"PK\x05\x06");
        tail.extend_from_slice(&[0, 0, 0, 0]); // disk numbers
        tail.extend_from_slice(&1u16.to_le_bytes());
        tail.extend_from_slice(&1u16.to_le_bytes());
        tail.extend_from_slice(&cd_len.to_le_bytes());
        tail.extend_from_slice(&(span as u32).to_le_bytes());
        tail.extend_from_slice(&0u16.to_le_bytes());
        Ok(Virt {
            file: BufReader::with_capacity(1 << 20, File::open(path)?),
            fpos: u64::MAX,
            base: fix.offset,
            span,
            tail,
            pos: 0,
        })
    }

    fn total(&self) -> u64 {
        self.span + self.tail.len() as u64
    }
}

impl Read for Virt {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.total() || buf.is_empty() {
            return Ok(0);
        }
        if self.pos < self.span {
            let want = ((self.span - self.pos) as usize).min(buf.len());
            let abs = self.base + self.pos;
            if self.fpos != abs {
                self.file.seek(SeekFrom::Start(abs))?;
            }
            let n = self.file.read(&mut buf[..want])?;
            self.fpos = abs + n as u64;
            self.pos += n as u64;
            Ok(n)
        } else {
            let t = (self.pos - self.span) as usize;
            let n = (self.tail.len() - t).min(buf.len());
            buf[..n].copy_from_slice(&self.tail[t..t + n]);
            self.pos += n as u64;
            Ok(n)
        }
    }
}

impl Seek for Virt {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
            SeekFrom::End(d) => self.total().checked_add_signed(d),
        };
        match new {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start",
            )),
        }
    }
}

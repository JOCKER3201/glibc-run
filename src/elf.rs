//! Minimal ELF64 reader: extracts PT_INTERP, DT_NEEDED and the type (PIE/EXEC).
//! Just enough to recognize a glibc binary and hand it to the loader.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub struct ElfInfo {
    /// Interpreter path from PT_INTERP, e.g. "/lib64/ld-linux-x86-64.so.2".
    pub interp: Option<String>,
    /// Libraries from DT_NEEDED (for diagnostics and filling in the prefix).
    pub needed: Vec<String>,
    /// true for ET_DYN (PIE), false for ET_EXEC.
    pub is_pie: bool,
}

fn err(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn vaddr_to_off(loads: &[(u64, u64, u64)], vaddr: u64) -> Option<u64> {
    for &(v, off, sz) in loads {
        if vaddr >= v && vaddr < v + sz {
            return Some(off + (vaddr - v));
        }
    }
    None
}

fn read_cstr_at(f: &mut File, off: u64) -> Option<String> {
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    for _ in 0..4096 {
        if f.read_exact(&mut byte).is_err() {
            break;
        }
        if byte[0] == 0 {
            break;
        }
        out.push(byte[0]);
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub fn read_elf(path: &Path) -> io::Result<ElfInfo> {
    let mut f = File::open(path)?;
    let mut hdr = [0u8; 64];
    f.read_exact(&mut hdr)?;
    if &hdr[0..4] != b"\x7fELF" {
        return Err(err("not an ELF file"));
    }
    if hdr[4] != 2 {
        return Err(err("only ELF64 is supported"));
    }
    if hdr[5] != 1 {
        return Err(err("only little-endian is supported"));
    }

    let e_type = u16::from_le_bytes([hdr[16], hdr[17]]);
    let e_phoff = u64::from_le_bytes(hdr[32..40].try_into().unwrap());
    let e_phentsize = u16::from_le_bytes([hdr[54], hdr[55]]) as u64;
    let e_phnum = u16::from_le_bytes([hdr[56], hdr[57]]) as u64;

    let mut interp = None;
    let mut dyn_off = None;
    let mut dyn_size = 0u64;
    let mut loads: Vec<(u64, u64, u64)> = Vec::new();

    for i in 0..e_phnum {
        f.seek(SeekFrom::Start(e_phoff + i * e_phentsize))?;
        let mut ph = [0u8; 56];
        f.read_exact(&mut ph)?;
        let p_type = u32::from_le_bytes(ph[0..4].try_into().unwrap());
        let p_offset = u64::from_le_bytes(ph[8..16].try_into().unwrap());
        let p_vaddr = u64::from_le_bytes(ph[16..24].try_into().unwrap());
        let p_filesz = u64::from_le_bytes(ph[32..40].try_into().unwrap());
        match p_type {
            3 => {
                // PT_INTERP
                f.seek(SeekFrom::Start(p_offset))?;
                let mut b = vec![0u8; p_filesz as usize];
                f.read_exact(&mut b)?;
                if let Some(z) = b.iter().position(|&c| c == 0) {
                    b.truncate(z);
                }
                interp = Some(String::from_utf8_lossy(&b).into_owned());
            }
            2 => {
                // PT_DYNAMIC
                dyn_off = Some(p_offset);
                dyn_size = p_filesz;
            }
            1 => loads.push((p_vaddr, p_offset, p_filesz)), // PT_LOAD
            _ => {}
        }
    }

    let mut needed = Vec::new();
    if let Some(doff) = dyn_off {
        f.seek(SeekFrom::Start(doff))?;
        let mut dbuf = vec![0u8; dyn_size as usize];
        f.read_exact(&mut dbuf)?;
        let mut strtab_vaddr = None;
        let mut needed_offsets = Vec::new();
        let n = dbuf.len() / 16;
        for i in 0..n {
            let tag = i64::from_le_bytes(dbuf[i * 16..i * 16 + 8].try_into().unwrap());
            let val = u64::from_le_bytes(dbuf[i * 16 + 8..i * 16 + 16].try_into().unwrap());
            match tag {
                1 => needed_offsets.push(val), // DT_NEEDED
                5 => strtab_vaddr = Some(val),  // DT_STRTAB
                0 => break,                     // DT_NULL
                _ => {}
            }
        }
        if let Some(sv) = strtab_vaddr {
            if let Some(stroff) = vaddr_to_off(&loads, sv) {
                for no in needed_offsets {
                    if let Some(s) = read_cstr_at(&mut f, stroff + no) {
                        needed.push(s);
                    }
                }
            }
        }
    }

    Ok(ElfInfo {
        interp,
        needed,
        is_pie: e_type == 3,
    })
}

//! EFICOMMAND v0.5 - single-file CLI shell for UEFI.
//!
//! Commands: dir cd del md rd copy xcopy move ren mount disk-list diskmgr echo type find
//!           start edit kbl help cls reboot shutdown exit
//! Operators: | < > >> ; && &      Wildcards: * ?
//! Scripts: name.bat runs a batch file (one command per line, :: or REM = comment).
//! Binaries: .com (x86-64 flat, ComApi at RCX) / .efi, bare names work too.
//! Keyboard: xkb-style .kbl files (key <AE02> { [ 2, quotedbl ] };). \EFICOMMAND.kbl
//!           on the boot drive auto-loads. 'kbl test' shows what each key really sends.
//! Note: UEFI never reports scancodes for printable keys - the firmware translates
//! with its own (usually US) layout. KBL inverts that US table, so layouts work.
// Made by @Syed930s
#![no_main]
#![no_std]

extern crate alloc;
use core::fmt::Display;
use alloc::{boxed::Box, format, string::{String, ToString}, vec, vec::Vec};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicPtr, Ordering};

use uefi::boot::{
    self, AllocateType, LoadImageSource, MemoryType, OpenProtocolAttributes, OpenProtocolParams,
    ScopedProtocol, SearchType,
};
use uefi::fs::{FileSystem, PathBuf};
use uefi::prelude::*;
use uefi::proto::console::text::{Color, Key, ScanCode};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::runtime::ResetType;
use uefi::{print, println, CString16};

macro_rules! w {
    ($w:expr, $($a:tt)*) => {{ let _ = write!($w, $($a)*); }};
}
macro_rules! wl {
    ($w:expr) => {{ let _ = writeln!($w); }};
    ($w:expr, $($a:tt)*) => {{ let _ = writeln!($w, $($a)*); }};
}

fn es<E: Display>(e: E) -> String {
    format!("{e}")
}

fn letter(i: usize) -> char {
    (b'A' + i as u8) as char
}

fn human(b: u64) -> String {
    if b >= 1 << 30 {
        format!("{}.{:02} GiB", b >> 30, ((b & ((1 << 30) - 1)) * 100) >> 30)
    } else if b >= 1 << 20 {
        format!("{} MiB", b >> 20)
    } else if b >= 1 << 10 {
        format!("{} KiB", b >> 10)
    } else {
        format!("{b} B")
    }
}

fn pb(s: &str) -> Result<PathBuf, String> {
    let c = CString16::try_from(s).map_err(|_| "invalid path".to_string())?;
    Ok(PathBuf::from(&*c))
}

fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn base(p: &str) -> &str {
    p.trim_end_matches('\\').rsplit('\\').next().unwrap_or("")
}

fn joinp(dir: &str, name: &str) -> String {
    format!("{}\\{}", dir.trim_end_matches('\\'), name)
}

fn print_bytes(b: &[u8]) {
    let s = String::from_utf8_lossy(b);
    let mut buf = String::new();
    for c in s.chars() {
        match c {
            '\r' => {}
            '\n' | '\t' => buf.push(c),
            c if (c as u32) < 0x20 || (c as u32) > 0xFFFF => buf.push('.'),
            c => buf.push(c),
        }
        if buf.len() > 512 {
            print!("{}", buf);
            buf.clear();
        }
    }
    print!("{}", buf);
}

fn getkey() -> Key {
    loop {
        if let Some(ev) = system::with_stdin(|i| i.wait_for_key_event()) {
            let mut evs = [ev];
            let _ = boot::wait_for_event(&mut evs);
        }
        if let Ok(Some(k)) = system::with_stdin(|i| i.read_key()) {
            return k;
        }
    }
}

// ---------------------------------------------------------------- keyboard layouts (.kbl)

// key position ids (xkb-style names)
const KEY_NAMES: &[(&str, u16)] = &[
    ("TLDE", 0),
    ("AE01", 1), ("AE02", 2), ("AE03", 3), ("AE04", 4), ("AE05", 5), ("AE06", 6),
    ("AE07", 7), ("AE08", 8), ("AE09", 9), ("AE10", 10), ("AE11", 11), ("AE12", 12),
    ("AD01", 13), ("AD02", 14), ("AD03", 15), ("AD04", 16), ("AD05", 17), ("AD06", 18),
    ("AD07", 19), ("AD08", 20), ("AD09", 21), ("AD10", 22), ("AD11", 23), ("AD12", 24),
    ("AC01", 25), ("AC02", 26), ("AC03", 27), ("AC04", 28), ("AC05", 29), ("AC06", 30),
    ("AC07", 31), ("AC08", 32), ("AC09", 33), ("AC10", 34), ("AC11", 35), ("AC12", 36),
    ("AB01", 37), ("AB02", 38), ("AB03", 39), ("AB04", 40), ("AB05", 41), ("AB06", 42),
    ("AB07", 43), ("AB08", 44), ("AB09", 45), ("AB10", 46),
    ("LSGT", 47), ("BKSL", 48),
];

// US layout: every char the firmware can deliver, mapped to (key id, level)
// level 0 = unshifted, 1 = shifted. This is how we recover the physical key.
const US_MAP: &[(char, u16, u8)] = &[
    ('`', 0, 0), ('~', 0, 1),
    ('1', 1, 0), ('!', 1, 1),
    ('2', 2, 0), ('@', 2, 1),
    ('3', 3, 0), ('#', 3, 1),
    ('4', 4, 0), ('$', 4, 1),
    ('5', 5, 0), ('%', 5, 1),
    ('6', 6, 0), ('^', 6, 1),
    ('7', 7, 0), ('&', 7, 1),
    ('8', 8, 0), ('*', 8, 1),
    ('9', 9, 0), ('(', 9, 1),
    ('0', 10, 0), (')', 10, 1),
    ('-', 11, 0), ('_', 11, 1),
    ('=', 12, 0), ('+', 12, 1),
    ('q', 13, 0), ('Q', 13, 1), ('w', 14, 0), ('W', 14, 1),
    ('e', 15, 0), ('E', 15, 1), ('r', 16, 0), ('R', 16, 1),
    ('t', 17, 0), ('T', 17, 1), ('y', 18, 0), ('Y', 18, 1),
    ('u', 19, 0), ('U', 19, 1), ('i', 20, 0), ('I', 20, 1),
    ('o', 21, 0), ('O', 21, 1), ('p', 22, 0), ('P', 22, 1),
    ('[', 23, 0), ('{', 23, 1), (']', 24, 0), ('}', 24, 1),
    ('a', 25, 0), ('A', 25, 1), ('s', 26, 0), ('S', 26, 1),
    ('d', 27, 0), ('D', 27, 1), ('f', 28, 0), ('F', 28, 1),
    ('g', 29, 0), ('G', 29, 1), ('h', 30, 0), ('H', 30, 1),
    ('j', 31, 0), ('J', 31, 1), ('k', 32, 0), ('K', 32, 1),
    ('l', 33, 0), ('L', 33, 1),
    (';', 34, 0), (':', 34, 1), ('\'', 35, 0), ('"', 35, 1),
    ('z', 37, 0), ('Z', 37, 1), ('x', 38, 0), ('X', 38, 1),
    ('c', 39, 0), ('C', 39, 1), ('v', 40, 0), ('V', 40, 1),
    ('b', 41, 0), ('B', 41, 1), ('n', 42, 0), ('N', 42, 1),
    ('m', 43, 0), ('M', 43, 1),
    (',', 44, 0), ('<', 44, 1), ('.', 45, 0), ('>', 45, 1),
    ('/', 46, 0), ('?', 46, 1),
    ('\\', 48, 0), ('|', 48, 1),
];

const KEYSYMS: &[(&str, char)] = &[
    ("grave", '`'), ("asciitilde", '~'), ("notsign", '\u{AC}'),
    ("quotedbl", '"'), ("sterling", '\u{A3}'), ("EuroSign", '\u{20AC}'),
    ("dollar", '$'), ("numbersign", '#'), ("asciicircum", '^'),
    ("ampersand", '&'), ("asterisk", '*'), ("parenleft", '('), ("parenright", ')'),
    ("minus", '-'), ("underscore", '_'), ("equal", '='), ("plus", '+'),
    ("bracketleft", '['), ("bracketright", ']'), ("braceleft", '{'), ("braceright", '}'),
    ("backslash", '\\'), ("bar", '|'), ("brokenbar", '\u{A6}'),
    ("apostrophe", '\''), ("at", '@'), ("semicolon", ';'), ("colon", ':'),
    ("comma", ','), ("less", '<'), ("period", '.'), ("greater", '>'),
    ("slash", '/'), ("question", '?'), ("exclam", '!'), ("space", ' '),
    ("percent", '%'), ("dead_circumflex", '^'), ("dead_grave", '`'),
    ("dead_tilde", '~'), ("dead_acute", '\u{B4}'), ("dead_caron", '\u{2C7}'),
    ("dead_breve", '\u{2D8}'), ("dead_diaeresis", '\u{A8}'), ("dead_abovering", '\u{B0}'),
    ("twosuperior", '\u{B2}'), ("threesuperior", '\u{B3}'),
    ("oneeighth", '\u{215B}'), ("onequarter", '\u{BC}'),
    ("ssharp", '\u{DF}'), ("section", '\u{A7}'), ("degree", '\u{B0}'), ("mu", '\u{B5}'),
    ("udiaeresis", '\u{FC}'), ("Udiaeresis", '\u{DC}'),
    ("adiaeresis", '\u{E4}'), ("Adiaeresis", '\u{C4}'),
    ("odiaeresis", '\u{F6}'), ("Odiaeresis", '\u{D6}'),
];

struct Kbl {
    name: String,
    keys: Vec<(u16, [Option<char>; 4])>,
    cmap: Vec<(char, char)>,
    scan: Vec<(u16, char)>,
}

static mut CUR_KBL: Option<Box<Kbl>> = None;

fn cur_kbl() -> Option<&'static Kbl> {
    unsafe { (&*core::ptr::addr_of!(CUR_KBL)).as_deref() }
}

// what a delivered char becomes under the current layout
fn kbl_translate(c: char) -> char {
    if let Some(k) = cur_kbl() {
        if let Some(&(_, t)) = k.cmap.iter().find(|(f, _)| *f == c) {
            return t;
        }
        if let Some(&(_, kid, lvl)) = US_MAP.iter().find(|(ch, _, _)| *ch == c) {
            if let Some(&(_, levels)) = k.keys.iter().find(|(id, _)| *id == kid) {
                if let Some(t) = levels[lvl as usize] {
                    return t;
                }
            }
        }
    }
    c
}

fn kbl_lookup(sc: u16) -> Option<char> {
    cur_kbl().and_then(|k| k.scan.iter().find(|(s, _)| *s == sc).map(|&(_, c)| c))
}

fn cur_kbl_name() -> String {
    cur_kbl().map(|k| k.name.clone()).unwrap_or_else(|| "English (US) passthrough".to_string())
}

fn key_name(sc: u16) -> String {
    match sc {
        0x01 => "UP".into(),
        0x02 => "DOWN".into(),
        0x03 => "RIGHT".into(),
        0x04 => "LEFT".into(),
        0x05 => "HOME".into(),
        0x06 => "END".into(),
        0x07 => "INSERT".into(),
        0x08 => "DELETE".into(),
        0x09 => "PGUP".into(),
        0x0A => "PGDN".into(),
        0x17 => "ESC".into(),
        0x0B..=0x16 => format!("F{}", sc - 0x0A),
        _ => format!("0x{sc:02X}"),
    }
}

fn parse_key(s: &str) -> Result<u16, String> {
    let u = s.trim().to_ascii_uppercase();
    if let Some(n) = u.strip_prefix('F') {
        let n: u16 = n.parse().map_err(|_| format!("bad key '{s}'"))?;
        if (1..=12).contains(&n) {
            return Ok(0x0A + n);
        }
        return Err(format!("bad key '{s}'"));
    }
    if let Some(h) = u.strip_prefix("0X") {
        return u16::from_str_radix(h, 16).map_err(|_| format!("bad key '{s}'"));
    }
    match u.as_str() {
        "UP" => Ok(0x01),
        "DOWN" => Ok(0x02),
        "RIGHT" => Ok(0x03),
        "LEFT" => Ok(0x04),
        "HOME" => Ok(0x05),
        "END" => Ok(0x06),
        "INSERT" | "INS" => Ok(0x07),
        "DELETE" | "DEL" => Ok(0x08),
        "PGUP" | "PAGE_UP" => Ok(0x09),
        "PGDN" | "PAGE_DOWN" => Ok(0x0A),
        "ESC" | "ESCAPE" => Ok(0x17),
        _ => Err(format!("bad key '{s}'")),
    }
}

fn kbl_char(s: &str) -> Result<char, String> {
    let mut it = s.chars();
    let c = it.next().ok_or("missing character after =")?;
    match c {
        '\\' => match it.next() {
            Some('n') => Ok('\n'),
            Some('t') => Ok('\t'),
            Some('r') => Ok('\r'),
            Some('0') => Ok('\0'),
            Some('\\') => Ok('\\'),
            Some('\'') => Ok('\''),
            Some('"') => Ok('"'),
            _ => Err("bad escape (use \\\\ for backslash)".into()),
        },
        x => Ok(x),
    }
}

fn keysym(tok: &str) -> Option<char> {
    let t = tok.trim();
    if t.len() >= 3 && t.starts_with('\'') && t.ends_with('\'') {
        return kbl_char(&t[1..t.len() - 1]).ok();
    }
    if let Some(hex) = t.strip_prefix("U+").or_else(|| t.strip_prefix("u+")) {
        return u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
    }
    if t.chars().count() == 1 {
        return t.chars().next();
    }
    KEYSYMS.iter().find(|(n, _)| n.eq_ignore_ascii_case(t)).map(|&(_, c)| c)
}

fn read_line() -> String {
    let mut s = String::new();
    loop {
        match getkey() {
            Key::Printable(c) => {
                let ch = char::from(c);
                match ch {
                    '\r' | '\n' => {
                        println!();
                        return s;
                    }
                    '\u{8}' => {
                        if s.pop().is_some() {
                            print!("\u{8} \u{8}");
                        }
                    }
                    c if (c as u32) >= 0x20 => {
                        let c = kbl_translate(c);
                        s.push(c);
                        print!("{c}");
                    }
                    _ => {}
                }
            }
            Key::Special(sc) => {
                if let Some(ch) = kbl_lookup(sc.0) {
                    s.push(ch);
                    print!("{ch}");
                }
            }
        }
    }
}

fn cls() {
    let _ = system::with_stdout(|o| o.clear());
}

fn hexdump<W: Write>(w: &mut W, base_off: u64, data: &[u8]) {
    for (i, row) in data.chunks(16).enumerate() {
        w!(w, "{:08X}  ", base_off + (i * 16) as u64);
        for j in 0..16 {
            if j < row.len() {
                w!(w, "{:02X} ", row[j]);
            } else {
                w!(w, "   ");
            }
            if j == 7 {
                w!(w, " ");
            }
        }
        w!(w, " |");
        for &b in row {
            w!(w, "{}", if (0x20..0x7f).contains(&b) { b as char } else { '.' });
        }
        wl!(w, "|");
    }
}

// ---------------------------------------------------------------- I/O plumbing

struct Io {
    input: Vec<u8>,
    has_input: bool,
    pos: usize,
    out: Vec<u8>,
    direct: bool,
}

impl Io {
    fn new(direct: bool) -> Self {
        Io { input: Vec::new(), has_input: false, pos: 0, out: Vec::new(), direct }
    }
    fn put(&mut self, b: &[u8]) {
        if self.direct {
            print_bytes(b);
        } else {
            self.out.extend_from_slice(b);
        }
    }
    fn take_input(&mut self) -> Vec<u8> {
        let d = self.input[self.pos..].to_vec();
        self.pos = self.input.len();
        d
    }
}

impl fmt::Write for Io {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.put(s.as_bytes());
        Ok(())
    }
}

struct Con;
impl fmt::Write for Con {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        print_bytes(s.as_bytes());
        Ok(())
    }
}

enum Tok {
    Word(String),
    Pipe,
    In,
    Out,
    App,
    Semi,
    And,
    Amp,
}

#[derive(Clone, Copy, PartialEq)]
enum Join {
    Seq,
    And,
    Amp,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let mut v = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut q = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if q {
            if c == '"' {
                q = false;
            } else {
                cur.push(c);
            }
            continue;
        }
        match c {
            '"' => {
                q = true;
                has = true;
            }
            c if c.is_whitespace() => {
                if has {
                    v.push(Tok::Word(core::mem::take(&mut cur)));
                    has = false;
                }
            }
            ';' => {
                if has {
                    v.push(Tok::Word(core::mem::take(&mut cur)));
                    has = false;
                }
                v.push(Tok::Semi);
            }
            '&' => {
                if has {
                    v.push(Tok::Word(core::mem::take(&mut cur)));
                    has = false;
                }
                if it.peek() == Some(&'&') {
                    it.next();
                    v.push(Tok::And);
                } else {
                    v.push(Tok::Amp);
                }
            }
            '|' | '<' | '>' => {
                if has {
                    v.push(Tok::Word(core::mem::take(&mut cur)));
                    has = false;
                }
                v.push(match c {
                    '|' => Tok::Pipe,
                    '<' => Tok::In,
                    _ => {
                        if it.peek() == Some(&'>') {
                            it.next();
                            Tok::App
                        } else {
                            Tok::Out
                        }
                    }
                });
            }
            _ => {
                cur.push(c);
                has = true;
            }
        }
    }
    if q {
        return Err("unterminated quote".to_string());
    }
    if has {
        v.push(Tok::Word(cur));
    }
    Ok(v)
}

#[derive(Default)]
struct Stage {
    args: Vec<String>,
    infile: Option<String>,
    outfile: Option<(String, bool)>,
}

fn next_name(it: &mut alloc::vec::IntoIter<Tok>, op: &str) -> Result<String, String> {
    match it.next() {
        Some(Tok::Word(w)) => Ok(w),
        _ => Err(format!("expected a file name after {op}")),
    }
}

fn parse(toks: Vec<Tok>) -> Result<Vec<(Join, Vec<Stage>)>, String> {
    let mut groups: Vec<(Join, Vec<Stage>)> = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    let mut st = Stage::default();
    let mut conn = Join::Seq;
    let mut it = toks.into_iter();
    while let Some(t) = it.next() {
        match t {
            Tok::Word(w) => st.args.push(w),
            Tok::Pipe => {
                if st.args.is_empty() {
                    return Err("syntax error near |".to_string());
                }
                stages.push(core::mem::take(&mut st));
            }
            Tok::In => st.infile = Some(next_name(&mut it, "<")?),
            Tok::Out => st.outfile = Some((next_name(&mut it, ">")?, false)),
            Tok::App => st.outfile = Some((next_name(&mut it, ">>")?, true)),
            Tok::Semi | Tok::And | Tok::Amp => {
                if !st.args.is_empty() {
                    stages.push(core::mem::take(&mut st));
                }
                if !stages.is_empty() {
                    groups.push((conn, core::mem::take(&mut stages)));
                }
                conn = match t {
                    Tok::Semi => Join::Seq,
                    Tok::And => Join::And,
                    _ => Join::Amp,
                };
            }
        }
    }
    if !st.args.is_empty() {
        stages.push(st);
    }
    if !stages.is_empty() {
        groups.push((conn, stages));
    }
    Ok(groups)
}

// ---------------------------------------------------------------- block devices

const GPT_BDP: [u8; 16] = [0xA2, 0xA0, 0xD0, 0xEB, 0xE5, 0xB9, 0x33, 0x44, 0x87, 0xC0, 0x68, 0xB6, 0xB7, 0x26, 0x99, 0xC7];

fn block_handles() -> Vec<Handle> {
    boot::locate_handle_buffer(SearchType::from_proto::<BlockIO>())
    .map(|b| b.to_vec())
    .unwrap_or_default()
}

fn open_bio(h: Handle) -> Result<ScopedProtocol<BlockIO>, String> {
    unsafe {
        boot::open_protocol::<BlockIO>(
            OpenProtocolParams { handle: h, agent: boot::image_handle(), controller: None },
                                       OpenProtocolAttributes::GetProtocol,
        )
    }
    .map_err(es)
}

fn disk_list<W: Write>(w: &mut W) {
    let hs = block_handles();
    if hs.is_empty() {
        wl!(w, "no block devices");
        return;
    }
    wl!(w, "{:<4} {:<6} {:>12} {:>6} {:>14}  flags", "#", "type", "size", "bs", "last lba");
    for (i, h) in hs.iter().enumerate() {
        match open_bio(*h) {
            Ok(b) => {
                let m = b.media();
                let size = (m.last_block() + 1) * m.block_size() as u64;
                let mut fl = String::new();
                if m.is_removable_media() { fl.push_str("removable "); }
                if m.is_read_only() { fl.push_str("ro "); }
                if !m.is_media_present() { fl.push_str("no-media "); }
                wl!(
                    w,
                    "{:<4} {:<6} {:>12} {:>6} {:>14}  {}",
                    i,
                    if m.is_logical_partition() { "part" } else { "disk" },
                        human(size),
                    m.block_size(),
                    m.last_block(),
                    fl
                );
            }
            Err(e) => wl!(w, "{:<4} error: {}", i, e),
        }
    }
}

fn guid_name(g: &[u8]) -> &'static str {
    const ESP: [u8; 16] = [0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B];
    const LNX: [u8; 16] = [0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4];
    if g == ESP { "ESP" } else if g == GPT_BDP { "MS-Basic" } else if g == LNX { "Linux" } else { "other" }
}

fn rd_blocks(b: &mut ScopedProtocol<BlockIO>, lba: u64, count: u64) -> Result<Vec<u8>, String> {
    let bs = b.media().block_size() as u64;
    let id = b.media().media_id();
    let mut buf = vec![0u8; (bs * count) as usize];
    b.read_blocks(id, lba, &mut buf).map_err(es)?;
    Ok(buf)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn gpt_hdr(b: &mut ScopedProtocol<BlockIO>) -> Result<Vec<u8>, String> {
    let hdr = rd_blocks(b, 1, 1)?;
    if &hdr[0..8] != b"EFI PART" {
        return Err("no GPT header at LBA 1".to_string());
    }
    Ok(hdr)
}

fn gpt_dump<W: Write>(w: &mut W, b: &mut ScopedProtocol<BlockIO>) -> Result<(), String> {
    let bs = b.media().block_size() as u64;
    let hdr = gpt_hdr(b)?;
    let elba = le64(&hdr[72..]);
    let n = le32(&hdr[80..]) as u64;
    let esz = le32(&hdr[84..]) as u64;
    if esz < 128 || n == 0 || n > 4096 {
        return Err("bad GPT header".to_string());
    }
    let blocks = (n * esz + bs - 1) / bs;
    let ents = rd_blocks(b, elba, blocks)?;
    wl!(w, "{:<4} {:>12} {:>12} {:>10} {:<9} name", "#", "first", "last", "size", "type");
    for i in 0..n as usize {
        let e = &ents[i * esz as usize..(i + 1) * esz as usize];
        if e[0..16].iter().all(|&x| x == 0) {
            continue;
        }
        let first = le64(&e[32..]);
        let last = le64(&e[40..]);
        let units: Vec<u16> = e[56..128]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
        wl!(
            w,
            "{:<4} {:>12} {:>12} {:>10} {:<9} {}",
            i,
            first,
            last,
            human((last - first + 1) * bs),
            guid_name(&e[0..16]),
            String::from_utf16_lossy(&units)
        );
    }
    Ok(())
}

// apply f to the GPT entry array, then fix up both primary and backup GPTs
fn gpt_commit(b: &mut ScopedProtocol<BlockIO>, f: impl FnOnce(&mut [u8]) -> Result<(), String>) -> Result<(), String> {
    let bs = b.media().block_size() as u64;
    let id = b.media().media_id();
    let last_lba = b.media().last_block();
    if b.media().is_read_only() {
        return Err("media is read-only".to_string());
    }
    let mut hdr = gpt_hdr(b)?;
    let hsize = le32(&hdr[12..]) as usize;
    if hsize < 92 || hsize as u64 > bs {
        return Err("bad GPT header size".to_string());
    }
    let elba = le64(&hdr[72..]);
    let n = le32(&hdr[80..]) as usize;
    let esz = le32(&hdr[84..]) as usize;
    if esz < 128 || n == 0 || n > 4096 {
        return Err("bad GPT header".to_string());
    }
    let eblocks = ((n * esz) as u64 + bs - 1) / bs;
    let mut ents = rd_blocks(b, elba, eblocks)?;
    f(&mut ents[..n * esz])?;
    let ecrc = crc32(&ents[..n * esz]);
    hdr[88..92].copy_from_slice(&ecrc.to_le_bytes());
    hdr[16..20].fill(0);
    let hcrc = crc32(&hdr[..hsize]);
    hdr[16..20].copy_from_slice(&hcrc.to_le_bytes());
    b.write_blocks(id, elba, &ents).map_err(es)?;
    b.write_blocks(id, 1, &hdr).map_err(es)?;
    let mut bh = rd_blocks(b, last_lba, 1)?;
    if &bh[0..8] == b"EFI PART" {
        let belba = le64(&bh[32..]);
        bh[88..92].copy_from_slice(&ecrc.to_le_bytes());
        bh[16..20].fill(0);
        let bhcrc = crc32(&bh[..hsize]);
        bh[16..20].copy_from_slice(&bhcrc.to_le_bytes());
        b.write_blocks(id, belba, &ents).map_err(es)?;
        b.write_blocks(id, last_lba, &bh).map_err(es)?;
    }
    b.flush_blocks().map_err(es)
}

fn gpt_del(b: &mut ScopedProtocol<BlockIO>, idx: usize) -> Result<(), String> {
    let hdr = gpt_hdr(b)?;
    let n = le32(&hdr[80..]) as usize;
    let esz = le32(&hdr[84..]) as usize;
    if idx >= n {
        return Err("no such partition entry".to_string());
    }
    gpt_commit(b, |ents| {
        let e = &mut ents[idx * esz..(idx + 1) * esz];
        if e[0..16].iter().all(|&x| x == 0) {
            return Err("partition entry is already empty".to_string());
        }
        e[0..16].fill(0);
        Ok(())
    })
}

fn gpt_new(b: &mut ScopedProtocol<BlockIO>, size_blocks: u64, name: &str) -> Result<usize, String> {
    let bs = b.media().block_size() as u64;
    let id = b.media().media_id();
    let last_lba = b.media().last_block();
    let hdr = gpt_hdr(b)?;
    let n = le32(&hdr[80..]) as usize;
    let esz = le32(&hdr[84..]) as usize;
    if esz < 128 || n == 0 || n > 4096 {
        return Err("bad GPT header".to_string());
    }
    let elba = le64(&hdr[72..]);
    let eblocks = ((n * esz) as u64 + bs - 1) / bs;
    let ents = rd_blocks(b, elba, eblocks)?;
    let mut free = None;
    let mut maxl = 0u64;
    for i in 0..n {
        let e = &ents[i * esz..(i + 1) * esz];
        if e[0..16].iter().all(|&x| x == 0) {
            if free.is_none() {
                free = Some(i);
            }
        } else {
            maxl = maxl.max(le64(&e[40..]));
        }
    }
    let idx = free.ok_or("no free partition entries")?;
    let align = if bs >= 1_048_576 { 1 } else { 1_048_576 / bs };
    let first = if maxl == 0 { align } else { ((maxl + 1 + align - 1) / align) * align };
    let last = first + size_blocks - 1;
    if last > last_lba.saturating_sub(34) {
        return Err(format!("not enough space: last block would be {last}, disk ends at {last_lba}"));
    }
    let mut g = [0u8; 16];
    let t = uefi::runtime::get_time()
    .map(|t| (t.second() as u64) << 40 ^ (t.nanosecond() as u64) << 8 | t.day() as u64)
    .unwrap_or(0x1234_5678_9ABC_DEF0);
    let mut x = t ^ (id as u64) << 17 | 0x9E37_79B9_7F4A_7C15;
    for c in g.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        c.copy_from_slice(&x.to_le_bytes());
    }
    g[7] = (g[7] & 0x0F) | 0x40;
    g[8] = (g[8] & 0x3F) | 0x80;
    gpt_commit(b, |ents| {
        let e = &mut ents[idx * esz..(idx + 1) * esz];
        e[0..16].copy_from_slice(&GPT_BDP);
        e[16..32].copy_from_slice(&g);
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        e[48..56].fill(0);
        e[56..128].fill(0);
        for (j, ch) in name.chars().take(36).enumerate() {
            e[56 + j * 2..56 + j * 2 + 2].copy_from_slice(&(ch as u16).to_le_bytes());
        }
        Ok(())
    })?;
    Ok(idx)
}

fn diskmgr() {
    println!("diskmgr - type 'help' for commands, 'exit' to leave");
    loop {
        print!("diskmgr> ");
        let line = read_line();
        let a: Vec<&str> = line.split_whitespace().collect();
        if a.is_empty() {
            continue;
        }
        let hs = block_handles();
        let pick = |i: usize| -> Result<ScopedProtocol<BlockIO>, String> {
            let n: usize = a.get(i).ok_or("missing disk #")?.parse().map_err(|_| "bad disk #")?;
            open_bio(*hs.get(n).ok_or("no such disk")?)
        };
        let num = |i: usize| -> Result<u64, String> {
            a.get(i).ok_or("missing argument")?.parse::<u64>().map_err(|_| "bad number".to_string())
        };
        let r: Result<(), String> = (|| match a[0].to_ascii_lowercase().as_str() {
            "exit" | "quit" | "q" => Err("\0".to_string()),
                                     "help" | "?" => {
                                         println!("list                     list block devices");
                                         println!("info <d>                 media details");
                                         println!("gpt <d>                  dump GPT partitions");
                                         println!("read <d> <lba> [n]       hexdump n blocks (default 1, max 8)");
                                         println!("zero <d> <lba> <n>       write n zeroed blocks");
                                         println!("new <d> <size> [name]    create GPT partition, size like 512M / 4G (default M)");
                                         println!("del <d> <part#>          delete a GPT partition entry");
                                         println!("                         (only the table entry is removed; the data blocks");
                                         println!("                          are NOT erased, but the OS will no longer see it)");
                                         println!("exit");
                                         Ok(())
                                     }
                                     "list" => { disk_list(&mut Con); Ok(()) }
                                     "info" => {
                                         let b = pick(1)?;
                                         let m = b.media();
                                         println!("media id      : {}", m.media_id());
                                         println!("block size    : {}", m.block_size());
                                         println!("last block    : {}", m.last_block());
                                         println!("size          : {}", human((m.last_block() + 1) * m.block_size() as u64));
                                         println!("io align      : {}", m.io_align());
                                         println!("removable     : {}", m.is_removable_media());
                                         println!("present       : {}", m.is_media_present());
                                         println!("logical part  : {}", m.is_logical_partition());
                                         println!("read only     : {}", m.is_read_only());
                                         Ok(())
                                     }
                                     "gpt" => { let mut b = pick(1)?; gpt_dump(&mut Con, &mut b) }
                                     "read" => {
                                         let mut b = pick(1)?;
                                         let lba = num(2)?;
                                         let n = if a.len() > 3 { num(3)?.clamp(1, 8) } else { 1 };
                                         let bs = b.media().block_size() as u64;
                                         let buf = rd_blocks(&mut b, lba, n)?;
                                         hexdump(&mut Con, lba * bs, &buf);
                                         Ok(())
                                     }
                                     "zero" => {
                                         let mut b = pick(1)?;
                                         let lba = num(2)?;
                                         let n = num(3)?;
                                         if b.media().is_read_only() {
                                             return Err("media is read-only".to_string());
                                         }
                                         let bs = b.media().block_size() as u64;
                                         let id = b.media().media_id();
                                         let chunk = core::cmp::max(1, (1u64 << 20) / bs);
                                         let zeros = vec![0u8; (chunk * bs) as usize];
                                         let mut done = 0u64;
                                         while done < n {
                                             let c = core::cmp::min(chunk, n - done);
                                             b.write_blocks(id, lba + done, &zeros[..(c * bs) as usize]).map_err(es)?;
                                             done += c;
                                         }
                                         b.flush_blocks().map_err(es)?;
                                         println!("zeroed {n} blocks @ {lba}");
                                         Ok(())
                                     }
                                     "new" => {
                                         let mut b = pick(1)?;
                                         let sz = a.get(2).ok_or("missing size (e.g. 512M or 4G)")?;
                                         let up = sz.to_ascii_uppercase();
                                         let (num_s, mult) = if let Some(x) = up.strip_suffix('G') {
                                             (x, 1u64 << 30)
                                         } else if let Some(x) = up.strip_suffix('M') {
                                             (x, 1u64 << 20)
                                         } else {
                                             (up.as_str(), 1u64 << 20)
                                         };
                                         let v: u64 = num_s.parse().map_err(|_| "bad size".to_string())?;
                                         let blocks = core::cmp::max(1, v * mult / b.media().block_size() as u64);
                                         let name = if a.len() > 3 { a[3..].join(" ") } else { "EFICMD".to_string() };
                                         let idx = gpt_new(&mut b, blocks, &name)?;
                                         println!("created partition #{idx} '{name}' ({blocks} blocks)");
                                         Ok(())
                                     }
                                     "del" => {
                                         let mut b = pick(1)?;
                                         let i = num(2)?;
                                         gpt_del(&mut b, i as usize)?;
                                         println!("deleted partition #{i} (entry removed, data not erased)");
                                         Ok(())
                                     }
                                     _ => Err("unknown command".to_string()),
        })();
        if let Err(e) = r {
            if e == "\0" {
                return;
            }
            println!("error: {e}");
        }
    }
}

// ---------------------------------------------------------------- .com / .efi loader

#[repr(C)]
struct ComApi {
    magic: u64,
    version: u32,
    reserved: u32,
    args: *const u8,
    args_len: u64,
    write: extern "efiapi" fn(*const u8, u64),
    getchar: extern "efiapi" fn() -> u32,
    read: extern "efiapi" fn(*mut u8, u64) -> u64,
    stall: extern "efiapi" fn(u64),
}

static CUR_IO: AtomicPtr<Io> = AtomicPtr::new(core::ptr::null_mut());

extern "efiapi" fn com_write(p: *const u8, n: u64) {
    let io = CUR_IO.load(Ordering::SeqCst);
    if io.is_null() || p.is_null() {
        return;
    }
    unsafe { (*io).put(core::slice::from_raw_parts(p, n as usize)) };
}

extern "efiapi" fn com_getchar() -> u32 {
    match getkey() {
        Key::Printable(c) => {
            let ch = kbl_translate(char::from(c));
            u32::from(u16::try_from(ch as u32).unwrap_or(0xFFFF))
        }
        Key::Special(s) => 0x1_0000 + s.0 as u32,
    }
}

extern "efiapi" fn com_read(buf: *mut u8, cap: u64) -> u64 {
    let io = CUR_IO.load(Ordering::SeqCst);
    if io.is_null() || buf.is_null() {
        return 0;
    }
    unsafe {
        let io = &mut *io;
        let dst = core::slice::from_raw_parts_mut(buf, cap as usize);
        let data: Vec<u8> = if io.has_input {
            let rem = &io.input[io.pos..];
            let n = core::cmp::min(rem.len(), dst.len());
            let d = rem[..n].to_vec();
            io.pos += n;
            d
        } else {
            let mut b = read_line().into_bytes();
            b.push(b'\n');
            b
        };
        let n = core::cmp::min(data.len(), dst.len());
        dst[..n].copy_from_slice(&data[..n]);
        n as u64
    }
}

extern "efiapi" fn com_stall(us: u64) {
    boot::stall(us as usize);
}

fn run_com(data: &[u8], args: &str, io: &mut Io) -> Result<u64, String> {
    if data.is_empty() {
        return Err("empty file".to_string());
    }
    let pages = (data.len() + 0x10000 + 4095) / 4096;
    let mem = boot::allocate_pages(AllocateType::AnyPages, MemoryType::LOADER_CODE, pages).map_err(es)?;
    unsafe {
        core::ptr::write_bytes(mem.as_ptr(), 0, pages * 4096);
        core::ptr::copy_nonoverlapping(data.as_ptr(), mem.as_ptr(), data.len());
    }
    let mut argv = args.as_bytes().to_vec();
    argv.push(0);
    let api = ComApi {
        magic: u64::from_le_bytes(*b"EFICOM64"),
        version: 1,
        reserved: 0,
        args: argv.as_ptr(),
        args_len: (argv.len() - 1) as u64,
        write: com_write,
        getchar: com_getchar,
        read: com_read,
        stall: com_stall,
    };
    CUR_IO.store(io as *mut Io, Ordering::SeqCst);
    let entry: extern "efiapi" fn(*const ComApi) -> u64 = unsafe { core::mem::transmute(mem.as_ptr()) };
    let rc = entry(&api);
    CUR_IO.store(core::ptr::null_mut(), Ordering::SeqCst);
    unsafe {
        let _ = boot::free_pages(mem, pages);
    }
    Ok(rc)
}

fn run_efi(data: &[u8], cmdline: &str) -> Result<u64, String> {
    let h = boot::load_image(
        boot::image_handle(),
                             LoadImageSource::FromBuffer { buffer: data, file_path: None },
    )
    .map_err(es)?;
    if !cmdline.is_empty() {
        if let Ok(c) = CString16::try_from(cmdline) {
            let c = Box::leak(Box::new(c));
            let mut li = boot::open_protocol_exclusive::<LoadedImage>(h).map_err(es)?;
            unsafe { li.set_load_options(c.as_ptr() as *const u8, c.num_bytes() as u32) };
        }
    }
    match boot::start_image(h) {
        Ok(()) => Ok(0),
        Err(e) => Err(format!("image exited: {:?}", e.status())),
    }
}

// ---------------------------------------------------------------- editor

fn term_size() -> (usize, usize) {
    system::with_stdout(|o| o.current_mode().ok().flatten().map(|m| (m.columns(), m.rows())))
    .unwrap_or((80, 25))
}

fn put_row(row: usize, s: &str, width: usize, inv: bool) {
    let mut t: String = s
    .chars()
    .take(width)
    .map(|c| if (c as u32) < 0x20 || (c as u32) > 0xFFFF || c == '\u{7f}' { ' ' } else { c })
    .collect();
    while t.chars().count() < width {
        t.push(' ');
    }
    if let Ok(c) = CString16::try_from(t.as_str()) {
        system::with_stdout(|o| {
            if inv {
                let _ = o.set_color(Color::Black, Color::LightGray);
            }
            let _ = o.set_cursor_position(0, row);
            let _ = o.output_string(&c);
            if inv {
                let _ = o.set_color(Color::LightGray, Color::Black);
            }
        });
    }
}

struct Editor<'a> {
    sh: &'a Shell,
    lines: Vec<Vec<char>>,
    cy: usize,
    cx: usize,
    top: usize,
    left: usize,
    drive: usize,
    path: String,
    modified: bool,
    crlf: bool,
    cutbuf: Vec<Vec<char>>,
    last_cut: bool,
    msg: String,
    last_search: String,
}

impl<'a> Editor<'a> {
    fn bytes(&self) -> Vec<u8> {
        let nl = if self.crlf { "\r\n" } else { "\n" };
        let mut s = String::new();
        if !(self.lines.len() == 1 && self.lines[0].is_empty()) {
            for l in &self.lines {
                s.extend(l.iter());
                s.push_str(nl);
            }
        }
        s.into_bytes()
    }

    fn text_h(rows: usize) -> usize {
        rows.saturating_sub(3).max(1)
    }

    fn draw(&mut self) {
        let (cols, rows) = term_size();
        let th = Self::text_h(rows);
        if self.cy < self.top {
            self.top = self.cy;
        }
        if self.cy >= self.top + th {
            self.top = self.cy + 1 - th;
        }
        if self.cx < self.left {
            self.left = self.cx;
        }
        if self.cx >= self.left + cols {
            self.left = self.cx + 1 - cols;
        }
        let _ = system::with_stdout(|o| o.enable_cursor(false));
        let name = if self.path.is_empty() { "New Buffer" } else { self.path.as_str() };
        let title = format!(
            " EFIEDIT 0.1   {}{}   Ln {}/{} Col {}",
            name,
            if self.modified { " (modified)" } else { "" },
                self.cy + 1,
                self.lines.len(),
                            self.cx + 1
        );
        put_row(0, &title, cols, true);
        for r in 0..th {
            let li = self.top + r;
            let s: String = if li < self.lines.len() {
                self.lines[li].iter().skip(self.left).take(cols).collect()
            } else {
                String::new()
            };
            put_row(1 + r, &s, cols, false);
        }
        let m = if self.msg.is_empty() { String::new() } else { format!(" [ {} ]", self.msg) };
        put_row(th + 1, &m, cols, false);
        put_row(
            th + 2,
            " ^G Help  ^O Save  ^X Exit  ^W Search  ^K Cut  ^U Paste  ^A/^E Home/End  F2 Save  F10 Exit",
            cols.saturating_sub(1),
                true,
        );
        let _ = system::with_stdout(|o| {
            let _ = o.set_cursor_position(self.cx - self.left, 1 + self.cy - self.top);
            o.enable_cursor(true)
        });
    }

    fn prompt(&mut self, label: &str, default: &str) -> Option<String> {
        let mut s: Vec<char> = default.chars().collect();
        loop {
            let (cols, rows) = term_size();
            let row = rows.saturating_sub(2);
            let line = format!("{label}: {}", s.iter().collect::<String>());
            put_row(row, &line, cols, false);
            let cx = core::cmp::min(line.chars().count(), cols.saturating_sub(1));
            let _ = system::with_stdout(|o| {
                let _ = o.set_cursor_position(cx, row);
                o.enable_cursor(true)
            });
            match getkey() {
                Key::Printable(c) => {
                    let ch = char::from(c);
                    match ch as u32 {
                        0x0D => return Some(s.iter().collect()),
                        0x08 => {
                            s.pop();
                        }
                        0x03 => return None,
                        x if x >= 0x20 => s.push(ch),
                        _ => {}
                    }
                }
                Key::Special(ScanCode::ESCAPE) => return None,
                _ => {}
            }
        }
    }

    fn save(&mut self) -> bool {
        let default = self.path.clone();
        let name = match self.prompt("File Name to Write", &default) {
            Some(n) if !n.trim().is_empty() => n,
            _ => {
                self.msg = "Cancelled".to_string();
                return false;
            }
        };
        let (d, p) = self.sh.resolve(name.trim());
        let data = self.bytes();
        let r = self.sh.with_fs(d, |fs| fs.write(pb(&p)?, &data).map_err(es));
        match r {
            Ok(()) => {
                self.drive = d;
                self.path = format!("{}:{}", letter(d), p);
                self.modified = false;
                self.msg = format!("Wrote {} lines", self.lines.len());
                true
            }
            Err(e) => {
                self.msg = format!("Error: {e}");
                false
            }
        }
    }

    fn help(&mut self) {
        let (cols, rows) = term_size();
        let th = Self::text_h(rows);
        let text = [
            "EFIEDIT help",
            "",
            "^O / F2    Write out (save)         ^X / F10   Exit",
            "^W         Search forward           ^K         Cut line(s)",
            "^U         Paste cut line(s)        ^G / F1    This help",
            "^A / Home  Start of line            ^E / End   End of line",
            "^Y / PgUp  Page up                  ^V / PgDn  Page down",
            "Arrows move, Backspace/Delete edit, Tab inserts 4 spaces",
            "In prompts: Enter accepts, Esc / ^C cancels",
            "",
            "Press any key to return",
        ];
        for r in 0..th {
            put_row(1 + r, text.get(r).copied().unwrap_or(""), cols, false);
        }
        let _ = getkey();
    }

    fn search(&mut self) {
        let default = self.last_search.clone();
        let q = match self.prompt("Search", &default) {
            Some(q) if !q.is_empty() => q,
            _ => return,
        };
        self.last_search = q.clone();
        let needle: Vec<char> = q.chars().collect();
        let n = self.lines.len();
        for k in 0..=n {
            let li = (self.cy + k) % n;
            let start = if k == 0 { self.cx + 1 } else { 0 };
            let line = &self.lines[li];
            if line.len() >= needle.len() {
                for x in start..=(line.len() - needle.len()) {
                    if line[x..x + needle.len()] == needle[..] {
                        self.cy = li;
                        self.cx = x;
                        self.msg.clear();
                        return;
                    }
                }
            }
        }
        self.msg = format!("\"{q}\" not found");
    }

    fn clamp_x(&mut self) {
        let l = self.lines[self.cy].len();
        if self.cx > l {
            self.cx = l;
        }
    }

    fn run(&mut self) {
        loop {
            self.draw();
            let was_cut = self.last_cut;
            self.last_cut = false;
            let (_, rows) = term_size();
            let th = Self::text_h(rows);
            match getkey() {
                Key::Printable(c) => {
                    let ch = char::from(c);
                    self.msg.clear();
                    match ch as u32 {
                        0x0D => {
                            let rest = self.lines[self.cy].split_off(self.cx);
                            self.cy += 1;
                            self.lines.insert(self.cy, rest);
                            self.cx = 0;
                            self.modified = true;
                        }
                        0x08 => {
                            if self.cx > 0 {
                                self.lines[self.cy].remove(self.cx - 1);
                                self.cx -= 1;
                                self.modified = true;
                            } else if self.cy > 0 {
                                let cur = self.lines.remove(self.cy);
                                self.cy -= 1;
                                self.cx = self.lines[self.cy].len();
                                self.lines[self.cy].extend(cur);
                                self.modified = true;
                            }
                        }
                        0x09 => {
                            for _ in 0..4 {
                                self.lines[self.cy].insert(self.cx, ' ');
                                self.cx += 1;
                            }
                            self.modified = true;
                        }
                        0x01 => self.cx = 0,
                        0x05 => self.cx = self.lines[self.cy].len(),
                        0x07 => self.help(),
                        0x0F => {
                            self.save();
                        }
                        0x17 => self.search(),
                        0x0B => {
                            if !was_cut {
                                self.cutbuf.clear();
                            }
                            let l = self.lines.remove(self.cy);
                            self.cutbuf.push(l);
                            if self.lines.is_empty() {
                                self.lines.push(Vec::new());
                            }
                            if self.cy >= self.lines.len() {
                                self.cy = self.lines.len() - 1;
                            }
                            self.cx = 0;
                            self.modified = true;
                            self.last_cut = true;
                        }
                        0x15 => {
                            if !self.cutbuf.is_empty() {
                                for (i, l) in self.cutbuf.clone().into_iter().enumerate() {
                                    self.lines.insert(self.cy + i, l);
                                }
                                self.cy += self.cutbuf.len();
                                self.cx = 0;
                                self.modified = true;
                            }
                        }
                        0x19 => {
                            self.cy = self.cy.saturating_sub(th);
                            self.clamp_x();
                        }
                        0x16 => {
                            self.cy = core::cmp::min(self.cy + th, self.lines.len() - 1);
                            self.clamp_x();
                        }
                        0x18 => {
                            if self.try_exit() {
                                return;
                            }
                        }
                        0x20..=0x7E | 0xA0..=0xFFFF => {
                            let ch = kbl_translate(ch);
                            self.lines[self.cy].insert(self.cx, ch);
                            self.cx += 1;
                            self.modified = true;
                        }
                        _ => {}
                    }
                }
                Key::Special(s) => {
                    self.msg.clear();
                    match s {
                        ScanCode::UP => {
                            if self.cy > 0 {
                                self.cy -= 1;
                                self.clamp_x();
                            }
                        }
                        ScanCode::DOWN => {
                            if self.cy + 1 < self.lines.len() {
                                self.cy += 1;
                                self.clamp_x();
                            }
                        }
                        ScanCode::LEFT => {
                            if self.cx > 0 {
                                self.cx -= 1;
                            } else if self.cy > 0 {
                                self.cy -= 1;
                                self.cx = self.lines[self.cy].len();
                            }
                        }
                        ScanCode::RIGHT => {
                            if self.cx < self.lines[self.cy].len() {
                                self.cx += 1;
                            } else if self.cy + 1 < self.lines.len() {
                                self.cy += 1;
                                self.cx = 0;
                            }
                        }
                        ScanCode::HOME => self.cx = 0,
                        ScanCode::END => self.cx = self.lines[self.cy].len(),
                        ScanCode::PAGE_UP => {
                            self.cy = self.cy.saturating_sub(th);
                            self.clamp_x();
                        }
                        ScanCode::PAGE_DOWN => {
                            self.cy = core::cmp::min(self.cy + th, self.lines.len() - 1);
                            self.clamp_x();
                        }
                        ScanCode::DELETE => {
                            if self.cx < self.lines[self.cy].len() {
                                self.lines[self.cy].remove(self.cx);
                                self.modified = true;
                            } else if self.cy + 1 < self.lines.len() {
                                let nxt = self.lines.remove(self.cy + 1);
                                self.lines[self.cy].extend(nxt);
                                self.modified = true;
                            }
                        }
                        ScanCode::FUNCTION_1 => self.help(),
                        ScanCode::FUNCTION_2 => {
                            self.save();
                        }
                        ScanCode::FUNCTION_10 => {
                            if self.try_exit() {
                                return;
                            }
                        }
                        _ => {
                            if let Some(ch) = kbl_lookup(s.0) {
                                self.lines[self.cy].insert(self.cx, ch);
                                self.cx += 1;
                                self.modified = true;
                            }
                        }
                    }
                }
            }
        }
    }

    fn try_exit(&mut self) -> bool {
        if !self.modified {
            return true;
        }
        let (cols, rows) = term_size();
        put_row(rows.saturating_sub(2), " Save modified buffer? (Y = yes, N = discard, Esc = cancel)", cols, false);
        loop {
            match getkey() {
                Key::Printable(c) => match char::from(c).to_ascii_lowercase() {
                    'y' => return self.save(),
                    'n' => return true,
                    _ => {}
                },
                Key::Special(ScanCode::ESCAPE) => {
                    self.msg = "Cancelled".to_string();
                    return false;
                }
                _ => {}
            }
        }
    }
}

fn edit(sh: &Shell, arg: Option<&str>) -> Result<(), String> {
    let (drive, path) = match arg {
        Some(a) if !a.is_empty() => {
            let (d, p) = sh.resolve(a);
            (d, format!("{}:{}", letter(d), p))
        }
        _ => (sh.cur, String::new()),
    };
    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    let mut crlf = false;
    let mut msg = String::from("New buffer");
    if !path.is_empty() {
        let (d, p) = sh.resolve(&path);
        let r = sh.with_fs(d, |fs| {
            let pp = pb(&p)?;
            if fs.try_exists(pp.clone()).map_err(es)? {
                fs.read(pp).map(Some).map_err(es)
            } else {
                Ok(None)
            }
        })?;
        match r {
            Some(data) => {
                let text = String::from_utf8_lossy(&data);
                crlf = text.contains("\r\n");
                let mut v: Vec<Vec<char>> =
                text.split('\n').map(|l| l.trim_end_matches('\r').chars().collect()).collect();
                if v.len() > 1 && v.last().map_or(false, |l| l.is_empty()) {
                    v.pop();
                }
                msg = format!("Read {} lines", v.len());
                lines = v;
            }
            None => msg = "New file".to_string(),
        }
    }
    let mut ed = Editor {
        sh,
        lines,
        cy: 0,
        cx: 0,
        top: 0,
        left: 0,
        drive,
        path,
        modified: false,
        crlf,
        cutbuf: Vec::new(),
        last_cut: false,
        msg,
        last_search: String::new(),
    };
    let _ = ed.drive;
    cls();
    ed.run();
    let _ = system::with_stdout(|o| {
        let _ = o.set_color(Color::LightGray, Color::Black);
        let _ = o.clear();
        o.enable_cursor(true)
    });
    Ok(())
}

// ---------------------------------------------------------------- wildcards

fn has_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?')
}

fn glob_match(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    let (mut pi, mut ti, mut star_p, mut star_t) = (0usize, 0usize, usize::MAX, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_p = pi;
            star_t = ti;
            pi += 1;
        } else if star_p != usize::MAX {
            pi = star_p + 1;
            star_t += 1;
            ti = star_t;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------- shell

struct Shell {
    vols: Vec<Handle>,
    map: [Option<usize>; 26],
    cwd: [String; 26],
    cur: usize,
    boot: usize,
    quit: bool,
    bat_depth: u32,
}

impl Shell {
    fn new() -> Self {
        let mut s = Shell {
            vols: Vec::new(),
            map: [None; 26],
            cwd: core::array::from_fn(|_| String::from("\\")),
            cur: 2,
            boot: 2,
            quit: false,
            bat_depth: 0,
        };
        s.scan();
        s
    }

    fn scan(&mut self) {
        self.vols = boot::locate_handle_buffer(SearchType::from_proto::<SimpleFileSystem>())
        .map(|b| b.to_vec())
        .unwrap_or_default();
        let boot_dev = boot::open_protocol_exclusive::<LoadedImage>(boot::image_handle())
        .ok()
        .and_then(|li| li.device());
        self.map = [None; 26];
        let mut next = self.boot + 1;
        for (i, h) in self.vols.iter().enumerate() {
            if Some(*h) == boot_dev && self.map[self.boot].is_none() {
                self.map[self.boot] = Some(i);
                continue;
            }
            while next < 26 && (next == self.boot || self.map[next].is_some()) {
                next += 1;
            }
            if next < 26 {
                self.map[next] = Some(i);
                next += 1;
            }
        }
        if self.map[self.cur].is_none() {
            self.cur = (0..26).find(|&i| self.map[i].is_some()).unwrap_or(self.boot);
        }
    }

    fn with_fs<T>(&self, drv: usize, f: impl FnOnce(&mut FileSystem) -> Result<T, String>) -> Result<T, String> {
        let vi = self.map[drv].ok_or_else(|| format!("drive {}: not mounted", letter(drv)))?;
        let sfs = boot::open_protocol_exclusive::<SimpleFileSystem>(self.vols[vi]).map_err(es)?;
        let mut fs = FileSystem::new(sfs);
        f(&mut fs)
    }

    fn resolve(&self, arg: &str) -> (usize, String) {
        let b = arg.as_bytes();
        let (drv, rest) = if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
            ((b[0].to_ascii_uppercase() - b'A') as usize, &arg[2..])
        } else {
            (self.cur, arg)
        };
        let mut parts: Vec<&str> = if rest.starts_with('\\') || rest.starts_with('/') {
            Vec::new()
        } else {
            self.cwd[drv].split('\\').filter(|s| !s.is_empty()).collect()
        };
        for p in rest.split(|c| c == '\\' || c == '/') {
            match p {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                x => parts.push(x),
            }
        }
        let mut s = String::from("\\");
        s.push_str(&parts.join("\\"));
        (drv, s)
    }

    fn prompt(&self) -> String {
        format!("{}:{}> ", letter(self.cur), self.cwd[self.cur])
    }

    fn is_dir(fs: &mut FileSystem, p: &str) -> bool {
        if p == "\\" {
            return true;
        }
        match pb(p) {
            Ok(pp) => fs.metadata(pp).map(|m| m.is_directory()).unwrap_or(false),
            Err(_) => false,
        }
    }

    fn is_file(fs: &mut FileSystem, p: &str) -> bool {
        match pb(p) {
            Ok(pp) => fs.metadata(pp).map(|m| !m.is_directory()).unwrap_or(false),
            Err(_) => false,
        }
    }

    fn read_file(&self, p: &str) -> Result<Vec<u8>, String> {
        let (d, pp) = self.resolve(p);
        self.with_fs(d, |fs| fs.read(pb(&pp)?).map_err(es))
    }

    fn write_file(&self, p: &str, data: &[u8], append: bool) -> Result<(), String> {
        let (d, pp) = self.resolve(p);
        self.with_fs(d, |fs| {
            let path = pb(&pp)?;
            if append {
                let mut old = fs.read(path.clone()).unwrap_or_default();
                old.extend_from_slice(data);
                fs.write(path, old).map_err(es)
            } else {
                fs.write(path, data).map_err(es)
            }
        })
    }

    fn load_kbl(&self, f: &str) -> Result<(), String> {
        let data = self.read_file(f)?;
        let text = String::from_utf8_lossy(&data);
        let mut k = Kbl { name: "custom".to_string(), keys: Vec::new(), cmap: Vec::new(), scan: Vec::new() };
        for (n, line0) in text.lines().enumerate() {
            let line = line0.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
                continue;
            }
            let err = |e: String| format!("{}:{}: {e}", f, n + 1);
            if line.starts_with("key") || (line.starts_with("Key") && line.contains('<')) {
                let lt = line.find('<').ok_or_else(|| err("expected <KEY>".into()))?;
                let gt = line.find('>').ok_or_else(|| err("expected <KEY>".into()))?;
                let kn = line[lt + 1..gt].trim().to_ascii_uppercase();
                let id = KEY_NAMES
                    .iter()
                    .find(|(nm, _)| *nm == kn)
                    .map(|&(_, id)| id)
                    .ok_or_else(|| err(format!("unknown key <{kn}>")))?;
                let lb = line.find('[').ok_or_else(|| err("expected [ ... ]".into()))?;
                let rb = line.rfind(']').ok_or_else(|| err("expected [ ... ]".into()))?;
                let mut levels: [Option<char>; 4] = [None; 4];
                for (i, tok) in line[lb + 1..rb].split(',').enumerate() {
                    if i >= 4 {
                        break;
                    }
                    let tok = tok.trim();
                    if tok.is_empty() {
                        continue;
                    }
                    levels[i] = Some(keysym(tok).ok_or_else(|| err(format!("unknown keysym '{tok}'")))?);
                }
                k.keys.retain(|(kid, _)| *kid != id);
                k.keys.push((id, levels));
                continue;
            }
            if let Some(rest) = line.strip_prefix("remap").or_else(|| line.strip_prefix("REMAP")) {
                let rest = rest.trim_start();
                let eq = rest.find('=').ok_or_else(|| err("expected remap 'x' = 'y'".into()))?;
                let from = keysym(rest[..eq].trim()).ok_or_else(|| err("bad remap source".into()))?;
                let to = keysym(rest[eq + 1..].trim().trim_end_matches(';')).ok_or_else(|| err("bad remap target".into()))?;
                k.cmap.retain(|(c, _)| *c != from);
                k.cmap.push((from, to));
                continue;
            }
            let (l, r) = line
                .split_once('=')
                .ok_or_else(|| err("expected KEY=char, remap 'x' = 'y' or key <NAME> { [ ... ] }".into()))?;
            if l.trim().to_ascii_uppercase().starts_with("NAME") {
                let r = r.trim().trim_end_matches(';');
                let r = r.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(r);
                k.name = r.trim().to_string();
                continue;
            }
            let kt = l.trim();
            if kt.len() >= 3 && kt.starts_with('\'') && kt.ends_with('\'') {
                let mut cit = kt[1..kt.len() - 1].chars();
                let from = cit.next().ok_or_else(|| err("empty char key".into()))?;
                if cit.next().is_some() {
                    return Err(err("char key must be one character".into()));
                }
                let to = kbl_char(r.trim().trim_end_matches(';')).map_err(err)?;
                k.cmap.retain(|(c, _)| *c != from);
                k.cmap.push((from, to));
                continue;
            }
            let sc = parse_key(l).map_err(err)?;
            let ch = kbl_char(r.trim().trim_end_matches(';')).map_err(err)?;
            k.scan.retain(|(s, _)| *s != sc);
            k.scan.push((sc, ch));
        }
        if k.keys.is_empty() && k.cmap.is_empty() && k.scan.is_empty() {
            return Err("no key mappings found in file".to_string());
        }
        unsafe {
            *core::ptr::addr_of_mut!(CUR_KBL) = Some(Box::new(k));
        }
        Ok(())
    }

    // expand a wildcard argument into matching "L:\path" strings
    fn expand(&self, arg: &str) -> Result<Vec<String>, String> {
        if !has_glob(arg) {
            let (d, p) = self.resolve(arg);
            return Ok(vec![format!("{}:{}", letter(d), p)]);
        }
        let pos = arg.rfind(|c| c == '\\' || c == '/').map(|i| i + 1).unwrap_or(0);
        let (dirpart, pat) = arg.split_at(pos);
        let (d, dp) = self.resolve(dirpart.trim_end_matches(|c| c == '\\' || c == '/'));
        let ents = self.with_fs(d, |fs| {
            let it = fs.read_dir(pb(&dp)?).map_err(es)?;
            let mut v = Vec::new();
            for e in it {
                v.push(e.map_err(es)?);
            }
            Ok(v)
        })?;
        let mut out = Vec::new();
        for e in &ents {
            let name = format!("{}", e.file_name());
            if name == "." || name == ".." {
                continue;
            }
            if glob_match(pat, &name) {
                out.push(format!("{}:{}", letter(d), joinp(&dp, &name)));
            }
        }
        out.sort();
        Ok(out)
    }

    fn kbl_test(&self) {
        println!("kbl test - press keys to see what the firmware sends (Esc to exit)");
        loop {
            match getkey() {
                Key::Printable(c) => {
                    let raw = char::from(c);
                    let t = kbl_translate(raw);
                    if t != raw {
                        println!("  printable U+{:04X}  '{}'  -> layout makes '{}'", u16::from(c) as u32, raw, t);
                    } else {
                        println!("  printable U+{:04X}  '{}'", u16::from(c) as u32, raw);
                    }
                }
                Key::Special(s) => {
                    if s.0 == 0x17 {
                        println!("  ESC - exiting test");
                        return;
                    }
                    let mapped = kbl_lookup(s.0)
                        .map(|c| format!("  -> mapped to '{c}'"))
                        .unwrap_or_default();
                    println!("  scancode 0x{:02X} ({}){}", s.0, key_name(s.0), mapped);
                }
            }
        }
    }

    fn kbl(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        match rest.first().map(|s| s.as_str()) {
            None => {
                wl!(io, "keyboard layout: {}", cur_kbl_name());
                if let Some(k) = cur_kbl() {
                    for (kid, lv) in &k.keys {
                        let nm = KEY_NAMES.iter().find(|&(_, id)| id == kid).map(|(n, _)| *n).unwrap_or("?");
                        let lv: Vec<String> = lv.iter().map(|c| c.map(|x| format!("'{x}'")).unwrap_or_else(|| "none".into())).collect();
                        wl!(io, "  <{nm}>  [ {} ]", lv.join(", "));
                    }
                    for (f, t) in &k.cmap {
                        wl!(io, "  remap '{f}' -> '{t}'");
                    }
                    for (sc, ch) in &k.scan {
                        wl!(io, "  {:<6} = {ch}", key_name(sc));
                    }
                }
                Ok(())
            }
            Some("default") => {
                unsafe {
                    *core::ptr::addr_of_mut!(CUR_KBL) = None;
                }
                wl!(io, "layout reset to default");
                Ok(())
            }
            Some("test") => {
                self.kbl_test();
                Ok(())
            }
            Some(f) => {
                self.load_kbl(f)?;
                wl!(io, "layout loaded: {}", cur_kbl_name());
                Ok(())
            }
        }
    }

    fn dir(&self, arg: &str, io: &mut Io) -> Result<(), String> {
        let (d, p, pat) = if has_glob(arg) {
            let pos = arg.rfind(|c| c == '\\' || c == '/').map(|i| i + 1).unwrap_or(0);
            let (dirpart, pat) = arg.split_at(pos);
            let (d, dp) = self.resolve(dirpart.trim_end_matches(|c| c == '\\' || c == '/'));
            (d, dp, Some(pat.to_string()))
        } else {
            let (d, p) = self.resolve(arg);
            (d, p, None)
        };
        let ents = self.with_fs(d, |fs| {
            let it = fs.read_dir(pb(&p)?).map_err(es)?;
            let mut v = Vec::new();
            for e in it {
                v.push(e.map_err(es)?);
            }
            Ok(v)
        })?;
        wl!(io, " Directory of {}:{}\n", letter(d), p);
        let (mut nf, mut nd, mut bytes) = (0u32, 0u32, 0u64);
        for e in &ents {
            let name = format!("{}", e.file_name());
            if name == "." || name == ".." {
                continue;
            }
            if let Some(pat) = &pat {
                if !glob_match(pat, &name) {
                    continue;
                }
            }
            let t = e.modification_time();
            let isd = e.is_directory();
            wl!(
                io,
                "{:04}-{:02}-{:02} {:02}:{:02}  {:>5} {:>12}  {}",
                t.year(), t.month(), t.day(), t.hour(), t.minute(),
                if isd { "<DIR>" } else { "" },
                    if isd { String::new() } else { e.file_size().to_string() },
                        name
            );
            if isd { nd += 1 } else { nf += 1; bytes += e.file_size() }
        }
        wl!(io, "\n {nf} file(s) {bytes} bytes, {nd} dir(s)");
        Ok(())
    }

    fn cd(&mut self, arg: &str, io: &mut Io) -> Result<(), String> {
        if arg.is_empty() {
            wl!(io, "{}:{}", letter(self.cur), self.cwd[self.cur]);
            return Ok(());
        }
        let (d, p) = self.resolve(arg);
        let ok = self.with_fs(d, |fs| Ok(Self::is_dir(fs, &p)))?;
        if !ok {
            return Err("path not found or not a directory".to_string());
        }
        self.cwd[d] = p;
        Ok(())
    }

    fn del(&self, rest: &[String]) -> Result<(), String> {
        if rest.is_empty() {
            return Err("usage: del <file> [file...]  (wildcards * ? ok)".to_string());
        }
        for f in rest {
            for hit in self.expand(f)? {
                let (d, p) = self.resolve(&hit);
                let is_dir = self.with_fs(d, |fs| Ok(Self::is_dir(fs, &p)))?;
                if is_dir {
                    return Err(format!("{hit}: is a directory (use rd)"));
                }
                self.with_fs(d, |fs| fs.remove_file(pb(&p)?).map_err(es))?;
            }
        }
        Ok(())
    }

    fn md(&self, arg: &str) -> Result<(), String> {
        if arg.is_empty() {
            return Err("usage: md <dir>".to_string());
        }
        let (d, p) = self.resolve(arg);
        self.with_fs(d, |fs| fs.create_dir(pb(&p)?).map_err(es))
    }

    fn rd(&self, rest: &[String]) -> Result<(), String> {
        let mut recursive = false;
        let mut target = "";
        for a in rest {
            if a.eq_ignore_ascii_case("/s") {
                recursive = true
            } else {
                target = a
            }
        }
        if target.is_empty() {
            return Err("usage: rd [/s] <dir>".to_string());
        }
        let (d, p) = self.resolve(target);
        if p == "\\" {
            return Err("refusing to remove root".to_string());
        }
        self.with_fs(d, |fs| {
            let pp = pb(&p)?;
            if recursive {
                fs.remove_dir_all(pp).map_err(es)
            } else {
                fs.remove_dir(pp).map_err(es)
            }
        })
    }

    fn copy(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        if rest.len() != 2 {
            return Err("usage: copy <src> <dst>   (wildcards ok in src; multiple hits need dst dir)".to_string());
        }
        let hits = self.expand(&rest[0])?;
        let (dd, dp) = self.resolve(&rest[1]);
        let dst_is_dir = self.with_fs(dd, |fs| Ok(Self::is_dir(fs, &dp)))?;
        if hits.len() > 1 && !dst_is_dir {
            return Err("copying multiple files needs a directory as dst".to_string());
        }
        let mut total = 0u64;
        for hit in &hits {
            let (sd, sp) = self.resolve(hit);
            let data = self.with_fs(sd, |fs| fs.read(pb(&sp)?).map_err(es))?;
            let ddp = if dst_is_dir { joinp(&dp, base(&sp)) } else { dp.clone() };
            total += data.len() as u64;
            self.with_fs(dd, |fs| fs.write(pb(&ddp)?, data).map_err(es))?;
        }
        wl!(io, "copied {} file(s), {} -> {}:{}", hits.len(), human(total), letter(dd), dp);
        Ok(())
    }

    fn xcopy_tree(&self, sd: usize, sp: &str, dd: usize, dp: &str, empty: bool, nf: &mut u64, nb: &mut u64) -> Result<(), String> {
        let is_dir = self.with_fs(sd, |fs| Ok(Self::is_dir(fs, sp)))?;
        if !is_dir {
            let data = self.with_fs(sd, |fs| fs.read(pb(sp)?).map_err(es))?;
            *nb += data.len() as u64;
            self.with_fs(dd, |fs| fs.write(pb(dp)?, data).map_err(es))?;
            *nf += 1;
            return Ok(());
        }
        let ents = self.with_fs(sd, |fs| {
            let it = fs.read_dir(pb(sp)?).map_err(es)?;
            let mut v = Vec::new();
            for e in it {
                v.push(e.map_err(es)?);
            }
            Ok(v)
        })?;
        let mut names = Vec::new();
        for e in &ents {
            let name = format!("{}", e.file_name());
            if name == "." || name == ".." {
                continue;
            }
            names.push(name);
        }
        if !names.is_empty() || empty {
            let _ = self.with_fs(dd, |fs| fs.create_dir_all(pb(dp)?).map_err(es));
        }
        for name in names {
            self.xcopy_tree(sd, &joinp(sp, &name), dd, &joinp(dp, &name), empty, nf, nb)?;
        }
        Ok(())
    }

    fn xcopy(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        let mut empty = false;
        let mut args: Vec<&String> = Vec::new();
        for a in rest {
            if a.eq_ignore_ascii_case("/e") {
                empty = true;
            } else {
                args.push(a);
            }
        }
        if args.len() != 2 {
            return Err("usage: xcopy <src> <dst> [/e]   (recursive; /e copies empty dirs too)".to_string());
        }
        let (dd, dp) = self.resolve(args[1]);
        let dst_is_dir = self.with_fs(dd, |fs| Ok(Self::is_dir(fs, &dp)))?;
        let hits = self.expand(args[0])?;
        if hits.is_empty() {
            return Err(format!("{}: no match", args[0]));
        }
        let (mut nf, mut nb) = (0u64, 0u64);
        let mut last_dst = dp.clone();
        for hit in &hits {
            let (sd, sp) = self.resolve(hit);
            let ddp = if dst_is_dir { joinp(&dp, base(&sp)) } else { dp.clone() };
            self.xcopy_tree(sd, &sp, dd, &ddp, empty, &mut nf, &mut nb)?;
            last_dst = ddp;
        }
        wl!(io, "copied {nf} file(s), {} -> {}:{}", human(nb), letter(dd), last_dst);
        Ok(())
    }

    fn ren(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        if rest.len() != 2 {
            return Err("usage: ren <old> <new>".to_string());
        }
        let (d, p) = self.resolve(&rest[0]);
        let (d2, p2) = self.resolve(&rest[1]);
        if d != d2 {
            return Err("ren works within one drive (use 'move' across drives)".to_string());
        }
        self.with_fs(d, |fs| fs.rename(pb(&p)?, pb(&p2)?).map_err(es))?;
        wl!(io, "renamed -> {}:{}", letter(d2), p2);
        Ok(())
    }

    fn move_cmd(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        if rest.len() != 2 {
            return Err("usage: move <src> <dst>".to_string());
        }
        if has_glob(&rest[0]) {
            let hits = self.expand(&rest[0])?;
            let (dd, dp) = self.resolve(&rest[1]);
            let dst_is_dir = self.with_fs(dd, |fs| Ok(Self::is_dir(fs, &dp)))?;
            if !dst_is_dir {
                return Err("moving multiple files needs a directory as dst".to_string());
            }
            for hit in &hits {
                self.copy(&[hit.clone(), rest[1].clone()], io)?;
                self.del(&[hit.clone()])?;
            }
            return Ok(());
        }
        let (sd, sp) = self.resolve(&rest[0]);
        let (dd, dp) = self.resolve(&rest[1]);
        let src_is_dir = self.with_fs(sd, |fs| Ok(Self::is_dir(fs, &sp)))?;
        let dst_is_dir = self.with_fs(dd, |fs| Ok(Self::is_dir(fs, &dp)))?;
        let ddp = if dst_is_dir { joinp(&dp, base(&sp)) } else { dp };
        if sd == dd {
            let r = self.with_fs(sd, |fs| fs.rename(pb(&sp)?, pb(&ddp)?).map_err(es));
            if r.is_ok() {
                wl!(io, "moved -> {}:{}", letter(dd), ddp);
                return Ok(());
            }
        }
        let (mut nf, mut nb) = (0u64, 0u64);
        self.xcopy_tree(sd, &sp, dd, &ddp, true, &mut nf, &mut nb)?;
        if src_is_dir {
            self.with_fs(sd, |fs| fs.remove_dir_all(pb(&sp)?).map_err(es))?;
        } else {
            self.with_fs(sd, |fs| fs.remove_file(pb(&sp)?).map_err(es))?;
        }
        wl!(io, "moved {nf} file(s), {} -> {}:{}", human(nb), letter(dd), ddp);
        Ok(())
    }

    fn mount(&mut self, rest: &[String], io: &mut Io) -> Result<(), String> {
        let a: Vec<&str> = rest.iter().map(|s| s.as_str()).collect();
        match a.as_slice() {
            [] => {
                self.scan();
                if self.vols.is_empty() {
                    wl!(io, "no filesystems found");
                }
                for i in 0..26 {
                    if let Some(v) = self.map[i] {
                        wl!(io, "{}:  fs{}{}", letter(i), v, if i == self.boot { "  (boot)" } else { "" });
                    }
                }
                Ok(())
            }
            ["-d", l] => {
                let i = Self::drv(l)?;
                self.map[i] = None;
                Ok(())
            }
            [fs, l] => {
                let v: usize = fs.trim_start_matches("fs").parse().map_err(|_| "usage: mount fsN L:")?;
                if v >= self.vols.len() {
                    return Err("no such filesystem".to_string());
                }
                let i = Self::drv(l)?;
                for m in self.map.iter_mut() {
                    if *m == Some(v) {
                        *m = None;
                    }
                }
                self.map[i] = Some(v);
                Ok(())
            }
            _ => Err("usage: mount | mount fsN L: | mount -d L:".to_string()),
        }
    }

    fn drv(s: &str) -> Result<usize, String> {
        let b = s.as_bytes();
        if !b.is_empty() && b[0].is_ascii_alphabetic() {
            Ok((b[0].to_ascii_uppercase() - b'A') as usize)
        } else {
            Err("bad drive letter".to_string())
        }
    }

    fn type_cmd(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        let mut last_nl = true;
        if rest.is_empty() {
            if !io.has_input {
                return Err("usage: type <file> [file...]".to_string());
            }
            let d = io.take_input();
            last_nl = d.last().map_or(true, |&b| b == b'\n');
            io.put(&d);
        } else {
            for f in rest {
                for hit in self.expand(f).map_err(|e| format!("{f}: {e}"))? {
                    let d = self.read_file(&hit).map_err(|e| format!("{hit}: {e}"))?;
                    last_nl = d.last().map_or(true, |&b| b == b'\n');
                    io.put(&d);
                }
            }
        }
        if io.direct && !last_nl {
            io.put(b"\n");
        }
        Ok(())
    }

    fn find(&self, rest: &[String], io: &mut Io) -> Result<(), String> {
        let (mut ci, mut inv) = (false, false);
        let mut pat: Option<&String> = None;
        let mut files: Vec<&String> = Vec::new();
        for a in rest {
            match a.to_ascii_lowercase().as_str() {
                "/i" => ci = true,
                "/v" => inv = true,
                _ => {
                    if pat.is_none() {
                        pat = Some(a)
                    } else {
                        files.push(a)
                    }
                }
            }
        }
        let pat = pat.ok_or("usage: find [/i] [/v] <text> [files]")?;
        let needle = if ci { pat.to_lowercase() } else { pat.clone() };
        let mut srcs: Vec<Vec<u8>> = Vec::new();
        if files.is_empty() {
            srcs.push(io.take_input());
        } else {
            for f in &files {
                for hit in self.expand(f).map_err(|e| format!("{f}: {e}"))? {
                    srcs.push(self.read_file(&hit).map_err(|e| format!("{hit}: {e}"))?);
                }
            }
        }
        for d in srcs {
            let t = String::from_utf8_lossy(&d);
            for line in t.lines() {
                let hay = if ci { line.to_lowercase() } else { line.to_string() };
                if hay.contains(needle.as_str()) != inv {
                    wl!(io, "{line}");
                }
            }
        }
        Ok(())
    }

    fn run_bat(&mut self, d: usize, p: &str, io: &mut Io) -> Result<(), String> {
        if self.bat_depth >= 8 {
            return Err("batch files nested too deep".to_string());
        }
        let data = self.with_fs(d, |fs| fs.read(pb(p)?).map_err(es))?;
        let text = String::from_utf8_lossy(&data);
        self.bat_depth += 1;
        for raw in text.lines() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with("::") {
                continue;
            }
            let low = line.to_ascii_lowercase();
            if low == "rem" || low.starts_with("rem ") {
                continue;
            }
            if io.direct {
                println!("C:\\> {line}");
            }
            self.run_line(line);
            if self.quit {
                break;
            }
        }
        self.bat_depth -= 1;
        Ok(())
    }

    fn locate(&self, name: &str) -> Option<(usize, String)> {
        let (d, p) = self.resolve(name);
        let last = name.rsplit(|c| c == '\\' || c == '/').next().unwrap_or("");
        let has_ext = last.contains('.');
        let mut cands: Vec<(usize, String)> = Vec::new();
        let mut add = |d: usize, p: String| {
            if has_ext {
                cands.push((d, p));
            } else {
                cands.push((d, format!("{p}.com")));
                cands.push((d, format!("{p}.efi")));
                cands.push((d, format!("{p}.bat")));
            }
        };
        add(d, p);
        if !name.contains(':') && !name.contains('\\') && !name.contains('/') && d != self.boot {
            add(self.boot, format!("\\{name}"));
        }
        for (d, p) in cands {
            if self.with_fs(d, |fs| Ok(Self::is_file(fs, &p))).unwrap_or(false) {
                return Some((d, p));
            }
        }
        None
    }

    fn start(&mut self, a: &[String], io: &mut Io) -> Result<(), String> {
        let name = a.first().ok_or("usage: start <file.com|file.efi|file.bat> [args]")?;
        let (d, p) = self.locate(name).ok_or_else(|| format!("{name}: not found"))?;
        let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "bat" => return self.run_bat(d, &p, io),
            "com" | "efi" => {}
            _ => return Err("unsupported file type (use .com, .efi or .bat)".to_string()),
        }
        let data = self.with_fs(d, |fs| fs.read(pb(&p)?).map_err(es))?;
        let rc = match ext.as_str() {
            "com" => run_com(&data, &a[1..].join(" "), io)?,
            _ => run_efi(&data, &a.join(" "))?,
        };
        if rc != 0 {
            println!("[exit {rc}]");
        }
        Ok(())
    }

    fn exec(&mut self, a: &[String], io: &mut Io) -> Result<(), String> {
        let cmd = a[0].to_ascii_lowercase();
        let rest = &a[1..];
        let arg0 = rest.first().map(|s| s.as_str()).unwrap_or("");
        if cmd.len() == 2 && cmd.ends_with(':') && rest.is_empty() {
            return match Self::drv(&cmd) {
                Ok(i) if self.map[i].is_some() => {
                    self.cur = i;
                    Ok(())
                }
                _ => Err("drive not available".to_string()),
            };
        }
        match cmd.as_str() {
            "dir" | "ls" => self.dir(arg0, io),
            "cd" | "chdir" => self.cd(&rest.join(" "), io),
            "del" | "rm" | "erase" => self.del(rest),
            "md" | "mkdir" => self.md(arg0),
            "rd" | "rmdir" => self.rd(rest),
            "copy" | "cp" => self.copy(rest, io),
            "xcopy" => self.xcopy(rest, io),
            "ren" | "rename" => self.ren(rest, io),
            "move" | "mv" => self.move_cmd(rest, io),
            "mount" => self.mount(rest, io),
            "disk-list" => {
                disk_list(io);
                Ok(())
            }
            "diskmgr" => {
                diskmgr();
                Ok(())
            }
            "echo" => {
                wl!(io, "{}", rest.join(" "));
                Ok(())
            }
            "type" | "cat" => self.type_cmd(rest, io),
            "find" => self.find(rest, io),
            "start" => self.start(rest, io),
            "kbl" => self.kbl(rest, io),
            "edit" | "nano" | "notepad" | "ne" => edit(&*self, rest.first().map(|s| s.as_str())),
            "cls" | "clear" => {
                cls();
                Ok(())
            }
            "help" | "?" => {
                wl!(io, "dir [path|pattern]      list directory (wildcards *.* *.txt ...)");
                wl!(io, "cd [path]               change directory (L: switches drive)");
                wl!(io, "del <file>...           delete files (wildcards * ? ok)");
                wl!(io, "md <dir>                create directory");
                wl!(io, "rd [/s] <dir>           remove directory (/s recursive)");
                wl!(io, "copy <src> <dst>        copy file(s); wildcards ok, dst dir for many");
                wl!(io, "xcopy <src> <dst> [/e]  copy directories recursively (/e keeps empty dirs)");
                wl!(io, "ren <old> <new>         rename (same drive)");
                wl!(io, "move <src> <dst>        move file/dir (rename, or copy+delete cross-volume)");
                wl!(io, "echo [text]             print text");
                wl!(io, "type [file...]          print files (wildcards ok) or stdin");
                wl!(io, "find [/i] [/v] <txt> [file...]   filter lines");
                wl!(io, "start <prog> [args]     run .com (64-bit flat) / .efi / .bat script;");
                wl!(io, "                        bare names work too");
                wl!(io, "edit [file]             nano-like editor (alias: nano, notepad)");
                wl!(io, "kbl [file]              load xkb-style keyboard layout;");
                wl!(io, "                        'kbl default' resets, 'kbl test' shows key codes");
                wl!(io, "                        (\\EFICOMMAND.kbl auto-loads from boot drive)");
                wl!(io, "mount                   rescan + list; mount fsN L: ; mount -d L:");
                wl!(io, "disk-list               list block devices");
                wl!(io, "diskmgr                 disk manager (gpt/new/del partitions)");
                wl!(io, "cls | reboot | shutdown | exit");
                wl!(io, "operators: a ; b (always)   a && b (b if a ok)   a & b (b, quiet)");
                wl!(io, "           a | b   a > f   a >> f   a < f   \"quoted args\"");
                Ok(())
            }
            "reboot" => uefi::runtime::reset(ResetType::COLD, Status::SUCCESS, None),
            "shutdown" => uefi::runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None),
            "exit" => {
                self.quit = true;
                Ok(())
            }
            _ => {
                if self.locate(&a[0]).is_some() {
                    self.start(a, io)
                } else {
                    Err(format!("'{}' is not recognized", a[0]))
                }
            }
        }
    }

    fn run_stages(&mut self, stages: &[Stage], quiet: bool) -> bool {
        let n = stages.len();
        let mut ok = true;
        let mut carry: Option<Vec<u8>> = None;
        for (i, st) in stages.iter().enumerate() {
            let direct = i + 1 == n && st.outfile.is_none();
            let mut io = Io::new(direct);
            if let Some(c) = carry.take() {
                io.input = c;
                io.has_input = true;
            }
            if let Some(f) = &st.infile {
                match self.read_file(f) {
                    Ok(d) => {
                        io.input = d;
                        io.has_input = true;
                    }
                    Err(e) => {
                        if !quiet {
                            println!("error: {f}: {e}");
                        }
                        return false;
                    }
                }
            }
            if let Err(e) = self.exec(&st.args, &mut io) {
                if !quiet {
                    println!("error: {e}");
                }
                ok = false;
            }
            if self.quit {
                return ok;
            }
            if let Some((f, app)) = &st.outfile {
                if let Err(e) = self.write_file(f, &io.out, *app) {
                    if !quiet {
                        println!("error: {f}: {e}");
                    }
                    ok = false;
                }
            } else if i + 1 < n {
                carry = Some(core::mem::take(&mut io.out));
            }
        }
        ok
    }

    fn run_line(&mut self, line: &str) {
        let toks = match tokenize(line) {
            Ok(t) => t,
            Err(e) => {
                println!("error: {e}");
                return;
            }
        };
        let groups = match parse(toks) {
            Ok(g) => g,
            Err(e) => {
                println!("error: {e}");
                return;
            }
        };
        let mut ok = true;
        for (i, (conn, stages)) in groups.iter().enumerate() {
            if i > 0 && *conn == Join::And && !ok {
                continue;
            }
            let quiet = i > 0 && *conn == Join::Amp;
            ok = self.run_stages(stages, quiet);
            if self.quit {
                return;
            }
        }
    }
}

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();
    let _ = boot::set_watchdog_timer(0, 0x10000, None);
    cls();
    println!("EFICOMMAND v0.5 - type 'help'\n");
    let mut sh = Shell::new();
    for cand in ["EFICOMMAND.kbl", "kbl.kbl"] {
        let p = format!("{}:\\{}", letter(sh.boot), cand);
        if sh.load_kbl(&p).is_ok() {
            println!("keyboard layout: {}", cur_kbl_name());
            break;
        }
    }
    while !sh.quit {
        print!("{}", sh.prompt());
        let l = read_line();
        sh.run_line(&l);
    }
    Status::SUCCESS
}






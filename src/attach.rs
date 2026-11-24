//! Files attached to a Claude message: images and PDFs go as they are, everything else
//! (.docx, .xlsx, .csv, .txt, .md, any other text file) is turned into text.

use crate::claude::Attachment;
use base64::Engine;
use eframe::egui;
use quick_xml::events::{BytesStart, Event};
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];
pub const DOC_EXTS: &[&str] = &["pdf", "docx", "xlsx", "xlsm", "csv", "tsv", "txt", "md", "markdown", "log", "json", "xml", "yaml", "yml"];

/// The API takes 32 MB per request; base64 adds a third.
const MAX_PDF_BYTES: usize = 20_000_000;
/// Roughly 100k tokens of extracted text per file.
const MAX_TEXT_CHARS: usize = 400_000;

pub fn ext_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

/// Dropping a file with this extension on the Claude tab attaches it.
pub fn is_attachable(path: &Path) -> bool {
    let ext = ext_of(path);
    IMAGE_EXTS.contains(&ext.as_str()) || DOC_EXTS.contains(&ext.as_str())
}

/// Reads `path` into something that can go in a message.
pub fn load(ctx: &egui::Context, path: &Path) -> Result<Attachment, String> {
    let name = path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    let ext = ext_of(path);
    if IMAGE_EXTS.contains(&ext.as_str()) {
        return crate::claude::prepare_image(ctx, &name, bytes);
    }
    let text = match ext.as_str() {
        "pdf" => return pdf(&name, bytes),
        "docx" => docx(&bytes),
        "xlsx" | "xlsm" => xlsx(&bytes),
        _ => decode_text(&bytes).ok_or_else(|| "not a text file".to_owned()),
    }
    .map_err(|e| format!("{name}: {e}"))?;
    Ok(text_attachment(&name, &text))
}

fn pdf(name: &str, bytes: Vec<u8>) -> Result<Attachment, String> {
    if !bytes.starts_with(b"%PDF") {
        return Err(format!("{name}: not a PDF"));
    }
    if bytes.len() > MAX_PDF_BYTES {
        return Err(format!("{name}: PDF is over {} MB", MAX_PDF_BYTES / 1_000_000));
    }
    Ok(Attachment {
        name: name.to_owned(),
        block: json!({
            "type": "document",
            "source": {"type": "base64", "media_type": "application/pdf", "data": base64::engine::general_purpose::STANDARD.encode(bytes)},
            "title": name,
        }),
        thumb: None,
        full: None,
    })
}

/// A text block wrapped in a tag; blocks starting with `<` stay out of the restored history.
fn text_attachment(name: &str, text: &str) -> Attachment {
    let mut body = text.trim_end().to_owned();
    if body.chars().count() > MAX_TEXT_CHARS {
        body = body.chars().take(MAX_TEXT_CHARS).collect();
        body.push_str("\n[… truncated]");
    }
    let tag_name = name.replace('"', "'");
    Attachment {
        name: name.to_owned(),
        block: json!({"type": "text", "text": format!("<attached_file name=\"{tag_name}\">\n{body}\n</attached_file>")}),
        thumb: None,
        full: None,
    }
}

/// UTF-8 or UTF-16 with a BOM, else the system ANSI code page (CSV exports from Excel).
/// `None` for binary files.
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Some(String::from_utf8_lossy(b).into_owned());
    }
    let utf16 = |b: &[u8], le: bool| {
        let units: Vec<u16> = b.chunks_exact(2).map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) }).collect();
        String::from_utf16_lossy(&units)
    };
    if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return Some(utf16(b, true));
    }
    if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return Some(utf16(b, false));
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Some(s.to_owned()),
        Err(_) => Some(ansi_to_string(bytes)),
    }
}

#[cfg(windows)]
fn ansi_to_string(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_ACP, MultiByteToWideChar};
    let Ok(len) = i32::try_from(bytes.len()) else { return String::from_utf8_lossy(bytes).into_owned() };
    unsafe {
        let n = MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; n as usize];
        MultiByteToWideChar(CP_ACP, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), n);
        String::from_utf16_lossy(&wide)
    }
}

#[cfg(not(windows))]
fn ansi_to_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

// ---------------------------------------------------------------- office files

type Zip<'a> = zip::ZipArchive<std::io::Cursor<&'a [u8]>>;

fn open_zip(bytes: &[u8]) -> Result<Zip<'_>, String> {
    zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|_| "not an Office Open XML file (old .doc / .xls are not supported)".to_owned())
}

fn zip_text(z: &mut Zip, path: &str) -> Option<String> {
    let mut f = z.by_name(path).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

enum Xml<'a> {
    Open(&'a BytesStart<'a>),
    Close(&'a [u8]),
    Text(&'a str),
}

/// Calls `f` for every element and piece of text; self-closing elements give Open + Close.
fn walk_xml(xml: &str, mut f: impl FnMut(Xml)) -> Result<(), String> {
    let mut r = quick_xml::Reader::from_str(xml);
    loop {
        match r.read_event().map_err(|e| e.to_string())? {
            Event::Start(e) => f(Xml::Open(&e)),
            Event::Empty(e) => {
                f(Xml::Open(&e));
                f(Xml::Close(e.local_name().as_ref()));
            }
            Event::End(e) => f(Xml::Close(e.local_name().as_ref())),
            Event::Text(t) => f(Xml::Text(&t.xml10_content().map_err(|e| e.to_string())?)),
            Event::CData(t) => f(Xml::Text(&t.decode().map_err(|e| e.to_string())?)),
            Event::GeneralRef(g) => {
                let s = match g.resolve_char_ref() {
                    Ok(Some(c)) => c.to_string(),
                    _ => g.decode().ok().and_then(|n| quick_xml::escape::resolve_predefined_entity(&n)).unwrap_or("").to_owned(),
                };
                f(Xml::Text(&s));
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn attr(e: &BytesStart, name: &str) -> Option<String> {
    e.try_get_attribute(name).ok().flatten().and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

/// Body text of a Word document: headings as `#`, list items as `-`, tables as `| a | b |` rows.
fn docx(bytes: &[u8]) -> Result<String, String> {
    let mut z = open_zip(bytes)?;
    let xml = zip_text(&mut z, "word/document.xml").ok_or("no word/document.xml inside")?;
    let mut out = String::new();
    let (mut in_t, mut in_tabs, mut cells) = (false, false, 0usize);
    walk_xml(&xml, |x| match x {
        Xml::Open(e) => match e.local_name().as_ref() {
            b"t" => in_t = true,
            b"tabs" => in_tabs = true,
            b"tab" if !in_tabs => out.push('\t'),
            b"br" | b"cr" => out.push('\n'),
            b"tr" => out.push('|'),
            b"tc" => {
                cells += 1;
                out.push(' ');
            }
            b"pStyle" => {
                let level = attr(e, "w:val")
                    .and_then(|v| v.strip_prefix("Heading").and_then(|n| n.parse::<usize>().ok()))
                    .filter(|n| (1..=6).contains(n));
                if let Some(n) = level {
                    out.push_str(&"#".repeat(n));
                    out.push(' ');
                }
            }
            b"numPr" if cells == 0 => out.push_str("- "),
            _ => {}
        },
        Xml::Close(name) => match name {
            b"t" => in_t = false,
            b"tabs" => in_tabs = false,
            // paragraphs inside a table cell run together on its row
            b"p" if cells > 0 => out.push(' '),
            b"p" => out.push('\n'),
            b"tc" => {
                cells -= 1;
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(" |");
            }
            b"tr" => out.push('\n'),
            b"tbl" => out.push('\n'),
            _ => {}
        },
        Xml::Text(t) if in_t => out.push_str(t),
        Xml::Text(_) => {}
    })?;
    Ok(squeeze_blank_lines(&out))
}

fn squeeze_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Every sheet of a workbook as CSV under a `## Sheet: name` line. Formulas give their cached values.
fn xlsx(bytes: &[u8]) -> Result<String, String> {
    let mut z = open_zip(bytes)?;
    let shared = zip_text(&mut z, "xl/sharedStrings.xml").map(|x| shared_strings(&x)).transpose()?.unwrap_or_default();
    let workbook = zip_text(&mut z, "xl/workbook.xml").ok_or("no xl/workbook.xml inside")?;
    let rels = zip_text(&mut z, "xl/_rels/workbook.xml.rels").unwrap_or_default();
    let mut targets = HashMap::new();
    walk_xml(&rels, |x| {
        if let Xml::Open(e) = x {
            if let (b"Relationship", Some(id), Some(t)) = (e.local_name().as_ref(), attr(e, "Id"), attr(e, "Target")) {
                targets.insert(id, t);
            }
        }
    })?;
    let mut sheets = vec![];
    walk_xml(&workbook, |x| {
        if let Xml::Open(e) = x {
            if e.local_name().as_ref() == b"sheet" {
                let name = attr(e, "name").unwrap_or_default();
                let target = attr(e, "r:id").and_then(|id| targets.get(&id).cloned());
                sheets.push((name, target));
            }
        }
    })?;
    let mut out = String::new();
    for (i, (name, target)) in sheets.iter().enumerate() {
        let path = match target {
            Some(t) if t.starts_with('/') => t.trim_start_matches('/').to_owned(),
            Some(t) => format!("xl/{t}"),
            None => format!("xl/worksheets/sheet{}.xml", i + 1),
        };
        // chart sheets and dialog sheets have no cells
        let Some(xml) = zip_text(&mut z, &path) else { continue };
        let rows = sheet_rows(&xml, &shared)?;
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&format!("## Sheet: {name}\n"));
        if rows.is_empty() {
            out.push_str("(empty)");
        }
        for row in rows {
            out.push_str(&row.iter().map(|c| csv_field(c)).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
    }
    Ok(out)
}

fn shared_strings(xml: &str) -> Result<Vec<String>, String> {
    let mut v = vec![];
    let (mut cur, mut in_t, mut in_phonetic) = (String::new(), false, false);
    walk_xml(xml, |x| match x {
        Xml::Open(e) => match e.local_name().as_ref() {
            b"si" => cur.clear(),
            b"t" => in_t = true,
            b"rPh" => in_phonetic = true,
            _ => {}
        },
        Xml::Close(name) => match name {
            b"si" => v.push(std::mem::take(&mut cur)),
            b"t" => in_t = false,
            b"rPh" => in_phonetic = false,
            _ => {}
        },
        Xml::Text(t) if in_t && !in_phonetic => cur.push_str(t),
        Xml::Text(_) => {}
    })?;
    Ok(v)
}

/// Rows of cell values, padded so every column lines up; trailing empty cells are dropped.
fn sheet_rows(xml: &str, shared: &[String]) -> Result<Vec<Vec<String>>, String> {
    let mut rows: Vec<Vec<String>> = vec![];
    let mut row: Vec<String> = vec![];
    let (mut col, mut kind, mut val, mut in_val) = (0usize, String::new(), String::new(), false);
    walk_xml(xml, |x| match x {
        Xml::Open(e) => match e.local_name().as_ref() {
            b"row" => row.clear(),
            b"c" => {
                col = attr(e, "r").and_then(|r| column_index(&r)).unwrap_or(row.len());
                kind = attr(e, "t").unwrap_or_default();
                val.clear();
            }
            b"v" | b"t" => in_val = true,
            _ => {}
        },
        Xml::Close(name) => match name {
            b"v" | b"t" => in_val = false,
            b"c" => {
                let text = match kind.as_str() {
                    "s" => val.trim().parse::<usize>().ok().and_then(|i| shared.get(i).cloned()).unwrap_or_default(),
                    "b" => if val.trim() == "1" { "TRUE" } else { "FALSE" }.to_owned(),
                    _ => std::mem::take(&mut val),
                };
                if !text.is_empty() {
                    if row.len() <= col {
                        row.resize(col + 1, String::new());
                    }
                    row[col] = text;
                }
            }
            b"row" => rows.push(std::mem::take(&mut row)),
            _ => {}
        },
        Xml::Text(t) if in_val => val.push_str(t),
        Xml::Text(_) => {}
    })?;
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    Ok(rows)
}

/// "C7" -> 2
fn column_index(cell_ref: &str) -> Option<usize> {
    let letters: Vec<u8> = cell_ref.bytes().take_while(|b| b.is_ascii_alphabetic()).collect();
    if letters.is_empty() {
        return None;
    }
    Some(letters.iter().fold(0usize, |n, b| n * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize) - 1)
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns() {
        assert_eq!(column_index("A1"), Some(0));
        assert_eq!(column_index("C7"), Some(2));
        assert_eq!(column_index("AA10"), Some(26));
        assert_eq!(column_index("12"), None);
    }

    #[test]
    fn text_encodings() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi").as_deref(), Some("hi"));
        assert_eq!(decode_text(b"\xFF\xFEh\0i\0").as_deref(), Some("hi"));
        assert_eq!(decode_text(b"a\0b"), None);
    }

    #[test]
    fn sheet() {
        let xml = r#"<worksheet><sheetData>
            <row r="1"><c r="A1" t="s"><v>0</v></c><c r="C1" t="inlineStr"><is><t>x &amp; y</t></is></c></row>
            <row r="2"><c r="B2"><v>3.5</v></c><c r="C2" t="b"><v>1</v></c></row>
        </sheetData></worksheet>"#;
        let rows = sheet_rows(xml, &["name".to_owned()]).unwrap();
        assert_eq!(rows, vec![vec!["name", "", "x & y"], vec!["", "3.5", "TRUE"]]);
    }

    #[test]
    fn word() {
        let xml = r#"<w:document xmlns:w="w"><w:body>
            <w:p><w:pPr><w:pStyle w:val="Heading1"/><w:tabs><w:tab w:val="left"/></w:tabs></w:pPr><w:r><w:t>Title</w:t></w:r></w:p>
            <w:p><w:r><w:t xml:space="preserve">Hello </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
            <w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl>
        </w:body></w:document>"#;
        let mut buf = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            zw.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zw, xml.as_bytes()).unwrap();
            zw.finish().unwrap();
        }
        assert_eq!(docx(&buf).unwrap(), "# Title\nHello world\n| a | b |");
    }
}

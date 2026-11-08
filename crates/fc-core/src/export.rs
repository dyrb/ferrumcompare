use crate::{Comparison, scan::NONE};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

fn json_string(out: &mut impl Write, value: impl Iterator<Item = u16>) -> io::Result<()> {
    out.write_all(b"\"")?;
    for c in value {
        match c {
            34 => out.write_all(b"\\\"")?,
            92 => out.write_all(b"\\\\")?,
            32..=126 => out.write_all(&[c as u8])?,
            _ => write!(out, "\\u{c:04x}")?,
        }
    }
    out.write_all(b"\"")
}

pub fn json(pair: &Comparison, out: &mut impl Write) -> io::Result<()> {
    let summary = pair.summary();
    out.write_all(b"{\"left\":")?;
    json_string(out, pair.left.root.as_os_str().encode_wide())?;
    out.write_all(b",\"right\":")?;
    json_string(out, pair.right.root.as_os_str().encode_wide())?;
    out.write_all(b",\"filters\":{\"include\":")?;
    json_string(out, pair.scope.include().encode_utf16())?;
    out.write_all(b",\"exclude\":")?;
    json_string(out, pair.scope.exclude().encode_utf16())?;
    out.write_all(b"}")?;
    write!(out, ",\"root_errors\":{{\"left\":{},\"right\":{}}}", pair.left.nodes[0].error, pair.right.nodes[0].error)?;
    write!(
        out,
        ",\"content_checked\":{},\"scope\":\"unnamed-file-data-and-reparse-records\",\"seconds\":{:.6},\"summary\":{{\"files\":{},\"directories\":{},\"different\":{},\"uncertain\":{},\"verified_equal\":{},\"size_time_match\":{}}},\"items\":[",
        pair.contents,
        pair.elapsed.as_secs_f64(),
        summary.files,
        summary.dirs,
        summary.different,
        summary.uncertain,
        summary.verified,
        summary.unverified
    )?;
    for (i, row) in pair.items.iter().enumerate().skip(1) {
        if i != 1 {
            out.write_all(b",")?;
        }
        out.write_all(b"{\"path\":")?;
        let path = if row.left != NONE { pair.left.relative(row.left) } else { pair.right.relative(row.right) };
        json_string(out, path.as_os_str().encode_wide())?;
        write!(out, ",\"state\":\"{}\",\"aggregate\":\"{}\",\"error\":{}", row.own.code(), row.state.code(), row.error)?;
        write!(
            out,
            ",\"content_equal\":{}",
            if row.verified {
                "true"
            } else if row.own.content_differs() {
                "false"
            } else {
                "null"
            }
        )?;
        for (side, tree, id) in [("left", &pair.left, row.left), ("right", &pair.right, row.right)] {
            write!(out, ",\"{side}\":")?;
            if id == NONE {
                out.write_all(b"null")?;
            } else {
                let n = tree.nodes[id as usize];
                out.write_all(b"{\"path\":")?;
                json_string(out, tree.relative(id).as_os_str().encode_wide())?;
                write!(
                    out,
                    ",\"directory\":{},\"reparse\":{},\"size\":{},\"mtime\":{},\"error\":{}}}",
                    n.dir(),
                    n.link(),
                    n.size,
                    n.mtime,
                    n.error
                )?;
            }
        }
        out.write_all(b"}")?;
    }
    out.write_all(b"]}\n")
}

fn csv_field(out: &mut impl Write, value: &str) -> io::Result<()> {
    write!(out, "\"{}\"", value.replace('"', "\"\""))
}

pub fn csv(pair: &Comparison, out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"\xef\xbb\xbfPath,State,Left size,Right size,Left FILETIME,Right FILETIME,Error,Include masks,Exclude masks\r\n")?;
    for row in pair.items.iter().skip(1) {
        let path = if row.left != NONE { pair.left.relative(row.left) } else { pair.right.relative(row.right) };
        csv_field(out, &path.to_string_lossy())?;
        write!(out, ",{}", row.own.code())?;
        for field in 0..2 {
            for (tree, id) in [(&pair.left, row.left), (&pair.right, row.right)] {
                out.write_all(b",")?;
                if id != NONE {
                    let n = tree.nodes[id as usize];
                    write!(out, "{}", if field == 0 { n.size } else { n.mtime })?;
                }
            }
        }
        write!(out, ",{},", row.error)?;
        csv_field(out, pair.scope.include())?;
        out.write_all(b",")?;
        csv_field(out, pair.scope.exclude())?;
        out.write_all(b"\r\n")?;
    }
    Ok(())
}

/// Reports cannot overwrite compared data or an existing destination.
pub fn save(pair: &Comparison, path: &Path, as_json: bool) -> io::Result<()> {
    let absolute = std::path::absolute(path)?;
    let parent = std::fs::canonicalize(absolute.parent().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?)?;
    let target = parent.join(absolute.file_name().ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?);
    if parent.starts_with(&pair.left.root) || parent.starts_with(&pair.right.root) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "report destination is inside a compared folder"));
    }
    let file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target)?;
    let mut out = io::BufWriter::new(file);
    let result = if as_json { json(pair, &mut out) } else { csv(pair, &mut out) }.and_then(|_| out.flush());
    drop(out);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

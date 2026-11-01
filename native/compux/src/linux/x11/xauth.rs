//! The X authority cookie for one display, read from a file compux is handed.
//!
//! x11rb reads `XAUTHORITY` from the environment itself. A helper started before
//! login takes the session's display from the user manager as a value and never
//! writes it into its environment, so the authority comes the same way and is
//! read here, from the path it names: the file format is the one `xauth` writes,
//! entries of a big-endian family and four counted strings (address, display
//! number, method name, cookie).

use std::io::Read;
use std::path::Path;

use x11rb::reexports::x11rb_protocol::xauth::Family;

/// The one method compux sends: what every X server a desktop runs accepts.
const MIT_MAGIC_COOKIE_1: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The largest authority file read; a larger one is not an authority file.
const MAX_AUTHORITY_BYTES: u64 = 1 << 20;

/// The most entries read from one file.
const MAX_ENTRIES: usize = 4096;

/// One entry of an authority file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub family: u16,
    pub address: Vec<u8>,
    pub number: Vec<u8>,
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

/// An authority's method name and its data, as the connection setup sends them.
pub type Cookie = (Vec<u8>, Vec<u8>);

/// The method name and cookie for a display, read from the file at `path`. An
/// absent file, or no entry for this display, is no authority: the connection is
/// tried without one, as Xlib does.
pub fn cookie(path: &Path, peer: (Family, &[u8]), display: u16) -> Result<Option<Cookie>, String> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("the X authority file would not open: {error}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_AUTHORITY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("the X authority file would not read: {error}"))?;
    if bytes.len() as u64 > MAX_AUTHORITY_BYTES {
        return Err("the X authority file is larger than any authority file".to_string());
    }
    let entries = parse(&bytes)?;
    Ok(matching(&entries, peer, display))
}

/// Every entry in an authority file's bytes.
pub fn parse(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        if entries.len() == MAX_ENTRIES {
            return Err(format!(
                "the X authority file has over {MAX_ENTRIES} entries"
            ));
        }
        let (family, after) = take_u16(rest)?;
        let (address, after) = take_counted(after)?;
        let (number, after) = take_counted(after)?;
        let (name, after) = take_counted(after)?;
        let (data, after) = take_counted(after)?;
        entries.push(Entry {
            family,
            address,
            number,
            name,
            data,
        });
        rest = after;
    }
    Ok(entries)
}

/// The first entry that names this peer and display with the one method sent.
pub fn matching(
    entries: &[Entry],
    (family, address): (Family, &[u8]),
    display: u16,
) -> Option<Cookie> {
    let number = display.to_string().into_bytes();
    entries
        .iter()
        .find(|entry| {
            let entry_family = Family::from(entry.family);
            let peer_matches = entry_family == Family::WILD
                || family == Family::WILD
                || (entry_family == family && entry.address == address);
            peer_matches
                && (entry.number.is_empty() || entry.number == number)
                && entry.name == MIT_MAGIC_COOKIE_1
        })
        .map(|entry| (entry.name.clone(), entry.data.clone()))
}

fn take_u16(bytes: &[u8]) -> Result<(u16, &[u8]), String> {
    match bytes {
        [high, low, rest @ ..] => Ok((u16::from_be_bytes([*high, *low]), rest)),
        _ => Err("the X authority file ends inside an entry".to_string()),
    }
}

fn take_counted(bytes: &[u8]) -> Result<(Vec<u8>, &[u8]), String> {
    let (length, rest) = take_u16(bytes)?;
    let length = usize::from(length);
    if rest.len() < length {
        return Err("the X authority file ends inside an entry".to_string());
    }
    let (value, rest) = rest.split_at(length);
    Ok((value.to_vec(), rest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(family: u16, address: &[u8], number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut bytes = family.to_be_bytes().to_vec();
        for field in [address, number, name, data] {
            bytes.extend((field.len() as u16).to_be_bytes());
            bytes.extend(field);
        }
        bytes
    }

    fn file() -> Vec<u8> {
        [
            entry(256, b"otherhost", b"1", MIT_MAGIC_COOKIE_1, b"other"),
            entry(256, b"thishost", b"0", MIT_MAGIC_COOKIE_1, b"zero"),
            entry(256, b"thishost", b"1", b"XDM-AUTHORIZATION-1", b"xdm"),
            entry(256, b"thishost", b"1", MIT_MAGIC_COOKIE_1, b"cookie"),
        ]
        .concat()
    }

    #[test]
    fn the_entry_for_this_host_and_display_with_the_magic_cookie_is_the_one_sent() {
        let entries = parse(&file()).expect("a well-formed file");
        assert_eq!(entries.len(), 4);
        assert_eq!(
            matching(&entries, (Family::LOCAL, b"thishost"), 1),
            Some((MIT_MAGIC_COOKIE_1.to_vec(), b"cookie".to_vec()))
        );
        assert_eq!(
            matching(&entries, (Family::LOCAL, b"thishost"), 0),
            Some((MIT_MAGIC_COOKIE_1.to_vec(), b"zero".to_vec()))
        );
        assert_eq!(matching(&entries, (Family::LOCAL, b"thishost"), 2), None);
        assert_eq!(matching(&entries, (Family::INTERNET, b"thishost"), 1), None);
    }

    // GDM writes one wildcard entry with no display number.
    #[test]
    fn a_wildcard_entry_with_no_number_serves_any_display() {
        let bytes = entry(65535, b"", b"", MIT_MAGIC_COOKIE_1, b"gdm");
        let entries = parse(&bytes).unwrap();
        assert_eq!(
            matching(&entries, (Family::LOCAL, b"anyhost"), 7),
            Some((MIT_MAGIC_COOKIE_1.to_vec(), b"gdm".to_vec()))
        );
    }

    #[test]
    fn a_file_cut_inside_an_entry_is_refused() {
        let mut bytes = file();
        bytes.truncate(bytes.len() - 3);
        assert!(parse(&bytes).is_err());
        assert!(parse(&[1]).is_err());
        assert_eq!(parse(&[]), Ok(Vec::new()));
    }

    #[test]
    fn a_missing_file_is_no_authority_and_a_present_one_is_read_from_its_path() {
        let dir = std::env::temp_dir().join(format!("compux-xauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Xauthority");
        assert_eq!(cookie(&path, (Family::LOCAL, b"thishost"), 1), Ok(None));
        std::fs::write(&path, file()).unwrap();
        assert_eq!(
            cookie(&path, (Family::LOCAL, b"thishost"), 1),
            Ok(Some((MIT_MAGIC_COOKIE_1.to_vec(), b"cookie".to_vec())))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

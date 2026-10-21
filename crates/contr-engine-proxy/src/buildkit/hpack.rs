//! HPACK header block decoding (RFC 7541), which follows the header lists one peer sends the other.

use std::collections::VecDeque;
use std::sync::LazyLock;

/// Dynamic table size before the encoder changes it (`SETTINGS_HEADER_TABLE_SIZE` default, RFC 9113).
const DEFAULT_TABLE_SIZE: usize = 4096;
/// Bytes a dynamic table entry counts beyond its name and value.
const ENTRY_OVERHEAD: usize = 32;

pub(super) type Field = (Vec<u8>, Vec<u8>);

/// Decoder state of one direction of a connection; it must see every header block of that direction in order.
pub(super) struct Decoder {
    /// Newest entry first.
    dynamic: VecDeque<Field>,
    size: usize,
    max_size: usize,
}

impl Decoder {
    pub(super) fn new() -> Self {
        Self {
            dynamic: VecDeque::new(),
            size: 0,
            max_size: DEFAULT_TABLE_SIZE,
        }
    }

    /// The header list of `block`, or `None` when it is malformed; the decoder is unusable after `None`.
    pub(super) fn decode(&mut self, mut block: &[u8]) -> Option<Vec<Field>> {
        let mut fields = Vec::new();
        while let Some(&first) = block.first() {
            if first & 0x80 != 0 {
                let index = integer(&mut block, 7)?;
                fields.push(self.field(index)?);
            } else if first & 0x40 != 0 {
                let field = self.literal(&mut block, 6)?;
                self.insert(field.clone());
                fields.push(field);
            } else if first & 0x20 != 0 {
                self.max_size = integer(&mut block, 5)?;
                self.evict(0);
            } else {
                fields.push(self.literal(&mut block, 4)?);
            }
        }

        Some(fields)
    }

    fn literal(&self, block: &mut &[u8], prefix: u32) -> Option<Field> {
        let name = match integer(block, prefix)? {
            0 => string(block)?,
            index => self.field(index)?.0,
        };

        Some((name, string(block)?))
    }

    fn field(&self, index: usize) -> Option<Field> {
        match index.checked_sub(1)? {
            index if index < STATIC.len() => {
                let (name, value) = STATIC[index];
                Some((name.as_bytes().to_vec(), value.as_bytes().to_vec()))
            }
            index => self.dynamic.get(index - STATIC.len()).cloned(),
        }
    }

    /// Adds `field`, evicting the entries added first; a field larger than the table empties it.
    fn insert(&mut self, field: Field) {
        let size = entry_size(&field);
        self.evict(size);
        if self.size + size <= self.max_size {
            self.size += size;
            self.dynamic.push_front(field);
        }
    }

    fn evict(&mut self, room: usize) {
        while self.size + room > self.max_size {
            let Some(oldest) = self.dynamic.pop_back() else {
                break;
            };
            self.size -= entry_size(&oldest);
        }
    }
}

fn entry_size((name, value): &Field) -> usize {
    name.len() + value.len() + ENTRY_OVERHEAD
}

/// An integer with a `prefix`-bit prefix (RFC 7541, section 5.1).
fn integer(block: &mut &[u8], prefix: u32) -> Option<usize> {
    let (&first, rest) = block.split_first()?;
    *block = rest;
    let max = (1 << prefix) - 1;
    let mut value = usize::from(first) & max;
    if value < max {
        return Some(value);
    }
    for shift in (0..=28).step_by(7) {
        let (&byte, rest) = block.split_first()?;
        *block = rest;
        value = value.checked_add(usize::from(byte & 0x7f) << shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }

    None
}

/// A string literal (RFC 7541, section 5.2).
fn string(block: &mut &[u8]) -> Option<Vec<u8>> {
    let huffman = block.first()? & 0x80 != 0;
    let length = integer(block, 7)?;
    let (bytes, rest) = block.split_at_checked(length)?;
    *block = rest;

    if huffman {
        decode_huffman(bytes)
    } else {
        Some(bytes.to_vec())
    }
}

/// Bit lengths of the Huffman codes of bytes 0 to 255 and of EOS (RFC 7541, appendix B). The code is canonical:
/// codes of one length are consecutive, in symbol order, and shorter codes precede longer ones.
const CODE_LENGTHS: [u8; 257] = [
    13, 23, 28, 28, 28, 28, 28, 28, 28, 24, 30, 28, 28, 30, 28, 28, 28, 28, 28, 28, 28, 28, 30, 28, 28, 28, 28, 28, 28,
    28, 28, 28, 6, 10, 10, 12, 13, 6, 8, 11, 10, 10, 8, 11, 8, 6, 6, 6, 5, 5, 5, 6, 6, 6, 6, 6, 6, 6, 7, 8, 15, 6, 12,
    10, 13, 6, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 8, 7, 8, 13, 19, 13, 14, 6, 15, 5, 6,
    5, 6, 5, 6, 6, 6, 5, 7, 7, 6, 6, 6, 5, 6, 7, 6, 5, 5, 6, 7, 7, 7, 7, 7, 15, 11, 14, 13, 28, 20, 22, 20, 20, 22, 22,
    22, 23, 22, 23, 23, 23, 23, 23, 24, 23, 24, 24, 22, 23, 24, 23, 23, 23, 23, 21, 22, 23, 22, 23, 23, 24, 22, 21, 20,
    22, 22, 23, 23, 21, 23, 22, 22, 24, 21, 22, 23, 23, 21, 21, 22, 21, 23, 22, 23, 23, 20, 22, 22, 22, 23, 22, 22, 23,
    26, 26, 20, 19, 22, 23, 22, 25, 26, 26, 26, 27, 27, 26, 24, 25, 19, 21, 26, 27, 27, 26, 27, 24, 21, 21, 26, 26, 28,
    27, 27, 27, 20, 24, 20, 21, 22, 21, 21, 23, 22, 22, 25, 25, 24, 24, 26, 23, 26, 27, 26, 26, 27, 27, 27, 27, 27, 28,
    27, 27, 27, 27, 27, 26, 30,
];

const LONGEST_CODE: usize = 30;

/// The canonical Huffman code in decoding form.
struct Canonical {
    /// First code of each length.
    first: [u32; LONGEST_CODE + 1],
    count: [u32; LONGEST_CODE + 1],
    /// Position in `symbols` of the first symbol of each length.
    offset: [usize; LONGEST_CODE + 1],
    /// Symbols in code order.
    symbols: Vec<u16>,
}

static CANONICAL: LazyLock<Canonical> = LazyLock::new(|| {
    let mut symbols: Vec<u16> = (0..=256).collect();
    symbols.sort_by_key(|&symbol| (CODE_LENGTHS[usize::from(symbol)], symbol));
    let mut count = [0; LONGEST_CODE + 1];
    for length in CODE_LENGTHS {
        count[usize::from(length)] += 1;
    }
    let mut first = [0; LONGEST_CODE + 1];
    let mut offset = [0; LONGEST_CODE + 1];
    let (mut code, mut position) = (0, 0);
    for length in 1..=LONGEST_CODE {
        first[length] = code;
        offset[length] = position;
        code = (code + count[length]) << 1;
        position += count[length] as usize;
    }

    Canonical {
        first,
        count,
        offset,
        symbols,
    }
});

fn decode_huffman(bytes: &[u8]) -> Option<Vec<u8>> {
    let canonical = &*CANONICAL;
    let mut decoded = Vec::with_capacity(bytes.len() * 8 / 5);
    let (mut code, mut length) = (0u32, 0);
    for byte in bytes {
        for bit in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> bit) & 1);
            length += 1;
            let index = code.checked_sub(*canonical.first.get(length)?);
            if let Some(index) = index.filter(|index| *index < canonical.count[length]) {
                let symbol = canonical.symbols[canonical.offset[length] + index as usize];
                decoded.push(u8::try_from(symbol).ok()?);
                (code, length) = (0, 0);
            }
        }
    }

    // What remains is padding: fewer than 8 bits of the start of EOS, which is all ones.
    (length < 8 && code == (1 << length) - 1).then_some(decoded)
}

/// The static table (RFC 7541, appendix A); index 1 is the first entry.
const STATIC: [(&str, &str); 61] = [
    (":authority", ""),
    (":method", "GET"),
    (":method", "POST"),
    (":path", "/"),
    (":path", "/index.html"),
    (":scheme", "http"),
    (":scheme", "https"),
    (":status", "200"),
    (":status", "204"),
    (":status", "206"),
    (":status", "304"),
    (":status", "400"),
    (":status", "404"),
    (":status", "500"),
    ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"),
    ("accept-language", ""),
    ("accept-ranges", ""),
    ("accept", ""),
    ("access-control-allow-origin", ""),
    ("age", ""),
    ("allow", ""),
    ("authorization", ""),
    ("cache-control", ""),
    ("content-disposition", ""),
    ("content-encoding", ""),
    ("content-language", ""),
    ("content-length", ""),
    ("content-location", ""),
    ("content-range", ""),
    ("content-type", ""),
    ("cookie", ""),
    ("date", ""),
    ("etag", ""),
    ("expect", ""),
    ("expires", ""),
    ("from", ""),
    ("host", ""),
    ("if-match", ""),
    ("if-modified-since", ""),
    ("if-none-match", ""),
    ("if-range", ""),
    ("if-unmodified-since", ""),
    ("last-modified", ""),
    ("link", ""),
    ("location", ""),
    ("max-forwards", ""),
    ("proxy-authenticate", ""),
    ("proxy-authorization", ""),
    ("range", ""),
    ("referer", ""),
    ("refresh", ""),
    ("retry-after", ""),
    ("server", ""),
    ("set-cookie", ""),
    ("strict-transport-security", ""),
    ("transfer-encoding", ""),
    ("user-agent", ""),
    ("vary", ""),
    ("via", ""),
    ("www-authenticate", ""),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).unwrap())
            .collect()
    }

    fn decoded(decoder: &mut Decoder, block: &str) -> Vec<(String, String)> {
        decoder
            .decode(&hex(block))
            .unwrap()
            .into_iter()
            .map(|(name, value)| (String::from_utf8(name).unwrap(), String::from_utf8(value).unwrap()))
            .collect()
    }

    fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    /// RFC 7541, appendix C.4: requests with Huffman coding.
    #[test]
    fn decodes_huffman_coded_requests() {
        let mut decoder = Decoder::new();

        assert_eq!(
            decoded(&mut decoder, "828684418cf1e3c2e5f23a6ba0ab90f4ff"),
            fields(&[
                (":method", "GET"),
                (":scheme", "http"),
                (":path", "/"),
                (":authority", "www.example.com")
            ])
        );
        assert_eq!(decoder.size, 57);
        assert_eq!(
            decoded(&mut decoder, "828684be5886a8eb10649cbf")[4],
            ("cache-control".to_owned(), "no-cache".to_owned())
        );
        assert_eq!(
            decoded(&mut decoder, "828785bf408825a849e95ba97d7f8925a849e95bb8e8b4bf"),
            fields(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/index.html"),
                (":authority", "www.example.com"),
                ("custom-key", "custom-value"),
            ])
        );
        assert_eq!(decoder.size, 164);
    }

    /// RFC 7541, appendix C.5: responses that evict entries from a 256-byte table.
    #[test]
    fn evicts_the_oldest_entries() {
        let mut decoder = Decoder::new();
        decoder.max_size = 256;

        decoded(
            &mut decoder,
            "4803333032580770726976617465611d4d6f6e2c203231204f637420323031332032303a31333a323120474d54\
             6e1768747470733a2f2f7777772e6578616d706c652e636f6d",
        );
        assert_eq!(decoder.size, 222);
        assert_eq!(
            decoded(&mut decoder, "4803333037c1c0bf")[0],
            (":status".to_owned(), "307".to_owned())
        );
        assert_eq!(decoder.size, 222);
        let third = decoded(
            &mut decoder,
            "88c1611d4d6f6e2c203231204f637420323031332032303a31333a323220474d54c05a04677a69707738666f6f\
             3d4153444a4b48514b425a584f5157454f50495541585157454f49553b206d61782d6167653d333630303b2076\
             657273696f6e3d31",
        );
        assert_eq!(third[3], ("location".to_owned(), "https://www.example.com".to_owned()));
        assert_eq!(third[5].1, "foo=ASDJKHQKBZXOQWEOPIUAXQWEOIU; max-age=3600; version=1");
        assert_eq!(decoder.size, 215);
        assert_eq!(decoder.dynamic.len(), 3);
    }

    #[test]
    fn follows_table_size_updates() {
        let mut decoder = Decoder::new();
        decoded(&mut decoder, "400a637573746f6d2d6b65790d637573746f6d2d686561646572");
        assert_eq!(decoder.dynamic.len(), 1);

        assert!(decoded(&mut decoder, "20").is_empty());
        assert_eq!((decoder.size, decoder.dynamic.len()), (0, 0));
    }

    #[test]
    fn refuses_malformed_blocks() {
        for block in ["80", "be", "ff", "4085", "418cf1e3c2e5f23a6ba0ab90f4", "4184ffffffff"] {
            assert_eq!(Decoder::new().decode(&hex(block)), None, "{block}");
        }
    }

    #[test]
    fn decodes_long_integers() {
        let mut block: &[u8] = &[0x1f, 0x9a, 0x0a];
        assert_eq!(integer(&mut block, 5), Some(1337));
        assert!(block.is_empty());
    }
}

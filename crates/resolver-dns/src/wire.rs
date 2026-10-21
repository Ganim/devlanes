//! DNS message parsing and response building (RFC 1035, EDNS(0) from RFC 6891).

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::Zone;

const HEADER_LEN: usize = 12;
const MAX_NAME_LEN: usize = 255;

const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const TYPE_OPT: u16 = 41;
const TYPE_ANY: u16 = 255;
const CLASS_IN: u16 = 1;
const CLASS_ANY: u16 = 255;

const FLAG_QR: u16 = 0x8000;
const FLAG_AA: u16 = 0x0400;
const FLAG_RD: u16 = 0x0100;

const RCODE_NOERROR: u16 = 0;
const RCODE_FORMERR: u16 = 1;
const RCODE_NOTIMP: u16 = 4;
const RCODE_REFUSED: u16 = 5;
/// Extended rcode 16; its upper bits travel in the OPT record.
const RCODE_BADVERS: u16 = 16;

/// UDP payload size advertised in our OPT record, the DNS Flag Day 2020 value.
const EDNS_PAYLOAD: u16 = 1232;

/// Pointer to the question name, which always starts right after the header.
const QUESTION_NAME_POINTER: [u8; 2] = [0xC0, HEADER_LEN as u8];

/// Answers one DNS message for `zone`.
///
/// Returns `None` for messages that must be dropped: shorter than a header, or
/// responses (answering those could loop between two servers).
pub fn answer(message: &[u8], zone: &Zone) -> Option<Vec<u8>> {
    let header = Header::parse(message)?;
    if header.flags & FLAG_QR != 0 {
        return None;
    }
    let opcode = (header.flags >> 11) & 0xF;
    let rd = header.flags & FLAG_RD;
    if opcode != 0 {
        return Some(bare_response(header.id, opcode, rd, RCODE_NOTIMP));
    }
    let Some(query) = Query::parse(message, &header) else {
        return Some(bare_response(header.id, 0, rd, RCODE_FORMERR));
    };
    Some(query.respond(header.id, rd, zone))
}

/// The fixed 12-byte header.
struct Header {
    id: u16,
    flags: u16,
    qdcount: u16,
    ancount: u16,
    nscount: u16,
    arcount: u16,
}

impl Header {
    /// Reads the header, or `None` when the message is too short to answer at all.
    fn parse(message: &[u8]) -> Option<Self> {
        let mut reader = Reader::new(message);
        Some(Self {
            id: reader.u16()?,
            flags: reader.u16()?,
            qdcount: reader.u16()?,
            ancount: reader.u16()?,
            nscount: reader.u16()?,
            arcount: reader.u16()?,
        })
    }
}

/// A parsed standard query.
struct Query<'a> {
    /// Question name exactly as sent, so the echo keeps the client's case (0x20 bits).
    raw_name: &'a [u8],
    labels: Vec<Vec<u8>>,
    qtype: u16,
    qclass: u16,
    /// EDNS version from the OPT record, when present.
    edns_version: Option<u8>,
}

impl<'a> Query<'a> {
    /// Parses a one-question query; `None` means FORMERR.
    fn parse(message: &'a [u8], header: &Header) -> Option<Self> {
        if header.qdcount != 1 || header.ancount != 0 || header.nscount != 0 {
            return None;
        }
        let mut reader = Reader::new(message);
        reader.skip(HEADER_LEN)?;
        let start = reader.pos;
        let labels = reader.question_name()?;
        let raw_name = &message[start..reader.pos];
        let qtype = reader.u16()?;
        let qclass = reader.u16()?;
        let mut edns_version = None;
        for _ in 0..header.arcount {
            let name_start = reader.pos;
            reader.skip_name()?;
            let is_root = reader.pos - name_start == 1;
            let rtype = reader.u16()?;
            let _class = reader.u16()?;
            let ttl = reader.u32()?;
            let rdlength = reader.u16()?;
            reader.skip(usize::from(rdlength))?;
            if rtype == TYPE_OPT {
                // RFC 6891 6.1.1: one OPT, owned by the root name.
                if edns_version.is_some() || !is_root {
                    return None;
                }
                edns_version = Some(ttl.to_be_bytes()[1]);
            }
        }
        Some(Self {
            raw_name,
            labels,
            qtype,
            qclass,
            edns_version,
        })
    }

    /// Builds the response: loopback answers inside the zone, REFUSED outside it.
    fn respond(&self, id: u16, rd: u16, zone: &Zone) -> Vec<u8> {
        let mut rcode = RCODE_NOERROR;
        let mut flags = FLAG_QR | rd;
        let mut answers: Vec<Vec<u8>> = Vec::new();
        if self.edns_version.is_some_and(|version| version > 0) {
            rcode = RCODE_BADVERS;
        } else if !zone.contains_labels(&self.labels)
            || !matches!(self.qclass, CLASS_IN | CLASS_ANY)
        {
            rcode = RCODE_REFUSED;
        } else {
            flags |= FLAG_AA;
            if matches!(self.qtype, TYPE_A | TYPE_ANY) {
                answers.push(record(TYPE_A, zone.ttl(), &Ipv4Addr::LOCALHOST.octets()));
            }
            if matches!(self.qtype, TYPE_AAAA | TYPE_ANY) {
                answers.push(record(TYPE_AAAA, zone.ttl(), &Ipv6Addr::LOCALHOST.octets()));
            }
            // Other types get NOERROR without answers (NODATA): the name exists.
        }
        flags |= rcode & 0xF;

        let mut out = Vec::with_capacity(HEADER_LEN + self.raw_name.len() + 64);
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&flags.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&count(answers.len()).to_be_bytes());
        out.extend_from_slice(&0u16.to_be_bytes());
        out.extend_from_slice(&u16::from(self.edns_version.is_some()).to_be_bytes());
        out.extend_from_slice(self.raw_name);
        out.extend_from_slice(&self.qtype.to_be_bytes());
        out.extend_from_slice(&self.qclass.to_be_bytes());
        for answer in &answers {
            out.extend_from_slice(answer);
        }
        if self.edns_version.is_some() {
            out.push(0);
            out.extend_from_slice(&TYPE_OPT.to_be_bytes());
            out.extend_from_slice(&EDNS_PAYLOAD.to_be_bytes());
            // TTL field: extended rcode, version 0, no flags (DO clear: no DNSSEC here).
            let extended = u32::from(rcode >> 4) << 24;
            out.extend_from_slice(&extended.to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes());
        }
        out
    }
}

/// A response with only a header, for messages too broken to echo.
fn bare_response(id: u16, opcode: u16, rd: u16, rcode: u16) -> Vec<u8> {
    let flags = FLAG_QR | (opcode << 11) | rd | rcode;
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&[0; 8]);
    out
}

/// An answer record owned by the question name.
fn record(rtype: u16, ttl: u32, rdata: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + rdata.len());
    out.extend_from_slice(&QUESTION_NAME_POINTER);
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&CLASS_IN.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&count(rdata.len()).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

/// Section counts and rdata here are tiny; saturate rather than panic if that changes.
fn count(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// Bounds-checked big-endian reader.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Starts at offset 0.
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Takes `n` bytes.
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let bytes = self.buf.get(self.pos..end)?;
        self.pos = end;
        Some(bytes)
    }

    /// Skips `n` bytes.
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(drop)
    }

    /// Reads one byte.
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    /// Reads a big-endian u16.
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    /// Reads a big-endian u32.
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads an uncompressed name into labels.
    ///
    /// Compression in the question of a query is legal but no stub resolver sends
    /// it, and refusing it keeps the raw-name echo trivially correct.
    fn question_name(&mut self) -> Option<Vec<Vec<u8>>> {
        let start = self.pos;
        let mut labels = Vec::new();
        loop {
            let len = self.u8()?;
            if len == 0 {
                break;
            }
            if len & 0xC0 != 0 {
                return None;
            }
            labels.push(self.take(usize::from(len))?.to_ascii_lowercase());
            if self.pos - start > MAX_NAME_LEN {
                return None;
            }
        }
        (self.pos - start <= MAX_NAME_LEN).then_some(labels)
    }

    /// Skips a possibly compressed name in the additional section.
    fn skip_name(&mut self) -> Option<()> {
        let start = self.pos;
        loop {
            let len = self.u8()?;
            match len & 0xC0 {
                0 if len == 0 => return Some(()),
                0 => self.skip(usize::from(len))?,
                // A pointer ends the name; its target was already parsed or is ignored.
                0xC0 => return self.skip(1),
                _ => return None,
            }
            if self.pos - start > MAX_NAME_LEN {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a query for `name` with optional EDNS version.
    fn query(id: u16, name: &str, qtype: u16, edns: Option<u8>) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&FLAG_RD.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&u16::from(edns.is_some()).to_be_bytes());
        for label in name.split('.').filter(|l| !l.is_empty()) {
            out.push(u8::try_from(label.len()).expect("label"));
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        out.extend_from_slice(&qtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        if let Some(version) = edns {
            out.push(0);
            out.extend_from_slice(&TYPE_OPT.to_be_bytes());
            out.extend_from_slice(&4096u16.to_be_bytes());
            out.extend_from_slice(&[0, version, 0x80, 0]);
            out.extend_from_slice(&0u16.to_be_bytes());
        }
        out
    }

    fn zone() -> Zone {
        Zone::new("test").expect("zone")
    }

    fn rcode(response: &[u8]) -> u16 {
        u16::from_be_bytes([response[2], response[3]]) & 0xF
    }

    fn ancount(response: &[u8]) -> u16 {
        u16::from_be_bytes([response[6], response[7]])
    }

    #[test]
    fn answers_a_and_aaaa_in_zone() {
        let q = query(7, "Crm.Lane-2.test", TYPE_A, None);
        let r = answer(&q, &zone()).expect("response");
        assert_eq!(&r[..2], &7u16.to_be_bytes());
        let flags = u16::from_be_bytes([r[2], r[3]]);
        assert_eq!(
            flags & (FLAG_QR | FLAG_AA | FLAG_RD),
            FLAG_QR | FLAG_AA | FLAG_RD
        );
        assert_eq!((rcode(&r), ancount(&r)), (0, 1));
        assert_eq!(&r[12..q.len()], &q[12..], "question echoed with its case");
        assert_eq!(&r[r.len() - 4..], &[127, 0, 0, 1]);

        let r = answer(&query(8, "x.test", TYPE_AAAA, None), &zone()).expect("response");
        assert_eq!(&r[r.len() - 16..], &Ipv6Addr::LOCALHOST.octets());
        let r = answer(&query(9, "x.test", TYPE_ANY, None), &zone()).expect("response");
        assert_eq!(ancount(&r), 2);
    }

    #[test]
    fn other_types_get_nodata_and_other_names_are_refused() {
        let r = answer(&query(1, "x.test", 16, None), &zone()).expect("response");
        assert_eq!((rcode(&r), ancount(&r)), (RCODE_NOERROR, 0));
        let r = answer(&query(1, "example.com", TYPE_A, None), &zone()).expect("response");
        assert_eq!((rcode(&r), ancount(&r)), (RCODE_REFUSED, 0));
        let flags = u16::from_be_bytes([r[2], r[3]]);
        assert_eq!(flags & FLAG_AA, 0);
    }

    #[test]
    fn edns_is_echoed_and_unknown_versions_get_badvers() {
        let q = query(3, "a.test", TYPE_A, Some(0));
        let r = answer(&q, &zone()).expect("response");
        assert_eq!(u16::from_be_bytes([r[10], r[11]]), 1, "OPT in additional");
        let opt = &r[r.len() - 11..];
        assert_eq!(opt[0], 0);
        assert_eq!(u16::from_be_bytes([opt[1], opt[2]]), TYPE_OPT);
        assert_eq!(u16::from_be_bytes([opt[3], opt[4]]), EDNS_PAYLOAD);
        assert_eq!(&opt[5..9], &[0, 0, 0, 0], "DO bit cleared");

        let r = answer(&query(4, "a.test", TYPE_A, Some(1)), &zone()).expect("response");
        assert_eq!((rcode(&r), ancount(&r)), (0, 0));
        let opt = &r[r.len() - 11..];
        assert_eq!(opt[5], 1, "extended rcode BADVERS = 16 >> 4");
    }

    #[test]
    fn malformed_messages() {
        let zone = zone();
        assert!(answer(&[0; 11], &zone).is_none());
        let mut response = query(1, "a.test", TYPE_A, None);
        response[2] |= 0x80;
        assert!(answer(&response, &zone).is_none(), "responses are dropped");

        let mut truncated = query(2, "a.test", TYPE_A, None);
        truncated.truncate(truncated.len() - 3);
        let r = answer(&truncated, &zone).expect("formerr");
        assert_eq!((rcode(&r), r.len()), (RCODE_FORMERR, HEADER_LEN));

        let mut compressed = query(3, "", TYPE_A, None);
        compressed.splice(12..13, [0xC0, 0x0C]);
        assert_eq!(
            rcode(&answer(&compressed, &zone).expect("formerr")),
            RCODE_FORMERR
        );

        let mut two_questions = query(4, "a.test", TYPE_A, None);
        two_questions[5] = 2;
        assert_eq!(
            rcode(&answer(&two_questions, &zone).expect("formerr")),
            RCODE_FORMERR
        );

        let mut notify = query(5, "a.test", TYPE_A, None);
        notify[2] |= 4 << 3;
        let r = answer(&notify, &zone).expect("notimp");
        assert_eq!(rcode(&r), RCODE_NOTIMP);
        assert_eq!((u16::from_be_bytes([r[2], r[3]]) >> 11) & 0xF, 4);

        let long = vec!["a".repeat(63); 4].join(".");
        assert_eq!(
            rcode(&answer(&query(6, &long, TYPE_A, None), &zone).expect("formerr")),
            RCODE_FORMERR
        );

        let dotted_label = {
            let mut q = query(7, "", TYPE_A, None);
            q.splice(12..13, *b"\x06a.test\x00");
            q
        };
        assert_eq!(
            rcode(&answer(&dotted_label, &zone).expect("refused")),
            RCODE_REFUSED,
            "a label containing a dot is not under the zone"
        );
    }
}

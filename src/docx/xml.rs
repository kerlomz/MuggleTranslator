use std::collections::BTreeMap;

use anyhow::{anyhow, Context};
use quick_xml::events::{BytesDecl, BytesStart, Event};
use quick_xml::Reader;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub enum XmlEvent {
    Decl {
        version: String,
        encoding: Option<String>,
        standalone: Option<String>,
    },
    Start {
        name: String,
        attrs: Vec<(String, String)>,
    },
    End {
        name: String,
    },
    Empty {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text {
        text: String,
    },
    CData {
        text: String,
    },
    Comment {
        text: String,
    },
    PI {
        content: String,
    },
    DocType {
        text: String,
    },
}

#[derive(Clone)]
pub struct XmlPart {
    pub name: String,
    pub events: Vec<XmlEvent>,
    pub baseline_hash: String,
}

pub fn parse_xml_part(name: &str, xml_bytes: &[u8]) -> anyhow::Result<XmlPart> {
    let mut reader = Reader::from_reader(xml_bytes);
    reader.config_mut().trim_text(false);

    let mut events: Vec<XmlEvent> = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let ev = reader.read_event_into(&mut buf).context("read xml event")?;
        match ev {
            Event::Eof => break,
            Event::Decl(d) => {
                let version = bytes_to_string(d.version().context("decl version")?);
                let encoding = d
                    .encoding()
                    .map(|r| r.map(bytes_to_string))
                    .transpose()
                    .unwrap_or(None);
                let standalone = d
                    .standalone()
                    .map(|r| r.map(bytes_to_string))
                    .transpose()
                    .unwrap_or(None);
                events.push(XmlEvent::Decl {
                    version,
                    encoding,
                    standalone,
                });
            }
            Event::Start(s) => {
                events.push(XmlEvent::Start {
                    name: bytes_to_string(s.name().as_ref()),
                    attrs: collect_attrs(&s)?,
                });
            }
            Event::End(e) => {
                events.push(XmlEvent::End {
                    name: bytes_to_string(e.name().as_ref()),
                });
            }
            Event::Empty(s) => {
                events.push(XmlEvent::Empty {
                    name: bytes_to_string(s.name().as_ref()),
                    attrs: collect_attrs(&s)?,
                });
            }
            Event::Text(t) => {
                let txt = t.unescape().context("unescape text")?.into_owned();
                events.push(XmlEvent::Text { text: txt });
            }
            Event::CData(t) => {
                let txt = bytes_to_string(t.into_inner());
                events.push(XmlEvent::CData { text: txt });
            }
            Event::Comment(t) => {
                let txt = bytes_to_string(t.into_inner());
                events.push(XmlEvent::Comment { text: txt });
            }
            Event::PI(t) => {
                let target = bytes_to_string(t.target());
                let content = bytes_to_string(t.content());
                events.push(XmlEvent::PI {
                    content: format!("{target}{content}"),
                });
            }
            Event::DocType(t) => {
                let txt = bytes_to_string(t.into_inner());
                events.push(XmlEvent::DocType { text: txt });
            }
        }
    }

    let baseline_hash = structure_hash(&events);
    Ok(XmlPart {
        name: name.to_string(),
        events,
        baseline_hash,
    })
}

fn collect_attrs(s: &BytesStart<'_>) -> anyhow::Result<Vec<(String, String)>> {
    let mut attrs: Vec<(String, String)> = Vec::new();
    for a in s.attributes() {
        let a = a.context("attr")?;
        let key = bytes_to_string(a.key.as_ref());
        // Keep raw (already-escaped) attribute bytes.
        // This is required for lossless round-trip of attribute values such as `o:gfxdata`
        // (VML), which encodes CRLF using character references (e.g. `&#13;&#10;`). If we
        // unescape those references into literal newlines and then write them back, XML
        // normalization will change the value (newlines in attribute values become spaces),
        // corrupting embedded objects.
        let val = bytes_to_string(a.value.as_ref());
        attrs.push((key, val));
    }
    Ok(attrs)
}

fn bytes_to_string(bytes: impl AsRef<[u8]>) -> String {
    String::from_utf8_lossy(bytes.as_ref()).into_owned()
}

pub fn write_xml_part(part: &XmlPart) -> anyhow::Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();

    fn escape_text_into(out: &mut Vec<u8>, text: &str) {
        for ch in text.chars() {
            match ch {
                '&' => out.extend_from_slice(b"&amp;"),
                '<' => out.extend_from_slice(b"&lt;"),
                '>' => out.extend_from_slice(b"&gt;"),
                _ => {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
    }

    fn write_start_like(out: &mut Vec<u8>, name: &str, attrs: &[(String, String)], empty: bool) {
        out.extend_from_slice(b"<");
        out.extend_from_slice(name.as_bytes());
        // Attribute values are stored as raw (already-escaped) XML bytes. Do NOT escape again.
        for (k, v) in attrs {
            out.extend_from_slice(b" ");
            out.extend_from_slice(k.as_bytes());
            out.extend_from_slice(b"=\"");
            out.extend_from_slice(v.as_bytes());
            out.extend_from_slice(b"\"");
        }
        if empty {
            out.extend_from_slice(b"/>");
        } else {
            out.extend_from_slice(b">");
        }
    }

    for ev in &part.events {
        match ev {
            XmlEvent::Decl {
                version,
                encoding,
                standalone,
            } => {
                let d =
                    BytesDecl::new(version.as_str(), encoding.as_deref(), standalone.as_deref());
                let mut writer = quick_xml::Writer::new(Vec::new());
                writer.write_event(Event::Decl(d)).context("write decl")?;
                out.extend_from_slice(&writer.into_inner());
            }
            XmlEvent::Start { name, attrs } => {
                write_start_like(&mut out, name, attrs, false);
            }
            XmlEvent::End { name } => {
                out.extend_from_slice(b"</");
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b">");
            }
            XmlEvent::Empty { name, attrs } => {
                write_start_like(&mut out, name, attrs, true);
            }
            XmlEvent::Text { text } => {
                escape_text_into(&mut out, text);
            }
            XmlEvent::CData { text } => {
                // CDATA must remain unescaped.
                out.extend_from_slice(b"<![CDATA[");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b"]]>");
            }
            XmlEvent::Comment { text } => {
                out.extend_from_slice(b"<!--");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b"-->");
            }
            XmlEvent::PI { content } => {
                out.extend_from_slice(b"<?");
                out.extend_from_slice(content.as_bytes());
                out.extend_from_slice(b"?>");
            }
            XmlEvent::DocType { text } => {
                out.extend_from_slice(b"<!DOCTYPE");
                out.extend_from_slice(text.as_bytes());
                out.extend_from_slice(b">");
            }
        }
    }

    Ok(out)
}

pub fn verify_structure_unchanged(part: &XmlPart) -> anyhow::Result<()> {
    let cur = structure_hash(&part.events);
    if cur != part.baseline_hash {
        return Err(anyhow!(
            "non-text structure changed in {} (baseline={} current={})",
            part.name,
            part.baseline_hash,
            cur
        ));
    }
    Ok(())
}

fn structure_hash(events: &[XmlEvent]) -> String {
    let mut hasher = Sha256::new();
    let mut stack: Vec<String> = Vec::new();

    for ev in events {
        match ev {
            XmlEvent::Start { name, attrs } => {
                stack.push(name.clone());
                hash_start_like(&mut hasher, name, attrs);
            }
            XmlEvent::Empty { name, attrs } => {
                hash_start_like(&mut hasher, name, attrs);
                hash_end_like(&mut hasher, name);
            }
            XmlEvent::End { name } => {
                hash_end_like(&mut hasher, name);
                let _ = stack.pop();
            }
            XmlEvent::Text { text } => {
                let cur = stack.last().map(|s| s.as_str()).unwrap_or("");
                if is_text_tag(cur) {
                    continue;
                }
                hasher.update(b"T:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::Decl {
                version,
                encoding,
                standalone,
            } => {
                hasher.update(b"D:");
                hasher.update(version.as_bytes());
                hasher.update(b"|");
                if let Some(e) = encoding.as_ref() {
                    hasher.update(e.as_bytes());
                }
                hasher.update(b"|");
                if let Some(s) = standalone.as_ref() {
                    hasher.update(s.as_bytes());
                }
                hasher.update(b"\n");
            }
            XmlEvent::CData { text } => {
                hasher.update(b"C:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::Comment { text } => {
                hasher.update(b"M:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::PI { content } => {
                hasher.update(b"P:");
                hasher.update(content.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::DocType { text } => {
                hasher.update(b"Y:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
        }
    }
    hex::encode(hasher.finalize())
}

pub fn full_hash(events: &[XmlEvent]) -> String {
    let mut hasher = Sha256::new();
    let mut stack: Vec<String> = Vec::new();

    for ev in events {
        match ev {
            XmlEvent::Start { name, attrs } => {
                stack.push(name.clone());
                hash_start_like_full(&mut hasher, name, attrs);
            }
            XmlEvent::Empty { name, attrs } => {
                hash_start_like_full(&mut hasher, name, attrs);
                hash_end_like(&mut hasher, name);
            }
            XmlEvent::End { name } => {
                hash_end_like(&mut hasher, name);
                let _ = stack.pop();
            }
            XmlEvent::Text { text } => {
                let cur = stack.last().map(|s| s.as_str()).unwrap_or("");
                hasher.update(b"T:");
                hasher.update(cur.as_bytes());
                hasher.update(b"|");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::Decl {
                version,
                encoding,
                standalone,
            } => {
                hasher.update(b"D:");
                hasher.update(version.as_bytes());
                hasher.update(b"|");
                if let Some(e) = encoding.as_ref() {
                    hasher.update(e.as_bytes());
                }
                hasher.update(b"|");
                if let Some(s) = standalone.as_ref() {
                    hasher.update(s.as_bytes());
                }
                hasher.update(b"\n");
            }
            XmlEvent::CData { text } => {
                hasher.update(b"C:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::Comment { text } => {
                hasher.update(b"M:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::PI { content } => {
                hasher.update(b"P:");
                hasher.update(content.as_bytes());
                hasher.update(b"\n");
            }
            XmlEvent::DocType { text } => {
                hasher.update(b"Y:");
                hasher.update(text.as_bytes());
                hasher.update(b"\n");
            }
        }
    }
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{parse_xml_part, write_xml_part};

    #[test]
    fn write_preserves_attr_entity_refs() {
        let xml = br#"<?xml version="1.0" encoding="UTF-8"?><root xmlns:o="urn:test" o:gfxdata="A&#xD;&#xA;B"/>"#;
        let part = parse_xml_part("test.xml", xml).expect("parse xml");
        let out = write_xml_part(&part).expect("write xml");
        let s = String::from_utf8(out).expect("utf8");

        assert!(s.contains(r#"o:gfxdata="A&#xD;&#xA;B""#));
        assert!(!s.contains(r#"o:gfxdata="A&amp;#xD;"#));
    }
}

fn is_text_tag(name: &str) -> bool {
    name == "w:t" || name == "a:t" || name == "w:delText"
}

fn hash_start_like(hasher: &mut Sha256, name: &str, attrs: &[(String, String)]) {
    hasher.update(b"S:");
    hasher.update(name.as_bytes());
    hasher.update(b"|");

    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in attrs {
        if k == "xml:space" {
            continue;
        }
        let val = if is_lvltext(name) && k == "w:val" {
            String::new()
        } else {
            v.clone()
        };
        map.insert(k.clone(), val);
    }
    for (k, v) in map {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b";");
    }
    hasher.update(b"\n");
}

fn hash_start_like_full(hasher: &mut Sha256, name: &str, attrs: &[(String, String)]) {
    hasher.update(b"S:");
    hasher.update(name.as_bytes());
    hasher.update(b"|");

    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in attrs {
        map.insert(k.clone(), v.clone());
    }
    for (k, v) in map {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
        hasher.update(v.as_bytes());
        hasher.update(b";");
    }
    hasher.update(b"\n");
}

fn hash_end_like(hasher: &mut Sha256, name: &str) {
    hasher.update(b"E:");
    hasher.update(name.as_bytes());
    hasher.update(b"\n");
}

fn is_lvltext(name: &str) -> bool {
    name == "w:lvlText"
}

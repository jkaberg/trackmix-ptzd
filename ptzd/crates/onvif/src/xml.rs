//! Minimal namespace-aware XML tree for SOAP requests, on quick-xml.
//! DOCTYPE is rejected (no entity tricks), depth and node count are capped.

use quick_xml::events::{BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;

/// Limits are generous for ONVIF requests (a few hundred bytes to 2 KB);
/// they only bound hostile input. Not protocol constants.
const MAX_DEPTH: usize = 24;
const MAX_NODES: usize = 2048;

#[derive(Debug, Default, Clone)]
pub struct Node {
    /// Namespace URI, empty if unbound.
    pub ns: String,
    /// Local name (prefix stripped).
    pub name: String,
    /// Attributes by local name; xmlns declarations are dropped.
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    pub fn text_trim(&self) -> &str {
        self.text.trim()
    }
    /// Trimmed text of a direct child.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text_trim())
    }
}

fn open_node(
    res: ResolveResult<'_>,
    e: &BytesStart<'_>,
) -> Result<Node, String> {
    let ns = match res {
        ResolveResult::Bound(n) => n.into_inner().to_string(),
        _ => String::new(),
    };
    let name = e.local_name().as_ref().to_string();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| format!("bad attribute: {e}"))?;
        let key = a.key.as_ref();
        if key == "xmlns" || key.starts_with("xmlns:") {
            continue;
        }
        let local = key.rsplit(':').next().unwrap_or(key);
        let val = a
            .normalized_value(quick_xml::XmlVersion::Explicit1_0)
            .map_err(|e| format!("bad attribute value: {e}"))?;
        attrs.push((local.to_string(), val.into_owned()));
    }
    Ok(Node { ns, name, attrs, text: String::new(), children: Vec::new() })
}

pub fn parse(xml: &[u8]) -> Result<Node, String> {
    let mut r = NsReader::from_reader(xml);
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut count = 0usize;
    loop {
        let pos = r.buffer_position();
        let (res, ev) = r
            .read_resolved_event()
            .map_err(|e| format!("xml error near byte {pos}: {e}"))?;
        match ev {
            Event::Start(e) => {
                count += 1;
                if count > MAX_NODES || stack.len() >= MAX_DEPTH {
                    return Err("xml too large or deep".into());
                }
                stack.push(open_node(res, &e)?);
            }
            Event::Empty(e) => {
                count += 1;
                if count > MAX_NODES || stack.len() >= MAX_DEPTH {
                    return Err("xml too large or deep".into());
                }
                let n = open_node(res, &e)?;
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => {
                        if root.is_some() {
                            return Err("multiple roots".into());
                        }
                        root = Some(n);
                    }
                }
            }
            Event::End(_) => {
                let n = stack.pop().ok_or("unbalanced end tag")?;
                match stack.last_mut() {
                    Some(p) => p.children.push(n),
                    None => {
                        if root.is_some() {
                            return Err("multiple roots".into());
                        }
                        root = Some(n);
                    }
                }
            }
            Event::Text(t) => {
                if let Some(n) = stack.last_mut() {
                    let s = t.xml10_content();
                    n.text.push_str(&s);
                }
            }
            Event::CData(c) => {
                if let Some(n) = stack.last_mut() {
                    let s = c.xml10_content();
                    n.text.push_str(&s);
                }
            }
            Event::GeneralRef(g) => {
                if let Some(n) = stack.last_mut() {
                    if let Ok(Some(ch)) = g.resolve_char_ref() {
                        n.text.push(ch);
                    } else {
                        let name = g.xml10_content();
                        match quick_xml::escape::resolve_predefined_entity(&name) {
                            Some(s) => n.text.push_str(s),
                            None => return Err(format!("unknown entity &{name};")),
                        }
                    }
                }
            }
            Event::DocType(_) => return Err("DOCTYPE not allowed".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("unclosed element".into());
    }
    root.ok_or_else(|| "empty document".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ns_attrs_text() {
        let n = parse(br#"<a:R xmlns:a="urn:x" xmlns="urn:d"><b k="1&amp;2">h&lt;i &#65;</b><c/></a:R>"#).unwrap();
        assert_eq!((n.ns.as_str(), n.name.as_str()), ("urn:x", "R"));
        let b = n.child("b").unwrap();
        assert_eq!(b.ns, "urn:d");
        assert_eq!(b.attr("k"), Some("1&2"));
        assert_eq!(b.text, "h<i A");
        assert!(n.child("c").is_some());
    }

    #[test]
    fn rejects_doctype_and_junk() {
        assert!(parse(b"<!DOCTYPE a [<!ENTITY x 'y'>]><a>&x;</a>").is_err());
        assert!(parse(b"<a><b></a>").is_err());
        assert!(parse(b"").is_err());
        assert!(parse(b"<a>").is_err());
    }

    #[test]
    fn depth_capped() {
        let s = "<a>".repeat(100) + &"</a>".repeat(100);
        assert!(parse(s.as_bytes()).is_err());
    }
}

//! The bits of WebDAV XML a CalDAV client needs: a small namespace-aware
//! tree, the multistatus responses (RFC 4918 §13), and request bodies.
//!
//! Elements are matched by namespace URI and local name, never by prefix:
//! servers pick their own prefixes (iCloud uses default namespaces, others
//! `d:`/`D:`/`cal:`/`C:`).

use anyhow::{anyhow, Result};
use quick_xml::events::Event as XmlEvent;
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
#[cfg(test)]
pub const CS: &str = "http://calendarserver.org/ns/";
pub const ICAL: &str = "http://apple.com/ns/ical/";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Node {
    pub ns: String,
    pub name: String,
    /// Attributes by local name.
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
    pub text: String,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }

    pub fn child(&self, ns: &str, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.is(ns, name))
    }

    pub fn children_named<'a>(&'a self, ns: &'a str, name: &'a str) -> impl Iterator<Item = &'a Node> {
        self.children.iter().filter(move |c| c.is(ns, name))
    }

    /// The first descendant (or itself) with this name, depth first.
    pub fn find(&self, ns: &str, name: &str) -> Option<&Node> {
        if self.is(ns, name) {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(ns, name))
    }

    /// The text of the first `<href>` inside, trimmed.
    pub fn href(&self) -> Option<String> {
        self.find(DAV, "href").map(|h| h.text.trim().to_owned()).filter(|h| !h.is_empty())
    }
}

/// Parses a document into its root element.
pub fn parse(xml: &str) -> Result<Node> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Node> = vec![Node::default()];
    let node = |ns: ResolveResult, e: &quick_xml::events::BytesStart| Node {
        ns: match ns {
            ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
            _ => String::new(),
        },
        name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
        attrs: e
            .attributes()
            .flatten()
            .filter_map(|a| {
                let key = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
                Some((key, a.unescape_value().ok()?.into_owned()))
            })
            .collect(),
        ..Node::default()
    };
    loop {
        match reader.read_resolved_event()? {
            (ns, XmlEvent::Start(e)) => stack.push(node(ns, &e)),
            (ns, XmlEvent::Empty(e)) => {
                let n = node(ns, &e);
                stack.last_mut().unwrap().children.push(n);
            }
            (_, XmlEvent::End(_)) => {
                let n = stack.pop().ok_or_else(|| anyhow!("unbalanced XML"))?;
                stack.last_mut().ok_or_else(|| anyhow!("unbalanced XML"))?.children.push(n);
            }
            (_, XmlEvent::Text(t)) => stack.last_mut().unwrap().text.push_str(&t.unescape()?),
            (_, XmlEvent::CData(c)) => stack.last_mut().unwrap().text.push_str(&String::from_utf8_lossy(&c.into_inner())),
            (_, XmlEvent::Eof) => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(anyhow!("truncated XML"));
    }
    stack.pop().unwrap().children.into_iter().next().ok_or_else(|| anyhow!("empty XML document"))
}

/// One `<response>` of a multistatus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Response {
    /// As sent: percent-encoded, absolute path or full URL.
    pub href: String,
    /// The response's own status (sync-collection reports deletions as 404).
    pub status: Option<u16>,
    /// The properties the server returned successfully.
    pub props: Vec<Node>,
}

impl Response {
    pub fn prop(&self, ns: &str, name: &str) -> Option<&Node> {
        self.props.iter().find(|p| p.is(ns, name))
    }

    pub fn prop_text(&self, ns: &str, name: &str) -> Option<String> {
        self.prop(ns, name).map(|p| p.text.trim().to_owned()).filter(|t| !t.is_empty())
    }
}

/// "HTTP/1.1 404 Not Found" → 404.
fn status_code(text: &str) -> Option<u16> {
    text.split_whitespace().nth(1)?.parse().ok()
}

/// The responses of a 207 Multi-Status, and its `<sync-token>` if any.
pub fn multistatus(xml: &str) -> Result<(Vec<Response>, Option<String>)> {
    let root = parse(xml)?;
    if !root.is(DAV, "multistatus") {
        return Err(anyhow!("expected a multistatus, got <{}>", root.name));
    }
    let mut out = Vec::new();
    for r in root.children_named(DAV, "response") {
        let Some(href) = r.child(DAV, "href").map(|h| h.text.trim().to_owned()) else { continue };
        let status = r.child(DAV, "status").and_then(|s| status_code(&s.text));
        let mut props = Vec::new();
        for ps in r.children_named(DAV, "propstat") {
            let ok = ps.child(DAV, "status").and_then(|s| status_code(&s.text)).map_or(true, |c| (200..300).contains(&c));
            if let (true, Some(prop)) = (ok, ps.child(DAV, "prop")) {
                props.extend(prop.children.iter().cloned());
            }
        }
        out.push(Response { href, status, props });
    }
    let token = root.child(DAV, "sync-token").map(|t| t.text.trim().to_owned()).filter(|t| !t.is_empty());
    Ok((out, token))
}

/// Escapes text for an XML body.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// ---------------------------------------------------------------------------
// Request bodies

pub const PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;

pub const HOME_SET: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;

pub const CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ic="http://apple.com/ns/ical/">
  <d:prop>
    <d:resourcetype/>
    <d:displayname/>
    <d:current-user-privilege-set/>
    <c:supported-calendar-component-set/>
    <ic:calendar-color/>
    <ic:calendar-order/>
    <cs:getctag/>
    <d:sync-token/>
  </d:prop>
</d:propfind>"#;

pub const SYNC_TOKEN: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:"><d:prop><d:sync-token/></d:prop></d:propfind>"#;

/// Every event overlapping [start, end) (UTC "YYYYMMDDTHHMMSSZ"), with its
/// data. Repeating events are returned once, as the whole series.
pub fn calendar_query(start: &str, end: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  <c:filter>
    <c:comp-filter name="VCALENDAR">
      <c:comp-filter name="VEVENT"><c:time-range start="{start}" end="{end}"/></c:comp-filter>
    </c:comp-filter>
  </c:filter>
</c:calendar-query>"#
    )
}

/// What changed since `token` (RFC 6578).
pub fn sync_collection(token: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:">
  <d:sync-token>{}</d:sync-token>
  <d:sync-level>1</d:sync-level>
  <d:prop><d:getetag/></d:prop>
</d:sync-collection>"#,
        escape(token)
    )
}

/// The data of these resources.
pub fn multiget(hrefs: &[String]) -> String {
    let list: String = hrefs.iter().map(|h| format!("<d:href>{}</d:href>", escape(h))).collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:getetag/><c:calendar-data/></d:prop>
  {list}
</c:calendar-multiget>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_are_matched_by_uri_not_prefix() {
        let a = parse(r#"<D:multistatus xmlns:D="DAV:"><D:response/></D:multistatus>"#).unwrap();
        let b = parse(r#"<multistatus xmlns="DAV:"><response xmlns="DAV:"/></multistatus>"#).unwrap();
        assert!(a.is(DAV, "multistatus") && b.is(DAV, "multistatus"));
        assert!(a.child(DAV, "response").is_some() && b.child(DAV, "response").is_some());
        let other = parse(r#"<x:multistatus xmlns:x="urn:other"/>"#).unwrap();
        assert!(!other.is(DAV, "multistatus"));
    }

    #[test]
    fn attributes_are_kept() {
        let set = parse(
            r#"<C:supported-calendar-component-set xmlns:C="urn:ietf:params:xml:ns:caldav"><C:comp name="VTODO"/><C:comp name='VEVENT'></C:comp></C:supported-calendar-component-set>"#,
        )
        .unwrap();
        let names: Vec<&str> = set.children_named(CALDAV, "comp").filter_map(|c| c.attr("name")).collect();
        assert_eq!(names, ["VTODO", "VEVENT"]);
    }

    #[test]
    fn multistatus_keeps_only_successful_properties() {
        // Shaped like iCloud's answer: default namespaces, a 404 propstat for
        // properties it doesn't have, entities in text.
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<multistatus xmlns="DAV:">
  <response>
    <href>/123/calendars/home/</href>
    <propstat>
      <prop>
        <displayname>Home &amp; Family</displayname>
        <resourcetype><collection/><calendar xmlns="urn:ietf:params:xml:ns:caldav"/></resourcetype>
        <calendar-color xmlns="http://apple.com/ns/ical/">#FF2968FF</calendar-color>
      </prop>
      <status>HTTP/1.1 200 OK</status>
    </propstat>
    <propstat>
      <prop><getctag xmlns="http://calendarserver.org/ns/"/></prop>
      <status>HTTP/1.1 404 Not Found</status>
    </propstat>
  </response>
  <response><href>/123/calendars/gone.ics</href><status>HTTP/1.1 404 Not Found</status></response>
  <sync-token>https://example.com/sync/42</sync-token>
</multistatus>"#;
        let (rs, token) = multistatus(xml).unwrap();
        assert_eq!(token.as_deref(), Some("https://example.com/sync/42"));
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].href, "/123/calendars/home/");
        assert_eq!(rs[0].prop_text(DAV, "displayname").as_deref(), Some("Home & Family"));
        assert!(rs[0].prop(DAV, "resourcetype").unwrap().child(CALDAV, "calendar").is_some());
        assert_eq!(rs[0].prop_text(ICAL, "calendar-color").as_deref(), Some("#FF2968FF"));
        assert!(rs[0].prop(CS, "getctag").is_none(), "404 properties are dropped");
        assert_eq!((rs[1].status, rs[1].props.len()), (Some(404), 0));
    }

    #[test]
    fn calendar_data_survives_escaping_and_cdata() {
        let xml = "<d:multistatus xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\"><d:response><d:href>/a.ics</d:href>\
            <d:propstat><d:prop><c:calendar-data>BEGIN:VCALENDAR&#13;\nX:a&lt;b&#13;\nEND:VCALENDAR</c:calendar-data></d:prop></d:propstat></d:response>\
            <d:response><d:href>/b.ics</d:href><d:propstat><d:prop><c:calendar-data><![CDATA[BEGIN:VCALENDAR\r\nX:<b>\r\nEND:VCALENDAR]]></c:calendar-data></d:prop></d:propstat></d:response></d:multistatus>";
        let (rs, _) = multistatus(xml).unwrap();
        assert_eq!(rs[0].prop(CALDAV, "calendar-data").unwrap().text, "BEGIN:VCALENDAR\r\nX:a<b\r\nEND:VCALENDAR");
        assert_eq!(rs[1].prop(CALDAV, "calendar-data").unwrap().text, "BEGIN:VCALENDAR\r\nX:<b>\r\nEND:VCALENDAR");
    }

    #[test]
    fn broken_documents_are_errors() {
        assert!(parse("<a><b></a>").is_err());
        assert!(parse("").is_err());
        assert!(multistatus(r#"<d:error xmlns:d="DAV:"/>"#).is_err());
    }

    #[test]
    fn request_bodies_are_well_formed() {
        for body in [
            PRINCIPAL.to_owned(),
            HOME_SET.to_owned(),
            CALENDARS.to_owned(),
            SYNC_TOKEN.to_owned(),
            calendar_query("20260501T000000Z", "20271201T000000Z"),
            sync_collection("http://x/sync?a=1&b=2"),
            multiget(&["/a b.ics".into(), "/c&d.ics".into()]),
        ] {
            parse(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
        }
        let q = parse(&sync_collection("t&1")).unwrap();
        assert_eq!(q.child(DAV, "sync-token").unwrap().text, "t&1");
    }
}

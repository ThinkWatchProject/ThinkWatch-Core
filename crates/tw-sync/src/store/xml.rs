//! 读 WebDAV 的 PROPFIND 和 S3 的 ListObjectsV2、错误答复：只要几个元素的文字。
//!
//! **按本地名认元素，不管命名空间前缀**：同一个 `DAV:` 命名空间，有的服务器写 `D:href`，
//! 有的写 `d:href`、`lp1:getcontentlength`，还有的不带前缀。

use std::collections::BTreeMap;

use quick_xml::events::Event;

/// 一条记录：一个 `record` 元素底下各个元素的文字（同名的取第一个），以及出现过的空元素
/// （`<D:collection/>` 这种只看在不在的）
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Record {
    pub text: BTreeMap<String, String>,
    pub present: std::collections::BTreeSet<String>,
}

impl Record {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.text.get(name).map(String::as_str)
    }
    pub fn has(&self, name: &str) -> bool {
        self.present.contains(name) || self.text.contains_key(name)
    }
}

/// 预定义的五个实体和字符引用。认不出的原样留着
fn entity(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        _ => {
            let n = name
                .strip_prefix("#x")
                .or_else(|| name.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| name.strip_prefix('#').and_then(|d| d.parse().ok()));
            match n.and_then(char::from_u32) {
                Some(c) => c.to_string(),
                None => format!("&{name};"),
            }
        }
    }
}

/// 文档里每个本地名是 `record` 的元素一条记录；`top` 是不在任何记录里的那些元素的文字。
/// 读不了的 XML 返回 `None`
pub fn parse(xml: &str, record: &str) -> Option<(Vec<Record>, BTreeMap<String, String>)> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack: Vec<String> = Vec::new();
    let mut records = Vec::new();
    let mut top = BTreeMap::new();
    // 正在读的那一条记录，以及它在栈里的深度
    let mut cur: Option<(usize, Record)> = None;
    let mut text = String::new();
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => {
                let name = e.local_name().as_ref().to_string();
                stack.push(name.clone());
                text.clear();
                if name == record && cur.is_none() {
                    cur = Some((stack.len(), Record::default()));
                }
            }
            Event::Empty(e) => {
                let name = e.local_name().as_ref().to_string();
                if let Some((_, r)) = cur.as_mut() {
                    r.present.insert(name);
                }
            }
            Event::Text(t) => text.push_str(&t),
            Event::CData(t) => text.push_str(&t),
            Event::GeneralRef(r) => text.push_str(&entity(&r)),
            Event::End(_) => {
                let name = stack.pop()?;
                let value = std::mem::take(&mut text).trim().to_string();
                match cur.as_mut() {
                    Some((depth, _)) if *depth == stack.len() + 1 => {
                        if let Some((_, r)) = cur.take() {
                            records.push(r);
                        }
                    }
                    Some((_, r)) => {
                        r.present.insert(name.clone());
                        if !value.is_empty() {
                            r.text.entry(name).or_insert(value);
                        }
                    }
                    None => {
                        if !value.is_empty() {
                            top.entry(name).or_insert(value);
                        }
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Some((records, top))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webdav_multistatus_with_any_prefix() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<d:multistatus xmlns:d="DAV:" xmlns:lp1="DAV:">
  <d:response>
    <d:href>/dav/tw/versions/</d:href>
    <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat>
  </d:response>
  <d:response>
    <d:href>/dav/tw/versions/0000000001-aaaaaaaa.twconfig</d:href>
    <d:propstat><d:prop>
      <lp1:getcontentlength>1234</lp1:getcontentlength>
      <d:resourcetype/>
    </d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  </d:response>
</d:multistatus>"#;
        let (rs, _) = parse(xml, "response").unwrap();
        assert_eq!(rs.len(), 2);
        assert!(rs[0].has("collection"));
        assert_eq!(
            rs[1].get("href"),
            Some("/dav/tw/versions/0000000001-aaaaaaaa.twconfig")
        );
        assert_eq!(rs[1].get("getcontentlength"), Some("1234"));
        assert!(!rs[1].has("collection"));
    }

    #[test]
    fn s3_listing_and_its_continuation() {
        let xml = r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
<Name>b</Name><Prefix>p/versions/</Prefix><IsTruncated>true</IsTruncated>
<NextContinuationToken>tok&amp;en</NextContinuationToken>
<Contents><Key>p/versions/a</Key><Size>3</Size></Contents>
<Contents><Key>p/versions/b&#x26;c</Key><Size>4</Size></Contents>
</ListBucketResult>"#;
        let (rs, top) = parse(xml, "Contents").unwrap();
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[1].get("Key"), Some("p/versions/b&c"));
        assert_eq!(top.get("IsTruncated").map(String::as_str), Some("true"));
        assert_eq!(
            top.get("NextContinuationToken").map(String::as_str),
            Some("tok&en")
        );
    }

    #[test]
    fn broken_xml_is_none_not_a_panic() {
        assert!(parse("<a><b></a>", "b").is_none());
    }
}

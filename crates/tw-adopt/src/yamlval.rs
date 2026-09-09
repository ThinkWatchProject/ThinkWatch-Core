//! 把一份 YAML 摊成可比较的语义值，给写回校验用。
//!
//! 只收标量，键是路径的人话形式。够用了：校验要回答的是「除了我们点名
//! 的那几个字段，其余有没有变」，而**结构变了的话路径本身就会变**，
//! 一样能被抓住。

use crate::json::Val;
use crate::yaml::YErr;

pub fn value(text: &str) -> Result<Val, YErr> {
    let mut out = Vec::new();
    for n in tw_yaml::nodes(text)? {
        if let tw_yaml::NodeKind::Scalar { value, .. } = &n.kind {
            out.push((tw_yaml::show(&n.path), Val::Str(value.clone())));
        }
    }
    Ok(Val::Obj(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_named_field_differs_after_a_patch() {
        let src = "model: gpt-4o\nopenai-api-base: 'http://old'\ndark: true\n";
        let out = crate::yaml::set(src, &["openai-api-base"], "http://new").unwrap();
        let expected = value(src)
            .unwrap()
            .with(&["openai-api-base"], &Val::s("http://new"));
        assert_eq!(value(&out).unwrap(), expected);
    }

    #[test]
    fn an_appended_field_shows_up_as_exactly_one_addition() {
        let src = "model: gpt-4o\n";
        let out = crate::yaml::set(src, &["openai-api-base"], "http://new").unwrap();
        let expected = value(src)
            .unwrap()
            .with(&["openai-api-base"], &Val::s("http://new"));
        assert_eq!(value(&out).unwrap(), expected);
    }
}

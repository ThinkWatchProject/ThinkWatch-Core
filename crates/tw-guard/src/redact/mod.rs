//! 出站脱敏：把敏感值换成占位符，响应回显时再换回来。

pub mod replace;
pub mod rules;
pub mod sse;
pub mod stream;

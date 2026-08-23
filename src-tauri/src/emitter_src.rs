//! build.rs 从 ui/emitter.js 生成的嵌入源（勿手改）。
include!(concat!(env!("OUT_DIR"), "/emitter_src.rs"));

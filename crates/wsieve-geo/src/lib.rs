//! geosite.dat / geoip.dat 的解析与匹配。
//!
//! 这两个文件是 v2ray 系的事实标准格式，protobuf 编码。结构极简
//! （枚举 + 字符串 + bytes + uint32，三层嵌套），因此手写 wire format
//! 解析而不引入 prost —— 与仓库里手写 base64 的取舍一致
//! （见 src-tauri/src/bridge.rs:285 的注释）。
//!
//! 权威定义见 .research/Xray-core/common/geodata/geodat.proto。

pub mod ip;
pub mod pb;
pub mod site;

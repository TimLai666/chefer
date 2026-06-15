//! chefer-bundle — 跨 crate 共用的 bundle 協定型別與工具。
//!
//! 這裡是 manifest.json schema、footer 格式、bundle 佈局的唯一定義處；
//! 其他 crate 一律 import 本 crate 的型別，不得自行手刻 JSON 或 footer 位元組。
//! 規格詳見 docs/DESIGN.md。

pub mod footer;
pub mod kit;
pub mod layout;
pub mod limit;
pub mod manifest;
pub mod mounts;
pub mod ports;
pub mod topo;

pub use footer::{FLAG_ZSTD, FOOTER_LEN, Footer, MAGIC};
pub use limit::{LimitedReader, bomb_limit_for};
pub use manifest::{
    AppMeta, CmdSpec, ConsoleMode, CrashPolicy, HealthCheck, ImageConfig, InterfaceMode, LayerRef,
    MANIFEST_FORMAT_VERSION, Manifest, NetworkMode, ServiceEntry,
};
pub use mounts::MountSpec;
pub use ports::{PortProto, PortSpec};
pub use topo::topo_sort;

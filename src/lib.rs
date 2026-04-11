// Exactly one of `wgpu` or `ort-cpu` must be active.
#[cfg(all(feature = "wgpu", feature = "ort-cpu"))]
compile_error!("features `wgpu` and `ort-cpu` are mutually exclusive — enable exactly one");

#[cfg(not(any(feature = "wgpu", feature = "ort-cpu")))]
compile_error!("one of features `wgpu` or `ort-cpu` must be enabled");

pub mod chunk;
pub mod chunker;
pub mod config;
pub mod daemon;
pub mod embedder;
pub mod git;
pub mod ipc;
#[cfg(feature = "wgpu")]
pub mod nomic_bert_burn;
pub mod search;
pub mod service;
pub mod store;

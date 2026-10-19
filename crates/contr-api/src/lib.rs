//! gRPC contract of the Contr daemon. `src/gen` is generated from `proto/` by `cargo xtask codegen`; edit the proto
//! files, never the generated code.

#[allow(clippy::all, clippy::pedantic, missing_docs, unreachable_pub, unused_qualifications)]
mod generated {
    include!("gen/mod.rs");
}

pub use generated::contr::*;

include!("gen/revision.rs");

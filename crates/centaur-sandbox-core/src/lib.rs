//! Backend-neutral sandbox runtime types.
//!
//! This crate intentionally models only isolated runtime workloads. Centaur
//! concepts such as thread keys, personas, harnesses, model choice, assignment
//! generations, and durable execution rows belong in higher-level crates.

mod backend;
mod error;
mod io;
mod lifecycle;
mod spec;

/// Home directory used by the sandbox agent image.
pub const SANDBOX_AGENT_HOME: &str = "/home/agent";

pub const COMPONENT_LABEL: &str = "centaur.ai/component";
pub const WORKFLOW_RUN_COMPONENT: &str = "workflow-run";
pub const WORKFLOW_RUN_ID_LABEL: &str = "centaur.ai/workflow-run-id";

pub use backend::SandboxBackend;
pub use error::{BoxedError, SandboxError, SandboxResult};
pub use io::{SandboxIo, SandboxIoGuard, SandboxIoParts, SandboxRead, SandboxWrite};
pub use lifecycle::{ObservedSandbox, SandboxHandle, SandboxId, SandboxStatus};
pub use spec::{
    EnvVar, Mount, MountKind, RepoCacheAccess, ResourceClaim, ResourceRequirements,
    SandboxCapabilities, SandboxFile, SandboxSpec,
};

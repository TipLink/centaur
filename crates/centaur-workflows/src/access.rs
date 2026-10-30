//! One classification shared by submission, child starts and host dispatch.
//! Checking admission does not replace checking again at execution time.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowAccess {
    Ordinary,
    ApprovalRequired,
    Internal,
}

pub(crate) fn classify(
    name: &str,
    policies: &approvals::Policies,
    sandbox: Option<&WorkflowHostSandboxRuntime>,
) -> WorkflowAccess {
    if approvals::is_reserved(name) {
        WorkflowAccess::Internal
    } else if policies.values().any(|p| p.workflow == name)
        || sandbox.is_some_and(|s| s.requires_approval(name))
    {
        WorkflowAccess::ApprovalRequired
    } else {
        WorkflowAccess::Ordinary
    }
}

pub(crate) fn ensure_public(
    name: &str,
    policies: &approvals::Policies,
    sandbox: Option<&WorkflowHostSandboxRuntime>,
) -> Result<(), WorkflowRuntimeError> {
    if classify(name, policies, sandbox) != WorkflowAccess::Ordinary {
        return Err(approvals::unavailable());
    }
    Ok(())
}

impl WorkflowQueueClients {
    pub(super) fn ensure_public_workflow(&self, name: &str) -> Result<(), WorkflowRuntimeError> {
        ensure_public(
            name,
            &self.approval_policies,
            self.workflow_host_sandbox.as_ref(),
        )
    }

    pub(super) fn approval_repository(&self) -> approvals::repository::Repository {
        approvals::repository::Repository {
            pool: self.standard.pool().clone(),
            policies: Arc::new(approvals::resolve_policies(
                &self.approval_policies,
                self.workflow_host_sandbox.as_ref(),
            )),
        }
    }
}

//! Integration tests for A2A dispatch authorization (Phase 1 network isolation).
//!
//! These exercise `avm_gateway::security` through the crate's public surface,
//! the same way the `POST /a2a/task` handler will.

use avm_agent::{A2APolicy, AgentCard, AgentScope, TrustDefault};
use avm_gateway::security::{
    validate_a2a_dispatch, A2ADispatch, AuthError, CollectingAudit, ScopeError, SecurityError,
};

fn target_card(scope: AgentScope, policy: A2APolicy) -> AgentCard {
    AgentCard::new("callee", scope).with_policy(policy)
}

#[test]
fn same_tenant_same_project_trusted_peer_is_allowed() {
    let target = AgentScope::new("tenant-a", "proj-1", "reviewer");
    let source = AgentScope::new("tenant-a", "proj-1", "planner");
    let policy = A2APolicy::default()
        .allowing_intra_project()
        .with_trusted_peer("planner");

    let audit = CollectingAudit::new();
    let dispatch = A2ADispatch::new("task-allow", source, target.clone());

    let event =
        validate_a2a_dispatch(&dispatch, &target_card(target, policy), &audit).expect("allowed");

    assert!(event.allowed);
    assert_eq!(event.reason, "trusted_peer");
    assert_eq!(event.task_id, "task-allow");
    assert_eq!(audit.len(), 1, "the allow path must still be audited");
}

#[test]
fn different_tenant_is_rejected() {
    let target = AgentScope::new("tenant-b", "proj-1", "reviewer");
    let source = AgentScope::new("tenant-a", "proj-1", "planner");

    // Deliberately the most permissive policy expressible.
    let policy = A2APolicy {
        default: TrustDefault::Allow,
        trusted_peers: vec!["planner".into()],
        allow_intra_project: true,
        allow_cross_project: true,
    };

    let audit = CollectingAudit::new();
    let dispatch = A2ADispatch::new("task-cross-tenant", source, target.clone());

    let err = validate_a2a_dispatch(&dispatch, &target_card(target, policy), &audit)
        .expect_err("cross-tenant must never be allowed");

    assert!(matches!(
        err,
        SecurityError::Scope(ScopeError::CrossTenantDenied { .. })
    ));
    assert_eq!(err.http_status(), 403);
    assert_eq!(audit.len(), 1);
    assert_eq!(audit.events()[0].reason, "cross_tenant_denied");
    assert!(!audit.events()[0].allowed);
}

#[test]
fn intra_project_is_denied_by_default() {
    let target = AgentScope::new("tenant-a", "proj-1", "reviewer");
    let source = AgentScope::new("tenant-a", "proj-1", "planner");

    let audit = CollectingAudit::new();
    let dispatch = A2ADispatch::new("task-intra", source, target.clone());

    let err = validate_a2a_dispatch(
        &dispatch,
        &target_card(target, A2APolicy::default()),
        &audit,
    )
    .expect_err("intra-project A2A must be deny-by-default");

    assert!(matches!(
        err,
        SecurityError::Scope(ScopeError::IntraProjectDenied { .. })
    ));
    assert_eq!(audit.len(), 1);
}

#[test]
fn cross_project_same_tenant_is_denied_by_default_and_allowed_on_opt_in() {
    let target = AgentScope::new("tenant-a", "proj-2", "reviewer");
    let source = AgentScope::new("tenant-a", "proj-1", "planner");
    let audit = CollectingAudit::new();

    let denied = validate_a2a_dispatch(
        &A2ADispatch::new("t1", source.clone(), target.clone()),
        &target_card(target.clone(), A2APolicy::default()),
        &audit,
    )
    .expect_err("cross-project denied by default");
    assert!(matches!(
        denied,
        SecurityError::Scope(ScopeError::CrossProjectDenied { .. })
    ));

    let policy = A2APolicy::default()
        .allowing_cross_project()
        .with_trusted_peer("planner");
    let allowed = validate_a2a_dispatch(
        &A2ADispatch::new("t2", source, target.clone()),
        &target_card(target, policy),
        &audit,
    )
    .expect("cross-project allowed after opt-in");
    assert!(allowed.allowed);

    assert_eq!(audit.len(), 2, "both attempts audited");
}

#[test]
fn untrusted_peer_within_allowed_scope_is_an_auth_error_not_a_scope_error() {
    let target = AgentScope::new("tenant-a", "proj-1", "reviewer");
    let source = AgentScope::new("tenant-a", "proj-1", "stranger");
    let policy = A2APolicy::default()
        .allowing_intra_project()
        .with_trusted_peer("planner");

    let audit = CollectingAudit::new();
    let err = validate_a2a_dispatch(
        &A2ADispatch::new("task-untrusted", source, target.clone()),
        &target_card(target, policy),
        &audit,
    )
    .expect_err("stranger is not a trusted peer");

    assert!(matches!(
        err,
        SecurityError::Auth(AuthError::PeerNotTrusted { .. })
    ));
    assert_eq!(err.reason_code(), "peer_not_trusted");
}

#[test]
fn audit_trail_is_complete_across_a_mixed_batch() {
    let audit = CollectingAudit::new();
    let target = AgentScope::new("tenant-a", "proj-1", "reviewer");
    let open = A2APolicy::default()
        .allowing_intra_project()
        .with_trusted_peer("planner");

    let cases: Vec<(&str, AgentScope, A2APolicy, bool)> = vec![
        (
            "ok",
            AgentScope::new("tenant-a", "proj-1", "planner"),
            open.clone(),
            true,
        ),
        (
            "xtenant",
            AgentScope::new("tenant-z", "proj-1", "planner"),
            open.clone(),
            false,
        ),
        (
            "xintra",
            AgentScope::new("tenant-a", "proj-1", "planner"),
            A2APolicy::default(),
            false,
        ),
        ("self", target.clone(), A2APolicy::default(), true),
    ];

    for (id, source, policy, expect_ok) in cases {
        let res = validate_a2a_dispatch(
            &A2ADispatch::new(id, source, target.clone()),
            &target_card(target.clone(), policy),
            &audit,
        );
        assert_eq!(res.is_ok(), expect_ok, "case {id}");
    }

    let events = audit.events();
    assert_eq!(
        events.len(),
        4,
        "every dispatch attempt is audited exactly once"
    );
    assert_eq!(
        events.iter().filter(|e| e.allowed).count(),
        2,
        "two allows: trusted peer + self-dispatch"
    );
    // No event may be missing its reason code.
    assert!(events.iter().all(|e| !e.reason.is_empty()));
}

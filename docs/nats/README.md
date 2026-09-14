# NATS tenant isolation — queue-layer boundaries

Layer 2 of AVM's defence-in-depth. See `IMPLEMENTATION_PLAN.md`
§ *Network Isolation & A2A Security*; this directory is the operator-facing
half of **Phase 2**.

## What this layer protects against

| | |
|---|---|
| **Stops** | An agent (or a stolen agent credential) subscribing to another tenant's job stream, publishing a forged result, or writing directly onto a peer agent's A2A subject to bypass the gateway. |
| **Does not stop** | Direct Pod-to-Pod traffic that never touches NATS — that is the NetworkPolicy layer (`docs/network-policies/`). Nor does it understand `trusted_peers`; policy nuance lives in the gateway. |

The key structural decision: **one NATS account per tenant**. Accounts, not
subject prefixes, are the hard isolation primitive in NATS — subjects do not
cross an account boundary at all. A subject-prefix scheme inside one shared
account is one typo away from a cross-tenant leak; an account boundary is not.

Inside the account, per-agent users get narrower subject permissions so one
agent cannot impersonate another. Note in `tenant-isolation.yaml` that agent
users are **denied publish on `avm.a2a.>` entirely** — an agent can only reach
a peer by asking the gateway, which is what makes `validate_a2a_dispatch()`
unavoidable at the queue layer.

## Subject layout

```
avm.jobs.<tenant>.<project>              job envelopes      (scheduler → executor)
avm.results.<tenant>.<project>           result envelopes   (executor → scheduler)
avm.a2a.<tenant>.<project>.<agent>       A2A delivery       (gateway → agent)
avm.gateway.<tenant>.dispatch            A2A request        (agent → gateway)
```

## Files

| File | Purpose |
|---|---|
| `tenant-isolation.yaml` | Annotated template: system account, two example tenant accounts, per-role users with subject ACLs, JetStream + TLS settings. |

Treat it as a template, not a deployable. Generate the real config per tenant.

## Generating credentials

The static-password form in the template is for local development. In
production use the NSC/JWT flow so credentials are signed, expiring and
revocable without a server restart.

```sh
# One-time: create the operator and system account.
nsc add operator --name AVM --generate-signing-key --sys
nsc edit operator --account-jwt-server-url nats://nats.avm-system:4222

# Per tenant: an account with JetStream enabled and hard limits.
nsc add account --name t_abc123
nsc edit account --name t_abc123 \
  --js-mem-storage 512M --js-disk-storage 10G \
  --conns 256 --payload 1048576

# Per agent: a user that can ONLY receive on its own A2A subject and can only
# reach a peer by going through the gateway.
nsc add user --account t_abc123 --name p_xyz789.agent_planner \
  --allow-sub "avm.a2a.t_abc123.p_xyz789.agent_planner" \
  --allow-sub "_INBOX.>" \
  --allow-pub "avm.gateway.t_abc123.dispatch" \
  --allow-pub "_INBOX.>" \
  --deny-pub  "avm.a2a.>" \
  --deny-pub  "avm.jobs.>" \
  --deny-pub  "avm.results.>" \
  --expiry 720h

# Export the .creds file the agent container mounts.
nsc generate creds --account t_abc123 --name p_xyz789.agent_planner \
  > /etc/avm/creds/t_abc123.p_xyz789.agent_planner.creds
```

Mount the `.creds` file read-only, mode `0400`, owned by the agent UID. Never
bake it into an image — it is per-agent, and images are shared.

## Rotating

Credentials carry `--expiry`, so rotation is the normal path, not an incident
response.

```sh
# Issue the replacement alongside the old one (both valid during the overlap).
nsc add user --account t_abc123 --name p_xyz789.agent_planner.v2 \
  --allow-sub "avm.a2a.t_abc123.p_xyz789.agent_planner" \
  --allow-pub "avm.gateway.t_abc123.dispatch" \
  --expiry 720h
nsc generate creds --account t_abc123 --name p_xyz789.agent_planner.v2 \
  > /etc/avm/creds/…v2.creds

# Roll the workload onto the new secret, confirm connections have moved:
nats --creds=/path/sys.creds server report connections | grep p_xyz789

# Only then revoke the old one.
nsc revocations add-user --account t_abc123 --name p_xyz789.agent_planner
nsc push --account t_abc123
```

Overlap first, verify, then revoke. Revoking before the rollout completes
takes the agent down.

## Revoking (incident path)

```sh
# Revoke a single leaked user credential — effective at the next auth check.
nsc revocations add-user --account t_abc123 --name p_xyz789.agent_planner
nsc push --account t_abc123

# Nuclear option: revoke every user in a tenant account.
nsc revocations add-user --account t_abc123 --user "*"
nsc push --account t_abc123

# Confirm the connections are actually gone — pushing the JWT is not proof.
nats --creds=/path/sys.creds server report connections
```

`nsc push` updates the account JWT on the server; existing connections are
dropped at the next authorization check, not instantly. If you need an
immediate cut, drop the connection at the NetworkPolicy layer as well.

## Verifying isolation

Do this after any change to the account layout. A misconfigured ACL is
indistinguishable from a correct one until someone tests it.

```sh
# MUST fail: tenant A's credential subscribing to tenant B's jobs.
nats --creds=/etc/avm/creds/t_abc123.executor.creds \
  sub "avm.jobs.t_def456.>"

# MUST fail: an agent publishing directly onto a peer's A2A subject.
nats --creds=/etc/avm/creds/t_abc123.p_xyz789.agent_planner.creds \
  pub "avm.a2a.t_abc123.p_xyz789.agent_reviewer" '{"hello":"peer"}'

# MUST succeed: the same agent reaching the gateway.
nats --creds=/etc/avm/creds/t_abc123.p_xyz789.agent_planner.creds \
  pub "avm.gateway.t_abc123.dispatch" '{"task_id":"probe"}'
```

If the first two succeed, the account layout is wrong — stop and fix it before
onboarding another tenant.

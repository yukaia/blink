# Windows ssh-agent Support

**Date:** 2026-05-07  
**Status:** Approved  

## Summary

Add Windows ssh-agent authentication to blink's SFTP/SCP transport. Currently the `AuthMethod::Agent` arm returns a hard error on Windows. russh-keys 0.49.2 already ships both required connection methods; the work is entirely in `src/transport/sftp.rs`.

## Scope

- **In scope:** Windows OpenSSH named pipe (`\\.\pipe\openssh-ssh-agent`) and Pageant (PuTTY), tried in that order.
- **Out of scope:** `SSH_AUTH_SOCK` on Windows (uncommon, not needed), per-session agent backend config, agent forwarding.

## Architecture

Single file change: `src/transport/sftp.rs`, `AuthMethod::Agent` arm (~line 232).

russh-keys provides all required primitives:

| Method | Type returned | Platform |
|---|---|---|
| `AgentClient::connect_named_pipe(path)` | `AgentClient<NamedPipeClient>` | `#[cfg(windows)]` |
| `AgentClient::connect_pageant()` | `AgentClient<PageantStream>` | `#[cfg(windows)]` |
| `AgentClient::connect_env()` | `AgentClient<UnixStream>` | `#[cfg(unix)]` |
| `.dynamic()` | `AgentClient<Box<dyn AgentStream + Send + Unpin>>` | all |

The `pageant` crate (0.0.1-beta.3) is already a conditional dependency of russh-keys on Windows — no new Cargo.toml changes needed.

## Windows Code Path

```
1. Try connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
   ├── ok  → .dynamic(), proceed to identity loop
   └── err → try connect_pageant()
              ├── ok  → .dynamic(), proceed to identity loop
              └── err → Err("ssh-agent: no agent found (tried OpenSSH pipe and Pageant)")

2. Identity loop (identical to Unix after type unification):
   a. request_identities()
      └── empty → Err("ssh-agent has no identities loaded (try `ssh-add` or load keys into Pageant)")
   b. for identity in identities:
         authenticate_publickey_with(username, identity, &mut agent)
         ├── Ok(true)  → authenticated, break
         ├── Ok(false) → try next
         └── Err(e)    → record last_err, try next
   c. none accepted → Err("ssh-agent: no identity accepted (last error: …)")
```

The Unix path is untouched; its cfg annotation is updated from `#[cfg(not(target_os = "windows"))]` to `#[cfg(unix)]` for symmetry.

## Error Messages

| Situation | Message |
|---|---|
| Both agents unavailable | `ssh-agent: no agent found (tried OpenSSH pipe and Pageant)` |
| Agent connected, no identities | `ssh-agent has no identities loaded (try \`ssh-add\` or load keys into Pageant)` |
| Identities present, none accepted | `ssh-agent: no identity accepted (last error: …)` |

## README Update

The "Honest Caveats" section currently says ssh-agent on Windows is unsupported. That paragraph is removed once the feature ships. The connectivity feature list is updated to note Windows OpenSSH agent and Pageant are supported.

## Testing

No automated integration test is possible from a Linux build environment (named pipe and Pageant are Windows-only). The change is covered by:

- The existing mock-based agent tests in the codebase verify the auth loop logic is unchanged.
- Manual verification on Windows: connect with OpenSSH agent loaded (`ssh-add`), connect with Pageant loaded, verify both succeed; verify clear errors when neither agent is running.

## Files Changed

| File | Change |
|---|---|
| `src/transport/sftp.rs` | Replace Windows stub with `#[cfg(windows)]` named-pipe → Pageant fallback; update Unix cfg annotation |
| `README.md` | Remove ssh-agent Windows caveat; update feature list |

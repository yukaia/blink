# Windows ssh-agent Support Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the Windows ssh-agent stub in `src/transport/sftp.rs` with working support for the Windows OpenSSH named pipe and Pageant, matching the auth UX on Unix.

**Architecture:** Two `#[cfg]` blocks replace the current `#[cfg(target_os = "windows")]` stub and `#[cfg(not(target_os = "windows"))]` Unix block. The Windows block tries `\\.\pipe\openssh-ssh-agent` first; on failure falls back to Pageant. Both sides call `.dynamic()` to unify types before the shared identity loop. No new files, no new dependencies.

**Tech Stack:** Rust, russh-keys 0.49.2 (`AgentClient::connect_named_pipe`, `AgentClient::connect_pageant`, `.dynamic()`), tokio.

---

## File Map

| File | Change |
|---|---|
| `src/transport/sftp.rs` lines 232–282 | Replace the two cfg blocks with a `#[cfg(unix)]` block (unchanged logic) and a new `#[cfg(windows)]` block |
| `README.md` line 19–20 | Update ssh-agent bullet — remove "deferred" caveat |
| `README.md` lines 489–493 | Remove the "ssh-agent on Windows is not supported" caveat paragraph |

---

## Task 1: Replace the Windows stub in `src/transport/sftp.rs`

**Files:**
- Modify: `src/transport/sftp.rs:232-282`

This is the only code change. The full `AuthMethod::Agent` arm currently looks like:

```rust
AuthMethod::Agent => {
    #[cfg(target_os = "windows")]
    {
        return Err(BlinkError::auth(
            "ssh-agent auth is not supported on Windows yet",
        ));
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut agent =
            russh::keys::agent::client::AgentClient::connect_env()
                .await
                .map_err(|e| {
                    BlinkError::auth(format!("ssh-agent connect: {e}"))
                })?;

        let identities = agent.request_identities().await.map_err(|e| {
            BlinkError::auth(format!("ssh-agent request_identities: {e}"))
        })?;
        if identities.is_empty() {
            return Err(BlinkError::auth(
                "ssh-agent has no identities loaded (try `ssh-add`)",
            ));
        }

        let mut succeeded = false;
        let mut last_err: Option<String> = None;
        for identity in identities {
            let auth_result = handle
                .authenticate_publickey_with(username, identity, &mut agent)
                .await;
            match auth_result {
                Ok(true) => {
                    succeeded = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        if !succeeded {
            return Err(BlinkError::auth(format!(
                "ssh-agent: no identity accepted{}",
                last_err
                    .map(|e| format!(" (last error: {e})"))
                    .unwrap_or_default()
            )));
        }
        true
    }
}
```

- [ ] **Step 1: Replace the `AuthMethod::Agent` arm**

Replace lines 232–282 with the following. The Unix block is logically identical — only the cfg annotation changes from `#[cfg(not(target_os = "windows"))]` to `#[cfg(unix)]`. The Windows block is new.

```rust
            AuthMethod::Agent => {
                #[cfg(unix)]
                {
                    let mut agent =
                        russh::keys::agent::client::AgentClient::connect_env()
                            .await
                            .map_err(|e| {
                                BlinkError::auth(format!("ssh-agent connect: {e}"))
                            })?;

                    let identities = agent.request_identities().await.map_err(|e| {
                        BlinkError::auth(format!("ssh-agent request_identities: {e}"))
                    })?;
                    if identities.is_empty() {
                        return Err(BlinkError::auth(
                            "ssh-agent has no identities loaded (try `ssh-add`)",
                        ));
                    }

                    let mut succeeded = false;
                    let mut last_err: Option<String> = None;
                    for identity in identities {
                        let auth_result = handle
                            .authenticate_publickey_with(username, identity, &mut agent)
                            .await;
                        match auth_result {
                            Ok(true) => {
                                succeeded = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => last_err = Some(e.to_string()),
                        }
                    }
                    if !succeeded {
                        return Err(BlinkError::auth(format!(
                            "ssh-agent: no identity accepted{}",
                            last_err
                                .map(|e| format!(" (last error: {e})"))
                                .unwrap_or_default()
                        )));
                    }
                    true
                }
                #[cfg(windows)]
                {
                    use russh::keys::agent::client::AgentClient;

                    const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

                    let mut agent =
                        match AgentClient::connect_named_pipe(OPENSSH_PIPE).await {
                            Ok(a) => a.dynamic(),
                            Err(_) => AgentClient::connect_pageant().await.dynamic(),
                        };

                    let identities = agent.request_identities().await.map_err(|e| {
                        BlinkError::auth(format!(
                            "ssh-agent: no agent found (tried OpenSSH pipe and Pageant): {e}"
                        ))
                    })?;
                    if identities.is_empty() {
                        return Err(BlinkError::auth(
                            "ssh-agent has no identities loaded \
                             (try `ssh-add` or load keys into Pageant)",
                        ));
                    }

                    let mut succeeded = false;
                    let mut last_err: Option<String> = None;
                    for identity in identities {
                        let auth_result = handle
                            .authenticate_publickey_with(username, identity, &mut agent)
                            .await;
                        match auth_result {
                            Ok(true) => {
                                succeeded = true;
                                break;
                            }
                            Ok(false) => {}
                            Err(e) => last_err = Some(e.to_string()),
                        }
                    }
                    if !succeeded {
                        return Err(BlinkError::auth(format!(
                            "ssh-agent: no identity accepted{}",
                            last_err
                                .map(|e| format!(" (last error: {e})"))
                                .unwrap_or_default()
                        )));
                    }
                    true
                }
            }
```

- [ ] **Step 2: Verify the Linux build is clean**

```bash
cargo check 2>&1
```

Expected: no errors, no warnings about the changed code. The `#[cfg(windows)]` block is dead code on Linux and will not be compiled — that's correct.

- [ ] **Step 3: Cross-compile check for Windows (if target is installed)**

```bash
cargo check --target x86_64-pc-windows-gnu 2>&1
```

If the target is not installed you'll see `error[E0463]: can't find crate for 'std'` — install it with:

```bash
rustup target add x86_64-pc-windows-gnu
```

Expected once target is present: no errors.

- [ ] **Step 4: Commit**

```bash
git add src/transport/sftp.rs
git commit -m "feat: add Windows ssh-agent support (OpenSSH pipe + Pageant fallback)"
```

---

## Task 2: Update README.md

**Files:**
- Modify: `README.md` lines 19–20 (feature bullet)
- Modify: `README.md` lines 489–493 (Honest Caveats paragraph)

- [ ] **Step 1: Update the connectivity feature bullet (lines 19–20)**

Current text:
```
- **ssh-agent** auth on Unix (uses `$SSH_AUTH_SOCK`); Windows ssh-agent
  support is deferred — see [Honest Caveats](#honest-caveats)
```

Replace with:
```
- **ssh-agent** auth on Unix (uses `$SSH_AUTH_SOCK`); on Windows, the
  built-in OpenSSH agent (`\\.\pipe\openssh-ssh-agent`) is tried first,
  falling back to Pageant
```

- [ ] **Step 2: Remove the Windows ssh-agent caveat (lines 489–493)**

Current text to remove:
```
- **ssh-agent on Windows is not supported.** The Unix path uses
  `$SSH_AUTH_SOCK`; Windows would need separate plumbing for the OpenSSH
  named pipe (and/or Pageant), which the russh 0.49 entry point doesn't
  handle uniformly. Trying agent auth on Windows surfaces a clear error.
  Use SSH key auth instead — works the same on Windows.
```

Delete those five lines entirely. The surrounding caveats (TLS cert validation above, passwords-in-memory below) remain unchanged.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: update README for Windows ssh-agent support"
```

---

## Manual Verification Checklist (Windows only)

These cannot be automated from a Linux build environment. Run on a Windows 10/11 machine after building with `cargo build --release`:

- [ ] OpenSSH agent running + `ssh-add <key>` loaded → blink connects with `method = agent`
- [ ] OpenSSH agent running but empty → blink shows "ssh-agent has no identities loaded (try `ssh-add` or load keys into Pageant)"
- [ ] OpenSSH agent service stopped, Pageant running + key loaded → blink falls back and connects
- [ ] Both agents absent → blink shows "no agent found (tried OpenSSH pipe and Pageant)"

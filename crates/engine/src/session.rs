//! Per-connection session principal (Phase 7 — Row-Level Security).
//!
//! Identity is the boundary's job: the wire layer (or an embedder) verifies a
//! JWT and forwards the *already-trusted* claims as an opaque blob, plus an
//! active role, via the 6E `SET` path. The engine never verifies a token or
//! holds a secret — it only *reads* this principal so policy predicates can
//! consult `auth.uid()` / `auth.role()` / `auth.claim(k)` (spec 17 §Session
//! principal). The context is per-connection and per-transaction overridable;
//! two connections to the same shared `Database` never see each other's
//! principal (the context lives in each `Connection`, installed into a
//! thread-local only for the duration of that connection's own statement).

use crate::json::Json;

/// The role of a connection that has not run `SET ROLE` — the unauthenticated
/// principal, matching Supabase's `anon`.
pub const ANON_ROLE: &str = "anon";

/// A connection's authorization principal: the active role, the trusted claims
/// blob, and the explicit RLS-bypass flag.
#[derive(Clone, Debug, Default)]
pub struct SessionContext {
    /// Active role pinned by `SET ROLE`; `None` until set (treated as `anon`).
    role: Option<String>,
    /// Raw `twill.jwt.claims` JSON text — opaque, untrusted, never verified here.
    claims: Option<String>,
    /// Explicit, off-by-default RLS bypass (`SET twill.rls.bypass = on`). Never
    /// inferred from a role name — the privileged exemption must be turned on
    /// deliberately (spec 17 §bypass governance).
    bypass: bool,
}

impl SessionContext {
    /// The active role, defaulting to [`ANON_ROLE`] when unset.
    pub fn role(&self) -> &str {
        self.role.as_deref().unwrap_or(ANON_ROLE)
    }

    /// `SET ROLE <name>` / `RESET ROLE` (`None` returns to the default).
    pub fn set_role(&mut self, role: Option<String>) {
        self.role = role;
    }

    /// `SET twill.jwt.claims = '<json>'` / reset (`None`). Stored verbatim and
    /// never validated — the boundary already vouched for it.
    pub fn set_claims(&mut self, claims: Option<String>) {
        self.claims = claims;
    }

    /// `SET twill.rls.bypass = on|off` — the explicit privileged exemption.
    pub fn set_bypass(&mut self, on: bool) {
        self.bypass = on;
    }

    /// Whether this session is exempt from RLS enforcement.
    pub fn bypass(&self) -> bool {
        self.bypass
    }

    /// `auth.uid()` → the `sub` claim as text, or NULL when absent.
    pub fn uid(&self) -> Option<String> {
        self.claim("sub")
    }

    /// `auth.claim(k)` → the top-level claim `k` as text, or NULL when the claims
    /// blob is unset, unparsable, or lacks the key.
    pub fn claim(&self, key: &str) -> Option<String> {
        let raw = self.claims.as_ref()?;
        Json::parse(raw)?.get_key(key)?.as_text()
    }
}
